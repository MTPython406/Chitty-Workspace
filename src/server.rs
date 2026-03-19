//! Local axum HTTP server
//!
//! Serves the chat UI and provides API endpoints for
//! conversations, providers, model management, streaming chat,
//! agents management, and tool listing.
//!
//! The chat handler implements the full agent execution loop:
//! LLM call → detect tool calls → execute tools → feed results back → repeat

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

use crate::chat::ChatEngine;
use crate::config;
use crate::providers::adaptors::xai::XaiProvider;
use crate::providers::cloud::AnthropicProvider;
use crate::providers::{ChatMessage, Provider, ProviderId, StreamChunk, ToolCall};
use crate::agents::{Agent, AgentsManager};
use crate::storage::Database;
use crate::tools::{ToolContext, ToolRegistry, ToolRuntime};

// Embed the chat UI HTML at compile time
const CHAT_HTML: &str = include_str!("../assets/chat.html");

// ---------------------------------------------------------------------------
// Browser Bridge — connects the browser native tool to the frontend iframe
// ---------------------------------------------------------------------------

/// Command sent from BrowserTool to the frontend via WebSocket
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserCommand {
    pub id: String,
    pub action: String,
    pub params: serde_json::Value,
}

/// Response from the frontend back to BrowserTool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserResponse {
    pub id: String,
    pub success: bool,
    pub data: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Bridge between the browser native tool (server-side) and the frontend iframe (client-side).
/// Tool sends commands via `send_command()`, the WS handler relays them to the frontend,
/// and the response flows back through a oneshot channel.
pub struct BrowserBridge {
    cmd_tx: mpsc::Sender<(BrowserCommand, oneshot::Sender<BrowserResponse>)>,
    cmd_rx: Mutex<mpsc::Receiver<(BrowserCommand, oneshot::Sender<BrowserResponse>)>>,
    connected: AtomicBool,
}

impl BrowserBridge {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        Self {
            cmd_tx,
            cmd_rx: Mutex::new(cmd_rx),
            connected: AtomicBool::new(false),
        }
    }

    /// Send a command to the frontend and await the response.
    pub async fn send_command(&self, cmd: BrowserCommand, timeout: Duration) -> anyhow::Result<BrowserResponse> {
        if !self.is_connected() {
            anyhow::bail!("No browser frontend connected");
        }

        let (resp_tx, resp_rx) = oneshot::channel();
        self.cmd_tx.send((cmd, resp_tx)).await
            .map_err(|_| anyhow::anyhow!("Browser bridge channel closed"))?;

        tokio::time::timeout(timeout, resp_rx)
            .await
            .map_err(|_| anyhow::anyhow!("Browser command timed out"))?
            .map_err(|_| anyhow::anyhow!("Browser response channel dropped"))
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    fn set_connected(&self, val: bool) {
        self.connected.store(val, Ordering::Relaxed);
    }
}

/// Shared application state
pub struct AppState {
    pub db: Database,
    pub tool_registry: Arc<ToolRegistry>,
    pub tool_runtime: Arc<tokio::sync::RwLock<ToolRuntime>>,
    pub browser_bridge: Arc<BrowserBridge>,
}

/// Start the axum server on the given port.
pub async fn start(db: Database, tool_registry: Arc<ToolRegistry>, tool_runtime: Arc<tokio::sync::RwLock<ToolRuntime>>, browser_bridge: Arc<BrowserBridge>, port: u16) -> anyhow::Result<()> {
    // Seed marketplace packages from bundled assets
    {
        let rt = tool_runtime.read().await;
        let marketplace_dir = rt.tools_dir().join("marketplace");

        // Seed each bundled package
        let packages = ["google-cloud", "web-tools", "social-media"];
        for pkg_name in &packages {
            let pkg_dir = marketplace_dir.join(pkg_name);
            if !pkg_dir.exists() {
                let assets_dir = std::path::Path::new("assets/marketplace").join(pkg_name);
                if assets_dir.exists() {
                    tracing::info!("Seeding marketplace package: {}", pkg_name);
                    if let Err(e) = copy_dir_recursive(&assets_dir, &pkg_dir) {
                        tracing::warn!("Failed to seed {} package: {}", pkg_name, e);
                    }
                }
            }
        }
        drop(rt);
        // Re-scan to pick up marketplace tools
        tool_runtime.write().await.scan_and_load();
    }

    let state = Arc::new(AppState { db, tool_registry, tool_runtime, browser_bridge });

    let app = Router::new()
        // UI
        .route("/", get(index_handler))
        // Chat
        .route("/api/chat", post(chat_handler))
        // Conversations
        .route("/api/conversations", get(list_conversations))
        .route("/api/conversations", post(create_conversation))
        .route(
            "/api/conversations/:id",
            get(get_conversation).delete(delete_conversation_handler),
        )
        // Providers
        .route("/api/providers", get(list_providers))
        .route("/api/providers/:id/key", post(save_api_key_handler))
        .route(
            "/api/providers/:id/key",
            delete(delete_api_key_handler),
        )
        .route("/api/providers/:id/models", get(list_models_handler))
        // Model management
        .route("/api/providers/:id/discover", get(discover_models_handler))
        .route("/api/models", get(list_user_models))
        .route("/api/models", post(add_user_model))
        .route("/api/models/:id", delete(remove_user_model))
        .route("/api/models/:id/default", post(set_default_model))
        // Token tracking
        .route("/api/tokens/summary", get(token_summary))
        .route(
            "/api/tokens/conversation/:id",
            get(conversation_token_usage),
        )
        // Tools
        .route("/api/tools", get(list_tools))
        // Agents
        .route("/api/agents", get(list_agents))
        .route("/api/agents", post(create_agent))
        .route("/api/agents/:id", get(get_agent))
        .route("/api/agents/:id", put(update_agent))
        .route("/api/agents/:id", delete(delete_agent))
        // Agent Builder
        .route("/api/agent-builder/generate", post(agent_builder_handler))
        // Browser bridge WebSocket
        .route("/ws/browser", get(ws_browser_handler))
        // Marketplace
        .route("/api/marketplace/packages", get(list_marketplace_packages))
        .route("/api/marketplace/packages/:vendor/auth-status", get(check_package_auth))
        .route("/api/marketplace/packages/:vendor/auth", post(trigger_package_auth))
        .route("/api/marketplace/packages/:vendor/setup", post(run_package_setup))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port)).await?;
    tracing::info!("Server listening on http://127.0.0.1:{}", port);
    axum::serve(listener, app).await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// UI handler
// ---------------------------------------------------------------------------

async fn index_handler() -> Html<&'static str> {
    Html(CHAT_HTML)
}

// ---------------------------------------------------------------------------
// Browser bridge WebSocket handler
// ---------------------------------------------------------------------------

async fn ws_browser_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_browser_ws(socket, state))
}

async fn handle_browser_ws(mut socket: WebSocket, state: Arc<AppState>) {
    let bridge = &state.browser_bridge;
    bridge.set_connected(true);
    tracing::info!("Browser bridge WebSocket connected");

    // In-flight commands awaiting responses, keyed by command ID
    let mut pending: HashMap<String, oneshot::Sender<BrowserResponse>> = HashMap::new();
    let mut cmd_rx = bridge.cmd_rx.lock().await;

    loop {
        tokio::select! {
            // New command from a tool wanting to reach the frontend
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some((browser_cmd, resp_tx)) => {
                        let cmd_id = browser_cmd.id.clone();
                        match serde_json::to_string(&browser_cmd) {
                            Ok(json) => {
                                if socket.send(Message::Text(json.into())).await.is_err() {
                                    tracing::warn!("Browser WS send failed, disconnecting");
                                    let _ = resp_tx.send(BrowserResponse {
                                        id: cmd_id,
                                        success: false,
                                        data: serde_json::Value::Null,
                                        error: Some("Browser WebSocket disconnected".into()),
                                    });
                                    break;
                                }
                                pending.insert(cmd_id, resp_tx);
                            }
                            Err(e) => {
                                let _ = resp_tx.send(BrowserResponse {
                                    id: cmd_id,
                                    success: false,
                                    data: serde_json::Value::Null,
                                    error: Some(format!("Serialization error: {}", e)),
                                });
                            }
                        }
                    }
                    None => {
                        tracing::info!("Browser bridge channel closed");
                        break;
                    }
                }
            }
            // Response from the frontend
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<BrowserResponse>(&text) {
                            Ok(resp) => {
                                if let Some(tx) = pending.remove(&resp.id) {
                                    let _ = tx.send(resp);
                                } else {
                                    tracing::warn!("Browser response for unknown command: {}", resp.id);
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Invalid browser response JSON: {}", e);
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!("Browser bridge WebSocket closed");
                        break;
                    }
                    Some(Err(e)) => {
                        tracing::warn!("Browser WS error: {}", e);
                        break;
                    }
                    _ => {} // ping/pong/binary — ignore
                }
            }
        }
    }

    bridge.set_connected(false);
    // Drop any pending commands with an error
    for (id, tx) in pending {
        let _ = tx.send(BrowserResponse {
            id,
            success: false,
            data: serde_json::Value::Null,
            error: Some("Browser WebSocket disconnected".into()),
        });
    }
    tracing::info!("Browser bridge WebSocket handler exiting");
}

// ---------------------------------------------------------------------------
// Chat handler (streaming SSE) — Agent Execution Loop
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ChatRequest {
    conversation_id: Option<String>,
    message: String,
    provider: String,
    model: String,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    project_path: Option<String>,
}

#[derive(Serialize, Default)]
struct ChatEventData {
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    conversation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    success: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    iteration: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_iterations: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_usage: Option<TokenUsageResponse>,
}

#[derive(Serialize, Clone)]
struct TokenUsageResponse {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
}

async fn chat_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (sse_tx, sse_rx) = mpsc::channel::<StreamChunk>(64);

    // Spawn the chat processing task
    tokio::spawn(async move {
        if let Err(e) = process_chat(state, req, sse_tx.clone()).await {
            tracing::error!("process_chat error: {}", e);
            let _ = sse_tx
                .send(StreamChunk::Error(e.to_string()))
                .await;
        }
        // Always send Done so UI never freezes
        let _ = sse_tx.send(StreamChunk::Done).await;
    });

    let stream = ReceiverStream::new(sse_rx).map(|chunk| {
        let event = match chunk {
            StreamChunk::Text(text) => Event::default()
                .event("text")
                .data(
                    serde_json::to_string(&ChatEventData {
                        content: Some(text),
                        ..Default::default()
                    })
                    .unwrap_or_default(),
                ),
            StreamChunk::ToolCallStart { id, name } => Event::default()
                .event("tool_call_start")
                .data(
                    serde_json::to_string(&ChatEventData {
                        tool_call_id: Some(id),
                        tool_name: Some(name),
                        ..Default::default()
                    })
                    .unwrap_or_default(),
                ),
            StreamChunk::ToolCallDelta { id, arguments } => Event::default()
                .event("tool_call_delta")
                .data(
                    serde_json::to_string(&ChatEventData {
                        tool_call_id: Some(id),
                        arguments: Some(arguments),
                        ..Default::default()
                    })
                    .unwrap_or_default(),
                ),
            StreamChunk::ToolCallEnd { id } => Event::default()
                .event("tool_call_end")
                .data(
                    serde_json::to_string(&ChatEventData {
                        tool_call_id: Some(id),
                        ..Default::default()
                    })
                    .unwrap_or_default(),
                ),
            StreamChunk::ToolResult {
                id,
                name,
                content,
                success,
                duration_ms,
            } => Event::default()
                .event("tool_result")
                .data(
                    serde_json::to_string(&ChatEventData {
                        tool_call_id: Some(id),
                        tool_name: Some(name),
                        content: Some(content),
                        success: Some(success),
                        duration_ms: Some(duration_ms),
                        ..Default::default()
                    })
                    .unwrap_or_default(),
                ),
            StreamChunk::Thinking(msg) => Event::default()
                .event("thinking")
                .data(
                    serde_json::to_string(&ChatEventData {
                        content: Some(msg),
                        ..Default::default()
                    })
                    .unwrap_or_default(),
                ),
            StreamChunk::IterationStart {
                iteration,
                max_iterations,
            } => Event::default()
                .event("iteration")
                .data(
                    serde_json::to_string(&ChatEventData {
                        iteration: Some(iteration),
                        max_iterations: Some(max_iterations),
                        ..Default::default()
                    })
                    .unwrap_or_default(),
                ),
            StreamChunk::Meta {
                conversation_id,
                message_id,
            } => Event::default()
                .event("meta")
                .data(
                    serde_json::to_string(&ChatEventData {
                        conversation_id: Some(conversation_id),
                        message_id,
                        ..Default::default()
                    })
                    .unwrap_or_default(),
                ),
            StreamChunk::Done => Event::default().event("done").data("{}"),
            StreamChunk::Error(err) => Event::default().event("error").data(
                serde_json::to_string(&ChatEventData {
                    error: Some(err),
                    ..Default::default()
                })
                .unwrap_or_default(),
            ),
        };
        Ok(event)
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Create a provider instance from the provider ID string.
async fn create_provider(
    state: &Arc<AppState>,
    provider_str: &str,
) -> anyhow::Result<Box<dyn Provider>> {
    let api_key = config::get_api_key(provider_str)?
        .ok_or_else(|| anyhow::anyhow!("No API key configured for {}", provider_str))?;

    let db = state.db.clone();
    let prov_id = provider_str.to_string();
    let base_url: Option<String> = db
        .with_conn(move |conn| {
            let mut stmt =
                conn.prepare("SELECT base_url FROM provider_configs WHERE provider_id = ?1")?;
            let url: Option<String> = stmt
                .query_row(rusqlite::params![prov_id], |row| row.get(0))
                .ok();
            Ok(url)
        })
        .await?;

    match provider_str.parse::<ProviderId>()? {
        ProviderId::Anthropic => Ok(Box::new(AnthropicProvider::new(api_key, base_url))),
        ProviderId::Xai => Ok(Box::new(XaiProvider::new(api_key, base_url))),
        ProviderId::Openai => {
            Ok(Box::new(XaiProvider::new(
                api_key,
                base_url.or_else(|| Some("https://api.openai.com/v1".to_string())),
            )))
        }
        other => anyhow::bail!("Provider '{}' not yet implemented", other),
    }
}

/// Pending tool call being accumulated from stream deltas
struct PendingToolCall {
    id: String,
    name: String,
    arguments_json: String,
}

/// Core chat processing — Agent Execution Loop
///
/// Mirrors DataVisions streaming_executor.py (lines 4516-5304):
/// while iteration < max_iterations {
///     call LLM → detect tool calls → execute tools → feed results back
/// }
async fn process_chat(
    state: Arc<AppState>,
    req: ChatRequest,
    sse_tx: mpsc::Sender<StreamChunk>,
) -> anyhow::Result<()> {
    let provider_str = req.provider.clone();
    let model_str = req.model.clone();
    let user_message = req.message.clone();
    let agent_id = req.agent_id.clone();
    let project_path = req.project_path.clone();

    // 1. Get or create conversation + save user message + assemble context
    let db = state.db.clone();
    let conv_id = req.conversation_id.clone();
    let msg = user_message.clone();
    let prov = provider_str.clone();
    let mdl = model_str.clone();
    let sid = agent_id.clone();
    let pp = project_path.clone();

    // Pre-read all tool definitions from runtime (native + custom + connection)
    // This must happen outside with_conn because tool_runtime is behind an async RwLock
    let all_tool_defs = {
        let rt = state.tool_runtime.read().await;
        rt.list_definitions()
    };

    let (conversation_id, context, exec_config, effective_project_path) = db
        .with_conn(move |conn| {
            let cid = if let Some(id) = conv_id {
                id
            } else {
                let title = msg.chars().take(80).collect::<String>();
                let conv = ChatEngine::create_conversation(conn, &prov, &mdl, Some(&title))?;
                conv.id
            };

            // Save user message
            ChatEngine::save_message(conn, &cid, "user", &msg, None, None)?;

            // Assemble context (with tools + agent instructions from full runtime)
            let (ctx, exec_cfg, eff_pp) = ChatEngine::assemble_context(
                conn,
                &cid,
                sid.as_deref(),
                pp.as_deref(),
                &all_tool_defs,
            )?;

            Ok((cid, ctx, exec_cfg, eff_pp))
        })
        .await?;

    // Send Meta event with conversation_id
    let _ = sse_tx
        .send(StreamChunk::Meta {
            conversation_id: conversation_id.clone(),
            message_id: None,
        })
        .await;

    tracing::info!("Chat: conv={}, agent={:?}, tools={}", conversation_id, agent_id, context.tools.len());

    // 2. Create provider
    let provider = match create_provider(&state, &provider_str).await {
        Ok(p) => p,
        Err(e) => {
            let _ = sse_tx.send(StreamChunk::Error(e.to_string())).await;
            return Ok(());
        }
    };

    // 3. Build initial messages
    let mut current_messages = vec![ChatMessage {
        role: "system".to_string(),
        content: context.system_prompt.clone(),
        tool_calls: None,
        tool_call_id: None,
    }];
    current_messages.extend(context.messages);

    let current_tools = context.tools;
    let max_iterations = exec_config.max_iterations;

    // =========================================================================
    // Agent Execution Loop (mirrors DataVisions streaming_executor)
    // =========================================================================
    let mut iteration: u32 = 0;

    loop {
        iteration += 1;

        if iteration > max_iterations {
            // Exhausted iterations — force a final text-only call
            let _ = sse_tx
                .send(StreamChunk::Thinking(
                    "Max iterations reached, generating final response...".to_string(),
                ))
                .await;

            // Strip tools to force text-only response (DataVisions line 4539)
            stream_llm_response(
                provider.as_ref(),
                &model_str,
                &current_messages,
                None, // no tools
                &sse_tx,
            )
            .await?;

            // Save whatever text came back
            // (simplified — we break after this)
            break;
        }

        // Send iteration event (only visible in UI when iteration > 1)
        if iteration > 1 {
            let _ = sse_tx
                .send(StreamChunk::IterationStart {
                    iteration,
                    max_iterations,
                })
                .await;
        }

        // If last iteration, strip tools to force text-only
        let tools_for_call = if iteration == max_iterations {
            None
        } else if current_tools.is_empty() {
            None
        } else {
            Some(current_tools.as_slice())
        };

        // 4. Context budget check & compaction
        let prompt_chars: usize = current_messages.iter().map(|m| m.content.len()).sum();

        // Auto-compact if prompt exceeds threshold (model context ~128K tokens ≈ 512K chars,
        // but we want to stay well under. Compact at ~80K chars ≈ 20K tokens)
        let compact_threshold = 80_000;
        if prompt_chars > compact_threshold && current_messages.len() > 4 {
            tracing::info!("Context compaction triggered: {} chars > {} threshold", prompt_chars, compact_threshold);
            let _ = sse_tx
                .send(StreamChunk::Thinking("Context compaction: summarizing older tool results...".to_string()))
                .await;

            compact_context(&mut current_messages, compact_threshold);

            let new_chars: usize = current_messages.iter().map(|m| m.content.len()).sum();
            tracing::info!("Context compacted: {} -> {} chars ({} messages)", prompt_chars, new_chars, current_messages.len());
            let _ = sse_tx
                .send(StreamChunk::Thinking(format!(
                    "Compacted: {} -> {} chars",
                    prompt_chars, new_chars
                )))
                .await;
        }

        let prompt_chars: usize = current_messages.iter().map(|m| m.content.len()).sum();
        tracing::info!("Iteration {}/{}: calling LLM with {} messages, tools={}, prompt_chars={}", iteration, max_iterations, current_messages.len(), tools_for_call.map_or(0, |t| t.len()), prompt_chars);

        // Send prompt stats to UI activity log
        let _ = sse_tx
            .send(StreamChunk::Thinking(format!(
                "PROMPT[{}] {} messages, {} tools",
                prompt_chars,
                current_messages.len(),
                tools_for_call.map_or(0, |t| t.len()),
            )))
            .await;

        // Log message roles and sizes for debugging
        for (mi, msg) in current_messages.iter().enumerate() {
            let preview = if msg.content.len() > 120 {
                format!("{}...", &msg.content[..120])
            } else {
                msg.content.clone()
            };
            let tc_info = msg.tool_calls.as_ref().map_or(String::new(), |tcs| {
                format!(" tool_calls=[{}]", tcs.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", "))
            });
            let tcid_info = msg.tool_call_id.as_ref().map_or(String::new(), |id| format!(" tool_call_id={}", id));
            tracing::debug!("  msg[{}] role={} len={}{}{} preview={}", mi, msg.role, msg.content.len(), tc_info, tcid_info, preview);
        }

        let (full_text, tool_calls) = stream_and_collect(
            provider.as_ref(),
            &model_str,
            &current_messages,
            tools_for_call,
            &sse_tx,
        )
        .await?;

        // 5. No tool calls → save assistant message, done
        tracing::info!("LLM returned: {}chars text, {} tool calls", full_text.len(), tool_calls.len());

        // Log LLM response details
        let _ = sse_tx
            .send(StreamChunk::Thinking(format!(
                "LLM returned: {} chars text, {} tool calls",
                full_text.len(),
                tool_calls.len()
            )))
            .await;

        if !tool_calls.is_empty() {
            for tc in &tool_calls {
                let args_preview = {
                    let s = tc.arguments.to_string();
                    if s.len() > 200 { format!("{}...", &s[..200]) } else { s }
                };
                tracing::info!("  Tool call: {}({}) args={}", tc.name, tc.id, args_preview);
                let _ = sse_tx
                    .send(StreamChunk::Thinking(format!(
                        "Tool call: {} args={}",
                        tc.name, args_preview
                    )))
                    .await;
            }
        }

        if !full_text.is_empty() {
            let text_preview = if full_text.len() > 200 {
                format!("{}...", &full_text[..200])
            } else {
                full_text.clone()
            };
            tracing::info!("  LLM text preview: {}", text_preview);
        }

        // Handle empty response (LLM returned nothing — likely context too large or API issue)
        if full_text.is_empty() && tool_calls.is_empty() {
            tracing::warn!("LLM returned empty response at iteration {} (prompt_chars={})", iteration, prompt_chars);
            let _ = sse_tx.send(StreamChunk::Thinking(
                "WARNING: LLM returned empty response (0 text, 0 tool calls)".to_string()
            )).await;
            let _ = sse_tx.send(StreamChunk::Text(
                "I wasn't able to generate a response. This may be due to the conversation being too long. Please try starting a new session or simplifying your request.".to_string()
            )).await;
            let _ = sse_tx.send(StreamChunk::Done).await;
            break;
        }

        if tool_calls.is_empty() {
            let db = state.db.clone();
            let cid = conversation_id.clone();
            let resp = full_text.clone();
            let prov_id = provider_str.clone();
            let mdl_id = model_str.clone();

            let est_prompt: u32 = current_messages
                .iter()
                .map(|m| m.content.len() as u32)
                .sum::<u32>()
                / 4;
            let est_completion = (resp.len() as u32) / 4;

            let _ = db
                .with_conn(move |conn| {
                    let msg = ChatEngine::save_message(
                        conn, &cid, "assistant", &resp, None, None,
                    )?;

                    // Track token usage
                    let usage_id = uuid::Uuid::new_v4().to_string();
                    conn.execute(
                        "INSERT INTO token_usage (id, conversation_id, message_id, provider_id, model_id, prompt_tokens, completion_tokens, total_tokens) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        rusqlite::params![usage_id, msg.conversation_id, msg.id, prov_id, mdl_id, est_prompt, est_completion, est_prompt + est_completion],
                    )?;

                    Ok(())
                })
                .await;

            let _ = sse_tx.send(StreamChunk::Done).await;
            break;
        }

        // 6. Tool calls present → execute tools, feed results back
        // Save assistant message with tool_calls
        let tool_calls_json = serde_json::to_value(&tool_calls)?;
        let db = state.db.clone();
        let cid = conversation_id.clone();
        let resp = full_text.clone();
        let tc_json = tool_calls_json.clone();

        db.with_conn(move |conn| {
            ChatEngine::save_message(
                conn,
                &cid,
                "assistant",
                &resp,
                Some(&tc_json),
                None,
            )?;
            Ok(())
        })
        .await?;

        // Add assistant message with tool calls to current_messages
        current_messages.push(ChatMessage {
            role: "assistant".to_string(),
            content: full_text,
            tool_calls: Some(tool_calls.clone()),
            tool_call_id: None,
        });

        // Execute each tool call
        tracing::info!("Executing {} tool calls", tool_calls.len());
        for tc in &tool_calls {
            tracing::info!("  Tool: {} ({})", tc.name, tc.id);
            let _ = sse_tx
                .send(StreamChunk::Thinking(format!("Executing {}...", tc.name)))
                .await;

            // Determine working directory (agent project_path > request project_path > cwd)
            let working_dir = effective_project_path
                .as_ref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

            let ctx = ToolContext {
                working_dir,
                db: state.db.clone(),
                conversation_id: conversation_id.clone(),
            };

            // Dispatch via tool_runtime (native + custom + connection tools)
            let tool_runtime = state.tool_runtime.read().await;
            let (result, duration_ms) = tool_runtime
                .execute(&tc.name, &tc.arguments, &ctx)
                .await;
            drop(tool_runtime);

            let result_content = result.as_content_string();
            tracing::info!("  Tool {} completed: success={}, {}ms, result_len={}", tc.name, result.success, duration_ms, result_content.len());
            let result_preview = if result_content.len() > 200 {
                format!("{}...", &result_content[..200])
            } else {
                result_content.clone()
            };
            tracing::debug!("  Tool {} result: {}", tc.name, result_preview);

            // Send ToolResult SSE event
            let _ = sse_tx
                .send(StreamChunk::ToolResult {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    content: result_content.clone(),
                    success: result.success,
                    duration_ms,
                })
                .await;

            // Save tool result message to DB
            let db = state.db.clone();
            let cid = conversation_id.clone();
            let tc_id = tc.id.clone();
            let rc = result_content.clone();
            db.with_conn(move |conn| {
                ChatEngine::save_message(
                    conn,
                    &cid,
                    "tool",
                    &rc,
                    None,
                    Some(&tc_id),
                )?;
                Ok(())
            })
            .await?;

            // Add tool result to current_messages for next LLM call
            current_messages.push(ChatMessage {
                role: "tool".to_string(),
                content: result_content,
                tool_calls: None,
                tool_call_id: Some(tc.id.clone()),
            });
        }

        // Loop continues — next iteration will call LLM with tool results
    }

    // Always send Done to close the SSE stream cleanly
    let _ = sse_tx.send(StreamChunk::Done).await;

    Ok(())
}

/// Stream LLM response and collect full text + tool calls.
/// Forwards text chunks and tool call events to SSE in real-time.
/// Returns (content_text, tool_calls).
///
/// IMPORTANT: chat_stream sends chunks into a bounded channel. We MUST read
/// chunks concurrently with the stream — otherwise the channel fills up
/// (capacity=256) and chat_stream blocks, causing a deadlock/timeout.
/// Reasoning models stream hundreds of thinking tokens, so this matters.
async fn stream_and_collect(
    provider: &dyn Provider,
    model: &str,
    messages: &[ChatMessage],
    tools: Option<&[serde_json::Value]>,
    sse_tx: &mpsc::Sender<StreamChunk>,
) -> anyhow::Result<(String, Vec<ToolCall>)> {
    // Use a larger channel buffer to reduce backpressure on the provider
    let (ptx, mut prx) = mpsc::channel::<StreamChunk>(256);

    let mut full_text = String::new();
    let mut pending_calls: HashMap<String, PendingToolCall> = HashMap::new();

    // Run chat_stream and chunk processing concurrently.
    // chat_stream writes to ptx; we read from prx in parallel.
    // When chat_stream finishes, ptx is dropped, prx.recv() returns None.
    let idle_timeout = std::time::Duration::from_secs(120);
    let mut chunk_count: u64 = 0;

    let stream_result = {
        // Create a future that processes chunks with idle timeout
        let chunk_processor = async {
            loop {
                match tokio::time::timeout(idle_timeout, prx.recv()).await {
                    Ok(Some(chunk)) => {
                        chunk_count += 1;
                        match &chunk {
                            StreamChunk::Text(text) => {
                                full_text.push_str(text);
                                let _ = sse_tx.send(chunk).await;
                            }
                            StreamChunk::ToolCallStart { id, name } => {
                                pending_calls.insert(
                                    id.clone(),
                                    PendingToolCall {
                                        id: id.clone(),
                                        name: name.clone(),
                                        arguments_json: String::new(),
                                    },
                                );
                                let _ = sse_tx.send(chunk).await;
                            }
                            StreamChunk::ToolCallDelta { id, arguments } => {
                                if let Some(tc) = pending_calls.get_mut(id) {
                                    tc.arguments_json.push_str(arguments);
                                }
                                let _ = sse_tx.send(chunk).await;
                            }
                            StreamChunk::ToolCallEnd { .. } => {
                                let _ = sse_tx.send(chunk).await;
                            }
                            StreamChunk::Done => {
                                // Don't forward Done — the loop controller decides when to send Done
                            }
                            StreamChunk::Error(_) => {
                                let _ = sse_tx.send(chunk).await;
                                return Err(anyhow::anyhow!("Stream error"));
                            }
                            _ => {
                                let _ = sse_tx.send(chunk).await;
                            }
                        }
                    }
                    Ok(None) => {
                        // Channel closed — provider finished
                        tracing::debug!("Provider stream completed, processed {} chunks", chunk_count);
                        return Ok(());
                    }
                    Err(_) => {
                        // Idle timeout — no chunk received for 120s
                        if chunk_count == 0 {
                            tracing::error!("Provider stream: no chunks received in {}s", idle_timeout.as_secs());
                            let _ = sse_tx.send(StreamChunk::Error(
                                "LLM request timed out — no response received. The model may be overloaded.".to_string()
                            )).await;
                        } else {
                            tracing::error!("Provider stream: idle timeout after {} chunks ({}s with no new data)",
                                chunk_count, idle_timeout.as_secs());
                            let _ = sse_tx.send(StreamChunk::Error(
                                format!("LLM stream stalled after {} chunks — no data for {}s", chunk_count, idle_timeout.as_secs())
                            )).await;
                        }
                        return Err(anyhow::anyhow!("Idle timeout"));
                    }
                }
            }
        };

        // Run both concurrently — chat_stream writes, chunk_processor reads
        let stream_fut = provider.chat_stream(model, messages, tools, ptx);
        tokio::pin!(stream_fut);
        tokio::pin!(chunk_processor);

        // Use join to run both; when stream finishes, ptx drops, processor ends
        let (stream_res, _processor_res) = tokio::join!(stream_fut, chunk_processor);
        stream_res
    };

    if let Err(e) = stream_result {
        tracing::error!("Provider stream error: {}", e);
        // We may still have partial results, so don't return error
    }

    tracing::info!("stream_and_collect: {} text chars, {} tool calls, {} total chunks",
        full_text.len(), pending_calls.len(), chunk_count);

    // Convert pending calls to ToolCall vec
    let tool_calls: Vec<ToolCall> = pending_calls
        .into_values()
        .map(|pc| {
            let args: serde_json::Value =
                serde_json::from_str(&pc.arguments_json).unwrap_or(serde_json::json!({}));
            ToolCall {
                id: pc.id,
                name: pc.name,
                arguments: args,
            }
        })
        .collect();

    Ok((full_text, tool_calls))
}

/// Stream a text-only LLM response (no tools, for final forced response)
async fn stream_llm_response(
    provider: &dyn Provider,
    model: &str,
    messages: &[ChatMessage],
    tools: Option<&[serde_json::Value]>,
    sse_tx: &mpsc::Sender<StreamChunk>,
) -> anyhow::Result<()> {
    let (ptx, mut prx) = mpsc::channel::<StreamChunk>(64);

    provider.chat_stream(model, messages, tools, ptx).await?;

    while let Some(chunk) = prx.recv().await {
        match &chunk {
            StreamChunk::Done => {
                // Don't forward — caller handles Done
            }
            _ => {
                let _ = sse_tx.send(chunk).await;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Conversation handlers
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ConversationResponse {
    id: String,
    title: Option<String>,
    provider: String,
    model: String,
    created_at: String,
    updated_at: String,
}

async fn list_conversations(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<ConversationResponse>> {
    let db = state.db.clone();
    let convs = db
        .with_conn(|conn| ChatEngine::list_conversations(conn))
        .await
        .unwrap_or_default();

    Json(
        convs
            .into_iter()
            .map(|c| ConversationResponse {
                id: c.id,
                title: c.title,
                provider: c.provider,
                model: c.model,
                created_at: c.created_at,
                updated_at: c.updated_at,
            })
            .collect(),
    )
}

#[derive(Deserialize)]
struct CreateConversationRequest {
    provider: String,
    model: String,
    title: Option<String>,
}

async fn create_conversation(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateConversationRequest>,
) -> impl IntoResponse {
    let db = state.db.clone();
    match db
        .with_conn(move |conn| {
            ChatEngine::create_conversation(conn, &req.provider, &req.model, req.title.as_deref())
        })
        .await
    {
        Ok(conv) => Json(serde_json::json!({
            "id": conv.id,
            "title": conv.title,
            "provider": conv.provider,
            "model": conv.model,
        }))
        .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn get_conversation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.clone();
    let conv_id = id.clone();
    match db
        .with_conn(move |conn| {
            let messages = ChatEngine::get_messages(conn, &conv_id)?;
            let mut stmt = conn.prepare(
                "SELECT id, title, provider, model FROM conversations WHERE id = ?1",
            )?;
            let conv: Option<(String, Option<String>, String, String)> = stmt
                .query_row(rusqlite::params![id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .ok();

            Ok((conv, messages))
        })
        .await
    {
        Ok((Some(conv), messages)) => Json(serde_json::json!({
            "id": conv.0,
            "title": conv.1,
            "provider": conv.2,
            "model": conv.3,
            "messages": messages.iter().map(|m| serde_json::json!({
                "id": m.id,
                "role": m.role,
                "content": m.content,
                "tool_calls": m.tool_calls,
                "tool_call_id": m.tool_call_id,
                "created_at": m.created_at,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Ok((None, _)) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Conversation not found"})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn delete_conversation_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.clone();
    match db
        .with_conn(move |conn| ChatEngine::delete_conversation(conn, &id))
        .await
    {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Provider handlers
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ProviderResponse {
    provider_id: String,
    display_name: String,
    base_url: Option<String>,
    enabled: bool,
    has_key: bool,
    model_count: i32,
}

async fn list_providers(State(state): State<Arc<AppState>>) -> Json<Vec<ProviderResponse>> {
    let db = state.db.clone();
    let providers = db
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT pc.provider_id, pc.display_name, pc.base_url, pc.enabled,
                        (SELECT COUNT(*) FROM user_models um WHERE um.provider_id = pc.provider_id AND um.enabled = 1)
                 FROM provider_configs pc ORDER BY pc.provider_id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, i32>(4)?,
                ))
            })?;
            let mut result = Vec::new();
            for row in rows {
                result.push(row?);
            }
            Ok(result)
        })
        .await
        .unwrap_or_default();

    let mut response = Vec::new();
    for (pid, display_name, base_url, enabled, model_count) in providers {
        let has_key = config::get_api_key(&pid)
            .ok()
            .flatten()
            .is_some();
        response.push(ProviderResponse {
            provider_id: pid,
            display_name,
            base_url,
            enabled,
            has_key,
            model_count,
        });
    }

    Json(response)
}

#[derive(Deserialize)]
struct SaveKeyRequest {
    api_key: String,
}

async fn save_api_key_handler(
    State(state): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
    Json(req): Json<SaveKeyRequest>,
) -> impl IntoResponse {
    if let Err(e) = config::set_api_key(&provider_id, &req.api_key) {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response();
    }

    let db = state.db.clone();
    let pid = provider_id.clone();
    let _ = db
        .with_conn(move |conn| {
            conn.execute(
                "UPDATE provider_configs SET enabled = 1 WHERE provider_id = ?1",
                rusqlite::params![pid],
            )?;
            Ok(())
        })
        .await;

    Json(serde_json::json!({"ok": true})).into_response()
}

async fn delete_api_key_handler(Path(provider_id): Path<String>) -> impl IntoResponse {
    match config::delete_api_key(&provider_id) {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn list_models_handler(
    State(state): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.clone();
    let pid = provider_id.clone();
    let models = db
        .with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, model_id, display_name, context_window, supports_tools, supports_streaming, supports_vision, is_default
                 FROM user_models WHERE provider_id = ?1 AND enabled = 1 ORDER BY is_default DESC, display_name",
            )?;
            let rows = stmt.query_map(rusqlite::params![pid], |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, String>(0)?,
                    "model_id": row.get::<_, String>(1)?,
                    "display_name": row.get::<_, String>(2)?,
                    "context_window": row.get::<_, Option<i32>>(3)?,
                    "supports_tools": row.get::<_, bool>(4)?,
                    "supports_streaming": row.get::<_, bool>(5)?,
                    "supports_vision": row.get::<_, bool>(6)?,
                    "is_default": row.get::<_, bool>(7)?,
                }))
            })?;
            let mut result = Vec::new();
            for row in rows {
                result.push(row?);
            }
            Ok(result)
        })
        .await
        .unwrap_or_default();

    if models.is_empty() && provider_id == "anthropic" {
        let api_key = config::get_api_key(&provider_id).ok().flatten();
        if api_key.is_some() {
            let provider = AnthropicProvider::new(api_key.unwrap(), None);
            let defaults = provider.list_models().await.unwrap_or_default();
            return Json(serde_json::json!({"models": defaults})).into_response();
        }
    }

    Json(serde_json::json!({"models": models})).into_response()
}

// ---------------------------------------------------------------------------
// Model discovery + management
// ---------------------------------------------------------------------------

async fn discover_models_handler(
    State(state): State<Arc<AppState>>,
    Path(provider_id): Path<String>,
) -> impl IntoResponse {
    let has_key = config::get_api_key(&provider_id)
        .ok()
        .flatten()
        .is_some();

    if !has_key {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "No API key configured for this provider"})),
        )
            .into_response();
    }

    let provider = match create_provider(&state, &provider_id).await {
        Ok(p) => p,
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    if provider_id == "xai" {
        let api_key = config::get_api_key(&provider_id).unwrap().unwrap();
        let xai = XaiProvider::new(api_key, None);
        match xai.discover_models().await {
            Ok(models) => {
                return Json(serde_json::json!({"models": models})).into_response();
            }
            Err(e) => {
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                )
                    .into_response();
            }
        }
    }

    match provider.list_models().await {
        Ok(models) => Json(serde_json::json!({"models": models})).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn list_user_models(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let db = state.db.clone();
    let models = db
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, provider_id, model_id, display_name, context_window, supports_tools, supports_streaming, supports_vision, is_default
                 FROM user_models WHERE enabled = 1 ORDER BY provider_id, is_default DESC, display_name",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, String>(0)?,
                    "provider_id": row.get::<_, String>(1)?,
                    "model_id": row.get::<_, String>(2)?,
                    "display_name": row.get::<_, String>(3)?,
                    "context_window": row.get::<_, Option<i32>>(4)?,
                    "supports_tools": row.get::<_, bool>(5)?,
                    "supports_streaming": row.get::<_, bool>(6)?,
                    "supports_vision": row.get::<_, bool>(7)?,
                    "is_default": row.get::<_, bool>(8)?,
                }))
            })?;
            let mut result = Vec::new();
            for row in rows {
                result.push(row?);
            }
            Ok(result)
        })
        .await
        .unwrap_or_default();

    Json(serde_json::json!({"models": models}))
}

#[derive(Deserialize)]
struct AddModelRequest {
    provider_id: String,
    model_id: String,
    display_name: String,
    context_window: Option<u32>,
    supports_tools: Option<bool>,
    supports_streaming: Option<bool>,
    supports_vision: Option<bool>,
    is_default: Option<bool>,
}

async fn add_user_model(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AddModelRequest>,
) -> impl IntoResponse {
    let db = state.db.clone();
    match db
        .with_conn(move |conn| {
            let id = uuid::Uuid::new_v4().to_string();
            let is_default = req.is_default.unwrap_or(false);

            if is_default {
                conn.execute(
                    "UPDATE user_models SET is_default = 0 WHERE provider_id = ?1",
                    rusqlite::params![req.provider_id],
                )?;
            }

            conn.execute(
                "INSERT OR REPLACE INTO user_models (id, provider_id, model_id, display_name, context_window, supports_tools, supports_streaming, supports_vision, is_default)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    id,
                    req.provider_id,
                    req.model_id,
                    req.display_name,
                    req.context_window.map(|c| c as i32),
                    req.supports_tools.unwrap_or(false),
                    req.supports_streaming.unwrap_or(true),
                    req.supports_vision.unwrap_or(false),
                    is_default,
                ],
            )?;
            Ok(id)
        })
        .await
    {
        Ok(id) => Json(serde_json::json!({"ok": true, "id": id})).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn remove_user_model(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.clone();
    match db
        .with_conn(move |conn| {
            conn.execute(
                "DELETE FROM user_models WHERE id = ?1",
                rusqlite::params![id],
            )?;
            Ok(())
        })
        .await
    {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn set_default_model(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.clone();
    match db
        .with_conn(move |conn| {
            let provider_id: String = conn.query_row(
                "SELECT provider_id FROM user_models WHERE id = ?1",
                rusqlite::params![id],
                |row| row.get(0),
            )?;

            conn.execute(
                "UPDATE user_models SET is_default = 0 WHERE provider_id = ?1",
                rusqlite::params![provider_id],
            )?;

            conn.execute(
                "UPDATE user_models SET is_default = 1 WHERE id = ?1",
                rusqlite::params![id],
            )?;

            Ok(())
        })
        .await
    {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Token tracking handlers
// ---------------------------------------------------------------------------

async fn token_summary(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let db = state.db.clone();
    let summary = db
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT provider_id, model_id,
                        SUM(prompt_tokens) as total_prompt,
                        SUM(completion_tokens) as total_completion,
                        SUM(total_tokens) as total_all,
                        COUNT(*) as call_count
                 FROM token_usage
                 GROUP BY provider_id, model_id
                 ORDER BY total_all DESC",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(serde_json::json!({
                    "provider_id": row.get::<_, String>(0)?,
                    "model_id": row.get::<_, String>(1)?,
                    "prompt_tokens": row.get::<_, i64>(2)?,
                    "completion_tokens": row.get::<_, i64>(3)?,
                    "total_tokens": row.get::<_, i64>(4)?,
                    "call_count": row.get::<_, i64>(5)?,
                }))
            })?;
            let mut result = Vec::new();
            for row in rows {
                result.push(row?);
            }
            Ok(result)
        })
        .await
        .unwrap_or_default();

    Json(serde_json::json!({"summary": summary}))
}

async fn conversation_token_usage(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.clone();
    let usage = db
        .with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT provider_id, model_id,
                        SUM(prompt_tokens) as total_prompt,
                        SUM(completion_tokens) as total_completion,
                        SUM(total_tokens) as total_all,
                        COUNT(*) as call_count
                 FROM token_usage
                 WHERE conversation_id = ?1
                 GROUP BY provider_id, model_id",
            )?;
            let rows = stmt.query_map(rusqlite::params![id], |row| {
                Ok(serde_json::json!({
                    "provider_id": row.get::<_, String>(0)?,
                    "model_id": row.get::<_, String>(1)?,
                    "prompt_tokens": row.get::<_, i64>(2)?,
                    "completion_tokens": row.get::<_, i64>(3)?,
                    "total_tokens": row.get::<_, i64>(4)?,
                    "call_count": row.get::<_, i64>(5)?,
                }))
            })?;
            let mut result = Vec::new();
            for row in rows {
                result.push(row?);
            }
            Ok(result)
        })
        .await
        .unwrap_or_default();

    Json(serde_json::json!({"usage": usage}))
}

// ---------------------------------------------------------------------------
// Tools handler
// ---------------------------------------------------------------------------

/// List all registered tools (for agent builder UI)
async fn list_tools(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let defs = state.tool_runtime.read().await.list_definitions();
    let tools: Vec<serde_json::Value> = defs
        .into_iter()
        .map(|d| {
            serde_json::json!({
                "name": d.name,
                "display_name": d.display_name,
                "description": d.description,
                "category": d.category,
                "has_instructions": d.instructions.is_some(),
            })
        })
        .collect();

    Json(serde_json::json!({"tools": tools}))
}

// ---------------------------------------------------------------------------
// Agents handlers
// ---------------------------------------------------------------------------

async fn list_agents(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let db = state.db.clone();
    let agents = db
        .with_conn(|conn| AgentsManager::list(conn))
        .await
        .unwrap_or_default();

    Json(serde_json::json!({"agents": agents}))
}

async fn get_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.clone();
    match db.with_conn(move |conn| AgentsManager::load(conn, &id)).await {
        Ok(Some(agent)) => Json(serde_json::json!(agent)).into_response(),
        Ok(None) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Agent not found"})),
        ).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ).into_response(),
    }
}

#[derive(Deserialize)]
struct CreateAgentRequest {
    name: String,
    description: String,
    instructions: String,
    tools: Vec<String>,
    #[serde(default)]
    project_path: Option<String>,
    #[serde(default)]
    max_iterations: Option<u32>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    max_tokens: Option<u32>,
}

async fn create_agent(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateAgentRequest>,
) -> impl IntoResponse {
    let agent = Agent {
        id: uuid::Uuid::new_v4().to_string(),
        name: req.name,
        description: req.description,
        instructions: req.instructions,
        tools: req.tools,
        project_path: req.project_path,
        preferred_provider: None,
        preferred_model: None,
        tags: Vec::new(),
        version: "1.0".to_string(),
        ai_generated: false,
        max_iterations: req.max_iterations,
        temperature: req.temperature,
        max_tokens: req.max_tokens,
    };

    let db = state.db.clone();
    let agent_id = agent.id.clone();
    match db
        .with_conn(move |conn| AgentsManager::save(conn, &agent))
        .await
    {
        Ok(()) => Json(serde_json::json!({"ok": true, "id": agent_id})).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn update_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<CreateAgentRequest>,
) -> impl IntoResponse {
    let agent = Agent {
        id,
        name: req.name,
        description: req.description,
        instructions: req.instructions,
        tools: req.tools,
        project_path: req.project_path,
        preferred_provider: None,
        preferred_model: None,
        tags: Vec::new(),
        version: "1.0".to_string(),
        ai_generated: false,
        max_iterations: req.max_iterations,
        temperature: req.temperature,
        max_tokens: req.max_tokens,
    };

    let db = state.db.clone();
    match db
        .with_conn(move |conn| AgentsManager::save(conn, &agent))
        .await
    {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn delete_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.clone();
    match db
        .with_conn(move |conn| AgentsManager::delete(conn, &id))
        .await
    {
        Ok(_) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Context Compaction — keep conversation within context budget
// ---------------------------------------------------------------------------

/// Compact older messages to fit within the context budget.
/// Preserves: system prompt (first message), last N messages.
/// Compacts: older tool results and assistant messages by truncating/summarizing.
fn compact_context(messages: &mut Vec<ChatMessage>, target_chars: usize) {
    if messages.len() <= 4 {
        return; // Nothing to compact
    }

    let preserve_last = 5.min(messages.len() - 1); // Keep last 5 messages + system
    let compact_end = messages.len() - preserve_last;

    // Compact messages from index 1 (skip system) to compact_end
    for i in 1..compact_end {
        let msg = &mut messages[i];
        let content_len = msg.content.len();

        if msg.role == "tool" && content_len > 500 {
            // Truncate tool results to first 300 chars + summary
            let preview = if content_len > 300 {
                format!(
                    "{}\n\n[... compacted: {} chars total, showing first 300]",
                    &msg.content[..300],
                    content_len
                )
            } else {
                msg.content.clone()
            };
            msg.content = preview;
        } else if msg.role == "assistant" && content_len > 1000 {
            // Keep assistant messages but trim very long ones
            let preview = format!(
                "{}\n\n[... compacted: {} chars total]",
                &msg.content[..800],
                content_len
            );
            msg.content = preview;
        }
    }

    // If still over budget after truncation, drop oldest tool result pairs
    let mut current_chars: usize = messages.iter().map(|m| m.content.len()).sum();
    if current_chars > target_chars && messages.len() > 6 {
        // Remove oldest tool results (keep system + at least preserved messages)
        let mut i = 1;
        while current_chars > target_chars && i < messages.len() - preserve_last {
            if messages[i].role == "tool" {
                current_chars -= messages[i].content.len();
                messages[i].content = format!("[compacted — tool result removed to fit context]");
            }
            i += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Agent Builder — AI-powered agent generation with agent loop
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AgentBuilderRequest {
    description: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct AgentBuilderSuggestion {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    instructions: String,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    marketplace_tools: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    max_iterations: Option<u32>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    notes: String,
}

async fn agent_builder_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AgentBuilderRequest>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (sse_tx, sse_rx) = mpsc::channel::<StreamChunk>(64);

    tokio::spawn(async move {
        if let Err(e) = process_agent_builder(state, req, sse_tx.clone()).await {
            tracing::error!("agent_builder error: {}", e);
            let _ = sse_tx.send(StreamChunk::Error(e.to_string())).await;
        }
        let _ = sse_tx.send(StreamChunk::Done).await;
    });

    let stream = ReceiverStream::new(sse_rx).map(|chunk| {
        let event = match chunk {
            StreamChunk::Text(text) => Event::default()
                .event("text")
                .data(serde_json::to_string(&serde_json::json!({"content": text})).unwrap_or_default()),
            StreamChunk::ToolCallStart { id, name } => Event::default()
                .event("tool_call_start")
                .data(serde_json::to_string(&serde_json::json!({"tool_call_id": id, "tool_name": name})).unwrap_or_default()),
            StreamChunk::ToolResult { name, content, .. } => Event::default()
                .event("tool_result")
                .data(serde_json::to_string(&serde_json::json!({"tool_name": name, "content": content})).unwrap_or_default()),
            StreamChunk::Done => Event::default().event("done").data("{}"),
            StreamChunk::Error(err) => Event::default()
                .event("error")
                .data(serde_json::to_string(&serde_json::json!({"error": err})).unwrap_or_default()),
            _ => Event::default().event("info").data("{}"),
        };
        Ok(event)
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Resolve the default provider and model for system agent tasks.
async fn resolve_default_agent(
    state: &Arc<AppState>,
) -> anyhow::Result<(String, String, Box<dyn Provider>)> {
    let data_dir = crate::storage::default_data_dir();
    let cfg = crate::config::AppConfig::load(&data_dir).unwrap_or_default();

    // Try config defaults first
    let provider_id = if let Some(ref pid) = cfg.default_provider {
        pid.clone()
    } else {
        // Find first provider with an API key
        let providers = ["anthropic", "xai", "openai"];
        let mut found = None;
        for p in providers {
            if config::get_api_key(p).ok().flatten().is_some() {
                found = Some(p.to_string());
                break;
            }
        }
        found.ok_or_else(|| anyhow::anyhow!("No provider configured with an API key. Please add an API key in Settings."))?
    };

    // Resolve model
    let model_id = if let Some(ref mid) = cfg.default_model {
        mid.clone()
    } else {
        // Get default model from DB for this provider
        let db = state.db.clone();
        let pid = provider_id.clone();
        let model = db
            .with_conn(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT model_id FROM user_models WHERE provider_id = ?1 AND is_default = 1 AND enabled = 1 LIMIT 1",
                )?;
                let mid: Option<String> = stmt
                    .query_row(rusqlite::params![pid], |row| row.get(0))
                    .ok();
                Ok(mid)
            })
            .await?;

        model.unwrap_or_else(|| {
            // Sensible fallbacks
            match provider_id.as_str() {
                "anthropic" => "claude-sonnet-4-20250514".to_string(),
                "xai" => "grok-3-mini-fast".to_string(),
                "openai" => "gpt-4o".to_string(),
                _ => "default".to_string(),
            }
        })
    };

    let provider = create_provider(state, &provider_id).await?;
    Ok((provider_id, model_id, provider))
}

/// Build the two agent-builder tool definitions in OpenAI function-calling format.
fn agent_builder_tools() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "list_system_tools",
                "description": "List all tools currently available in the Chitty Workspace system (native + custom + connections). Returns name, display name, description, and category for each tool.",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "list_marketplace_tools",
                "description": "List tools available in the Chitty Workspace marketplace. These are additional tools that can be installed to extend capabilities.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "category": {
                            "type": "string",
                            "description": "Optional filter by category (native, integration, custom)"
                        }
                    },
                    "required": []
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "create_tool",
                "description": "Create a new custom tool. The tool is a script (Python, Node.js, Shell) that receives JSON on stdin and returns JSON on stdout. Use this when the agent needs a capability that doesn't exist yet.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Unique tool name (snake_case, e.g., 'pdf_generator')"
                        },
                        "display_name": {
                            "type": "string",
                            "description": "Human-readable name (e.g., 'PDF Generator')"
                        },
                        "description": {
                            "type": "string",
                            "description": "What the tool does"
                        },
                        "runtime": {
                            "type": "string",
                            "enum": ["python", "node", "powershell", "shell"],
                            "description": "Script runtime"
                        },
                        "script": {
                            "type": "string",
                            "description": "The script source code. Must read JSON from stdin, write JSON to stdout: {\"success\": true, \"output\": \"...\"}"
                        },
                        "parameters": {
                            "type": "object",
                            "description": "Tool parameters. Each key is param name, value is {\"type\": \"string\", \"description\": \"...\", \"required\": true/false}"
                        }
                    },
                    "required": ["name", "display_name", "description", "runtime", "script", "parameters"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "install_package",
                "description": "Install Python or Node.js packages for use by a custom tool. Packages are installed in an isolated directory per tool.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "runtime": {
                            "type": "string",
                            "enum": ["python", "node"],
                            "description": "Package manager"
                        },
                        "packages": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Package names to install"
                        },
                        "tool_name": {
                            "type": "string",
                            "description": "Name of the custom tool these packages are for"
                        }
                    },
                    "required": ["runtime", "packages", "tool_name"]
                }
            }
        }),
    ]
}

/// Execute an agent-builder tool call and return the result string.
/// For create_tool and install_package, delegates to the actual native tool implementations.
async fn execute_builder_tool(
    tool_name: &str,
    args: &serde_json::Value,
    state: &Arc<AppState>,
) -> String {
    match tool_name {
        "list_system_tools" => {
            let runtime = state.tool_runtime.read().await;
            let defs = runtime.list_definitions();
            let catalog: Vec<serde_json::Value> = defs
                .iter()
                .map(|d| {
                    serde_json::json!({
                        "name": d.name,
                        "display_name": d.display_name,
                        "description": d.description,
                        "category": d.category,
                    })
                })
                .collect();
            serde_json::to_string_pretty(&catalog).unwrap_or_default()
        }
        "list_marketplace_tools" => {
            let marketplace = serde_json::json!([
                {
                    "name": "web_scraper",
                    "display_name": "Web Scraper",
                    "description": "Advanced web scraping with CSS selectors, pagination, and JavaScript rendering. Extract structured data from any website.",
                    "status": "available",
                    "category": "integration"
                },
                {
                    "name": "email_sender",
                    "display_name": "Email Sender",
                    "description": "Send emails via SMTP or API services (SendGrid, Mailgun). Supports templates, attachments, and bulk sending.",
                    "status": "coming_soon",
                    "category": "integration"
                },
                {
                    "name": "social_media",
                    "display_name": "Social Media Manager",
                    "description": "Post, read, and manage content across social platforms (LinkedIn, Twitter/X, Facebook). Search profiles and send messages.",
                    "status": "coming_soon",
                    "category": "integration"
                },
                {
                    "name": "pdf_generator",
                    "display_name": "PDF Generator",
                    "description": "Create, merge, split, and manipulate PDF documents. Supports templates and data-driven generation.",
                    "status": "available",
                    "category": "native"
                },
                {
                    "name": "database_query",
                    "display_name": "Database Query",
                    "description": "Query SQL databases (PostgreSQL, MySQL, SQLite) and NoSQL stores (MongoDB). Read-only by default with optional write mode.",
                    "status": "available",
                    "category": "integration"
                },
                {
                    "name": "api_connector",
                    "display_name": "API Connector",
                    "description": "Make HTTP requests to REST and GraphQL APIs. Supports authentication, headers, and response parsing.",
                    "status": "available",
                    "category": "integration"
                },
                {
                    "name": "image_analyzer",
                    "display_name": "Image Analyzer",
                    "description": "Analyze images using vision models. Extract text (OCR), describe content, detect objects, and compare images.",
                    "status": "coming_soon",
                    "category": "native"
                },
                {
                    "name": "spreadsheet",
                    "display_name": "Spreadsheet Manager",
                    "description": "Read, write, and manipulate Excel (.xlsx) and CSV files. Supports formulas, formatting, and data analysis.",
                    "status": "coming_soon",
                    "category": "native"
                }
            ]);
            serde_json::to_string_pretty(&marketplace).unwrap_or_default()
        }
        "create_tool" => {
            // Delegate to the actual native create_tool implementation
            let ctx = ToolContext {
                working_dir: std::path::PathBuf::from("."),
                db: state.db.clone(),
                conversation_id: String::new(),
            };
            let (result, _) = state.tool_runtime.read().await.execute("create_tool", args, &ctx).await;

            // After creating a tool, reload the runtime so it's immediately available
            if result.success {
                let mut runtime = state.tool_runtime.write().await;
                runtime.scan_and_load();
                tracing::info!("Agent Builder: tool created and runtime reloaded");
            }

            result.as_content_string()
        }
        "install_package" => {
            // Delegate to the actual native install_package implementation
            let ctx = ToolContext {
                working_dir: std::path::PathBuf::from("."),
                db: state.db.clone(),
                conversation_id: String::new(),
            };
            let (result, _) = state.tool_runtime.read().await.execute("install_package", args, &ctx).await;
            result.as_content_string()
        }
        _ => format!("Unknown tool: {}", tool_name),
    }
}

/// Build the system prompt for the agent builder agent.
fn build_agent_builder_prompt() -> String {
    r#"You are the Agent Builder agent for Chitty Workspace, a local-first AI assistant.

Your job is to take a user's free-text description of what they want an AI agent to do, and design a complete agent definition. You can also BUILD custom tools when the agent needs capabilities that don't exist yet.

## Your Process

1. FIRST, call `list_system_tools` to see what tools are currently available in the system.
2. THEN, call `list_marketplace_tools` to see what additional tools could be installed.
3. If the agent needs a capability that doesn't exist, use `install_package` and `create_tool` to BUILD it.
4. Based on the available tools (including any you just created), design a complete agent.

## Creating Custom Tools

When the user needs a tool that doesn't exist (e.g., PDF generator, chart builder, data converter):

1. Call `install_package` first if the tool needs external libraries (e.g., markdown2, matplotlib, openpyxl)
2. Call `create_tool` with a working script that follows this pattern:

**Python tool template:**
```python
import json, sys
args = json.load(sys.stdin)
# Do work with args...
result = {"success": True, "output": "result here"}
print(json.dumps(result))
```

The script MUST: read JSON from stdin, write JSON to stdout with {"success": bool, "output": ...}.
The tool will be saved to disk and available immediately for the agent and all future sessions.

## What is an Agent?

An agent in Chitty Workspace consists of:
- **name**: Short, clear name (2-5 words)
- **description**: One-sentence summary
- **instructions**: A detailed system prompt for the AI agent. This should describe the agent's role, approach, constraints, and quality standards. Do NOT include tool usage documentation — that is injected automatically.
- **tools**: Array of system tool names this agent needs (from `list_system_tools` results)
- **marketplace_tools**: Array of marketplace tool names that would enhance this agent (from `list_marketplace_tools` results)
- **tags**: Categorization tags (e.g., "coding", "writing", "analysis", "automation")
- **max_iterations**: Tool call rounds allowed (5 for simple Q&A, 10 for standard tasks, 20-25 for complex multi-step tasks)
- **temperature**: null for default, 0.0-0.3 for precise/coding, 0.7-1.0 for creative
- **max_tokens**: null for default
- **notes**: Your observations — what works well with current tools, what marketplace tools would add, any limitations or future recommendations

## Output Format

After calling both tools, respond with ONLY a JSON object (no markdown, no code fences, no explanation):

{
  "name": "...",
  "description": "...",
  "instructions": "...",
  "tools": ["tool_name_1", "tool_name_2"],
  "marketplace_tools": ["marketplace_tool_1"],
  "tags": ["tag1", "tag2"],
  "max_iterations": 10,
  "temperature": null,
  "max_tokens": null,
  "notes": "..."
}

## Guidelines

1. Write the instructions field as if briefing a capable AI assistant. Be specific about persona, approach, and quality standards.
2. Only include tools the agent actually needs. If purely conversational, use an empty tools array.
3. If the user's request needs capabilities beyond current system tools, recommend marketplace tools and explain in notes.
4. Be honest about limitations — if something isn't possible yet, say so in notes and suggest the best available approach.
5. The instructions should be thorough but focused. Include specific guidance on how to handle edge cases relevant to the agent's domain."#.to_string()
}

/// Parse an agent suggestion from LLM text output.
fn parse_agent_suggestion(content: &str) -> anyhow::Result<AgentBuilderSuggestion> {
    // Try direct parse
    if let Ok(s) = serde_json::from_str::<AgentBuilderSuggestion>(content.trim()) {
        return Ok(s);
    }
    // Try extracting from ```json code fences
    if let Some(start) = content.find("```json") {
        let json_start = start + 7;
        if let Some(end) = content[json_start..].find("```") {
            let json_str = content[json_start..json_start + end].trim();
            if let Ok(s) = serde_json::from_str(json_str) {
                return Ok(s);
            }
        }
    }
    // Try extracting from plain ``` code fences
    if let Some(start) = content.find("```") {
        let json_start = start + 3;
        // Skip optional language tag on same line
        let json_start = content[json_start..]
            .find('\n')
            .map(|n| json_start + n + 1)
            .unwrap_or(json_start);
        if let Some(end) = content[json_start..].find("```") {
            let json_str = content[json_start..json_start + end].trim();
            if let Ok(s) = serde_json::from_str(json_str) {
                return Ok(s);
            }
        }
    }
    // Try finding first { to last }
    if let Some(start) = content.find('{') {
        if let Some(end) = content.rfind('}') {
            let json_str = &content[start..=end];
            if let Ok(s) = serde_json::from_str::<AgentBuilderSuggestion>(json_str) {
                return Ok(s);
            }
            // Try as generic Value and manually map fields
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) {
                let get_str = |keys: &[&str]| -> String {
                    for k in keys {
                        if let Some(s) = v.get(k).and_then(|v| v.as_str()) {
                            return s.to_string();
                        }
                    }
                    String::new()
                };
                let get_vec = |keys: &[&str]| -> Vec<String> {
                    for k in keys {
                        if let Some(arr) = v.get(k).and_then(|v| v.as_array()) {
                            return arr.iter().filter_map(|v| v.as_str().map(String::from)).collect();
                        }
                    }
                    Vec::new()
                };
                let name = get_str(&["name", "agent_name", "title"]);
                if !name.is_empty() {
                    return Ok(AgentBuilderSuggestion {
                        name,
                        description: get_str(&["description", "desc", "summary"]),
                        instructions: get_str(&["instructions", "system_prompt", "prompt", "system_message"]),
                        tools: get_vec(&["tools", "tool_list", "enabled_tools", "system_tools"]),
                        marketplace_tools: get_vec(&["marketplace_tools", "marketplace", "external_tools"]),
                        tags: get_vec(&["tags", "categories"]),
                        max_iterations: v.get("max_iterations").and_then(|v| v.as_u64()).map(|v| v as u32),
                        temperature: v.get("temperature").and_then(|v| v.as_f64()),
                        max_tokens: v.get("max_tokens").and_then(|v| v.as_u64()).map(|v| v as u32),
                        notes: get_str(&["notes", "observations", "comments", "limitations"]),
                    });
                }
            }
        }
    }
    anyhow::bail!("Could not extract valid JSON agent definition from AI response")
}

/// Core agent builder processing — agent loop with tool calls.
async fn process_agent_builder(
    state: Arc<AppState>,
    req: AgentBuilderRequest,
    sse_tx: mpsc::Sender<StreamChunk>,
) -> anyhow::Result<()> {
    // 1. Resolve default agent
    let (_provider_id, model_id, provider) = resolve_default_agent(&state).await?;

    let system_prompt = build_agent_builder_prompt();
    let builder_tools = agent_builder_tools();

    // 2. Build messages
    let mut current_messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: system_prompt,
            tool_calls: None,
            tool_call_id: None,
        },
        ChatMessage {
            role: "user".to_string(),
            content: req.description,
            tool_calls: None,
            tool_call_id: None,
        },
    ];

    let max_iterations: u32 = 10; // allow enough iterations for tool creation + agent definition

    // 3. Agent loop
    for iteration in 1..=max_iterations {
        let tools_for_call = if iteration == max_iterations {
            None // force text-only on last iteration
        } else {
            Some(builder_tools.as_slice())
        };

        // Stream from LLM — use interceptor to also capture reasoning/thinking text
        let (intercept_tx, mut intercept_rx) = mpsc::channel::<StreamChunk>(64);
        let sse_tx_clone = sse_tx.clone();
        let mut reasoning_text = String::new();

        // Spawn a task that forwards chunks to sse_tx while also capturing thinking text
        let interceptor = tokio::spawn(async move {
            let mut thinking = String::new();
            while let Some(chunk) = intercept_rx.recv().await {
                if let StreamChunk::Thinking(ref t) = chunk {
                    thinking.push_str(t);
                }
                let _ = sse_tx_clone.send(chunk).await;
            }
            thinking
        });

        let (full_text, tool_calls) = stream_and_collect(
            provider.as_ref(),
            &model_id,
            &current_messages,
            tools_for_call,
            &intercept_tx,
        )
        .await?;
        drop(intercept_tx); // close the channel so interceptor finishes
        reasoning_text = interceptor.await.unwrap_or_default();

        // No tool calls → final response
        if tool_calls.is_empty() {
            // If content is empty but we have reasoning text, use that instead
            // (reasoning models like Grok may put the JSON in reasoning_content)
            let response_text = if full_text.trim().is_empty() && !reasoning_text.trim().is_empty() {
                tracing::info!("Agent builder: content empty, using reasoning_text ({} chars)", reasoning_text.len());
                &reasoning_text
            } else {
                &full_text
            };
            tracing::info!("Agent builder final response ({} chars): {}", response_text.len(), &response_text[..response_text.len().min(500)]);
            if response_text.len() > 500 {
                tracing::info!("... (truncated, full length: {})", response_text.len());
            }
            // Try to parse as agent suggestion
            match parse_agent_suggestion(response_text) {
                Ok(suggestion) => {
                    let _ = sse_tx
                        .send(StreamChunk::Text(
                            serde_json::to_string(&serde_json::json!({
                                "type": "result",
                                "suggestion": suggestion
                            }))
                            .unwrap_or_default(),
                        ))
                        .await;
                }
                Err(e) => {
                    tracing::warn!("Failed to parse agent suggestion: {}", e);
                    tracing::warn!("Raw content text ({} chars):\n{}", full_text.len(), full_text);
                    tracing::warn!("Raw reasoning text ({} chars):\n{}", reasoning_text.len(), reasoning_text);
                    let _ = sse_tx
                        .send(StreamChunk::Error(format!(
                            "Could not parse agent definition from AI response. Please try again."
                        )))
                        .await;
                }
            }
            break;
        }

        // Tool calls — execute and feed back
        current_messages.push(ChatMessage {
            role: "assistant".to_string(),
            content: full_text,
            tool_calls: Some(tool_calls.clone()),
            tool_call_id: None,
        });

        for tc in &tool_calls {
            let start = std::time::Instant::now();
            let result = execute_builder_tool(&tc.name, &tc.arguments, &state).await;
            let duration_ms = start.elapsed().as_millis() as u64;

            let _ = sse_tx
                .send(StreamChunk::ToolResult {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    content: result.clone(),
                    success: true,
                    duration_ms,
                })
                .await;

            current_messages.push(ChatMessage {
                role: "tool".to_string(),
                content: result,
                tool_calls: None,
                tool_call_id: Some(tc.id.clone()),
            });
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Marketplace handlers
// ---------------------------------------------------------------------------

async fn list_marketplace_packages(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let rt = state.tool_runtime.read().await;
    let packages: Vec<serde_json::Value> = rt.list_marketplace_packages()
        .into_iter()
        .map(|pkg| {
            let tools: Vec<serde_json::Value> = pkg.manifest.tools.iter().map(|tool_name| {
                // Look up the tool definition to get its display_name and description
                let tool_dir = pkg.dir.join(tool_name);
                let manifest_path = tool_dir.join("manifest.json");
                if let Ok(content) = std::fs::read_to_string(&manifest_path) {
                    if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&content) {
                        return serde_json::json!({
                            "name": manifest.get("name").and_then(|n| n.as_str()).unwrap_or(tool_name),
                            "display_name": manifest.get("display_name").and_then(|n| n.as_str()).unwrap_or(tool_name),
                            "description": manifest.get("description").and_then(|n| n.as_str()).unwrap_or(""),
                            "installed": true,
                        });
                    }
                }
                serde_json::json!({
                    "name": tool_name,
                    "display_name": tool_name,
                    "description": "",
                    "installed": true,
                })
            }).collect();

            let setup_steps: Vec<serde_json::Value> = pkg.manifest.setup_steps.iter().map(|s| {
                serde_json::json!({
                    "id": s.id,
                    "label": s.label,
                    "help_text": s.help_text,
                    "required": s.required,
                    "prompt_user": s.prompt_user,
                })
            }).collect();

            serde_json::json!({
                "name": pkg.manifest.name,
                "display_name": pkg.manifest.display_name,
                "vendor": pkg.manifest.vendor,
                "description": pkg.manifest.description,
                "version": pkg.manifest.version,
                "icon": pkg.manifest.icon,
                "color": pkg.manifest.color,
                "status": pkg.manifest.status,
                "setup_steps": setup_steps,
                "tools": tools,
            })
        })
        .collect();

    Json(serde_json::json!({ "packages": packages }))
}

async fn check_package_auth(
    Path(vendor): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let rt = state.tool_runtime.read().await;
    let pkg = rt.list_marketplace_packages()
        .into_iter()
        .find(|p| p.manifest.name == vendor)
        .cloned();
    drop(rt);

    match pkg {
        Some(pkg) => {
            // Check all setup steps — if all check_commands pass, package is ready
            let mut all_ok = true;
            let mut step_results = Vec::new();

            for step in &pkg.manifest.setup_steps {
                if let Some(check_cmd) = &step.check_command {
                    let result = run_shell_command(check_cmd).await;
                    step_results.push(serde_json::json!({
                        "step_id": step.id,
                        "label": step.label,
                        "ok": result.success,
                    }));
                    if !result.success && step.required {
                        all_ok = false;
                    }
                }
            }

            Json(serde_json::json!({
                "authenticated": all_ok,
                "steps": step_results,
            }))
        }
        None => {
            Json(serde_json::json!({
                "authenticated": false,
                "error": format!("Package '{}' not found", vendor)
            }))
        }
    }
}

async fn trigger_package_auth(
    Path(vendor): Path<String>,
    State(_state): State<Arc<AppState>>,
) -> impl IntoResponse {
    // Legacy endpoint — use /setup for the full install wizard
    Json(serde_json::json!({
        "success": false,
        "message": format!("Use POST /api/marketplace/packages/{}/setup instead", vendor),
    }))
}

async fn run_package_setup(
    Path(vendor): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let rt = state.tool_runtime.read().await;
    let pkg = rt.list_marketplace_packages()
        .into_iter()
        .find(|p| p.manifest.name == vendor)
        .cloned();
    drop(rt);

    let pkg = match pkg {
        Some(p) => p,
        None => return Json(serde_json::json!({
            "success": false,
            "error": format!("Package '{}' not found", vendor)
        })),
    };

    // User-provided values for prompt steps (e.g. project_id)
    let user_values = body.get("values").and_then(|v| v.as_object()).cloned()
        .unwrap_or_default();

    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut all_success = true;

    for step in &pkg.manifest.setup_steps {
        let step_id = &step.id;
        let label = &step.label;

        // 1. If there's a check_command, run it to see if already done
        if let Some(check_cmd) = &step.check_command {
            let check_result = run_shell_command(check_cmd).await;
            if check_result.success {
                results.push(serde_json::json!({
                    "step_id": step_id,
                    "label": label,
                    "status": "already_done",
                    "message": "Already configured",
                }));
                continue;
            }
        }

        // 2. Determine the install command
        let install_cmd = if let Some(template) = &step.install_command_template {
            // User must provide a value for this step
            if let Some(val) = user_values.get(step_id).and_then(|v| v.as_str()) {
                if val.is_empty() {
                    results.push(serde_json::json!({
                        "step_id": step_id,
                        "label": label,
                        "status": "needs_input",
                        "prompt_label": step.prompt_label,
                        "prompt_placeholder": step.prompt_placeholder,
                        "prompt_help": step.prompt_help,
                        "message": "User input required",
                    }));
                    all_success = false;
                    continue;
                }
                Some(template.replace("{value}", val))
            } else if step.prompt_user {
                results.push(serde_json::json!({
                    "step_id": step_id,
                    "label": label,
                    "status": "needs_input",
                    "prompt_label": step.prompt_label,
                    "prompt_placeholder": step.prompt_placeholder,
                    "prompt_help": step.prompt_help,
                    "message": "User input required",
                }));
                all_success = false;
                continue;
            } else {
                None
            }
        } else {
            // Pick platform-specific command or generic
            let cmd = if cfg!(target_os = "windows") {
                step.install_command_windows.as_deref()
                    .or(step.install_command.as_deref())
            } else if cfg!(target_os = "macos") {
                step.install_command_mac.as_deref()
                    .or(step.install_command.as_deref())
            } else {
                step.install_command_linux.as_deref()
                    .or(step.install_command.as_deref())
            };
            cmd.map(|s| s.to_string())
        };

        // 3. Run the install command
        match install_cmd {
            Some(cmd) => {
                tracing::info!("Package setup [{}]: running '{}'", step_id, cmd);
                let result = run_shell_command(&cmd).await;
                if result.success {
                    results.push(serde_json::json!({
                        "step_id": step_id,
                        "label": label,
                        "status": "completed",
                        "message": "Done",
                        "output": result.stdout.chars().take(300).collect::<String>(),
                    }));
                } else {
                    results.push(serde_json::json!({
                        "step_id": step_id,
                        "label": label,
                        "status": "failed",
                        "message": result.stderr.chars().take(300).collect::<String>(),
                        "help_text": step.help_text,
                        "help_url": step.help_url,
                    }));
                    if step.required {
                        all_success = false;
                        break; // Stop on required step failure
                    }
                }
            }
            None => {
                results.push(serde_json::json!({
                    "step_id": step_id,
                    "label": label,
                    "status": "skipped",
                    "message": "No install command available",
                }));
            }
        }
    }

    Json(serde_json::json!({
        "success": all_success,
        "steps": results,
    }))
}

struct CommandResult {
    success: bool,
    stdout: String,
    stderr: String,
}

async fn run_shell_command(cmd: &str) -> CommandResult {
    let shell = if cfg!(target_os = "windows") { "cmd" } else { "sh" };
    let flag = if cfg!(target_os = "windows") { "/C" } else { "-c" };

    match tokio::process::Command::new(shell)
        .arg(flag)
        .arg(cmd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
    {
        Ok(output) => CommandResult {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        },
        Err(e) => CommandResult {
            success: false,
            stdout: String::new(),
            stderr: format!("Failed to execute: {}", e),
        },
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
