//! Local axum HTTP server
//!
//! Serves the chat UI and provides API endpoints for
//! conversations, providers, model management, streaming chat,
//! skills management, and tool listing.
//!
//! The chat handler implements the full agent execution loop:
//! LLM call → detect tool calls → execute tools → feed results back → repeat

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

use crate::chat::ChatEngine;
use crate::config;
use crate::providers::adaptors::xai::XaiProvider;
use crate::providers::cloud::AnthropicProvider;
use crate::providers::{ChatMessage, Provider, ProviderId, StreamChunk, ToolCall};
use crate::skills::{Skill, SkillsManager};
use crate::storage::Database;
use crate::tools::{ToolContext, ToolRegistry};

// Embed the chat UI HTML at compile time
const CHAT_HTML: &str = include_str!("../assets/chat.html");

/// Shared application state
pub struct AppState {
    pub db: Database,
    pub tool_registry: Arc<ToolRegistry>,
}

/// Start the axum server on the given port.
pub async fn start(db: Database, tool_registry: Arc<ToolRegistry>, port: u16) -> anyhow::Result<()> {
    let state = Arc::new(AppState { db, tool_registry });

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
        // Skills
        .route("/api/skills", get(list_skills))
        .route("/api/skills", post(create_skill))
        .route("/api/skills/:id", put(update_skill))
        .route("/api/skills/:id", delete(delete_skill))
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
// Chat handler (streaming SSE) — Agent Execution Loop
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ChatRequest {
    conversation_id: Option<String>,
    message: String,
    provider: String,
    model: String,
    #[serde(default)]
    skill_id: Option<String>,
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
    let skill_id = req.skill_id.clone();
    let project_path = req.project_path.clone();

    // 1. Get or create conversation + save user message + assemble context
    let db = state.db.clone();
    let tool_registry = state.tool_registry.clone();
    let conv_id = req.conversation_id.clone();
    let msg = user_message.clone();
    let prov = provider_str.clone();
    let mdl = model_str.clone();
    let sid = skill_id.clone();
    let pp = project_path.clone();

    let (conversation_id, context, exec_config) = db
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

            // Assemble context (with tools + agent instructions)
            let (ctx, exec_cfg) = ChatEngine::assemble_context(
                conn,
                &cid,
                sid.as_deref(),
                pp.as_deref(),
                &tool_registry,
            )?;

            Ok((cid, ctx, exec_cfg))
        })
        .await?;

    // Send Meta event with conversation_id
    let _ = sse_tx
        .send(StreamChunk::Meta {
            conversation_id: conversation_id.clone(),
            message_id: None,
        })
        .await;

    tracing::info!("Chat: conv={}, skill={:?}, tools={}", conversation_id, skill_id, context.tools.len());

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

        // 4. Stream from LLM, collect text + tool calls
        tracing::info!("Iteration {}/{}: calling LLM with {} messages, tools={}", iteration, max_iterations, current_messages.len(), tools_for_call.map_or(0, |t| t.len()));
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

            // Determine working directory
            let working_dir = project_path
                .as_ref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

            let ctx = ToolContext {
                working_dir,
                db: state.db.clone(),
                conversation_id: conversation_id.clone(),
            };

            let (result, duration_ms) = state
                .tool_registry
                .execute(&tc.name, &tc.arguments, &ctx)
                .await;

            tracing::info!("  Tool {} completed: success={}, {}ms", tc.name, result.success, duration_ms);
            let result_content = result.as_content_string();

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
async fn stream_and_collect(
    provider: &dyn Provider,
    model: &str,
    messages: &[ChatMessage],
    tools: Option<&[serde_json::Value]>,
    sse_tx: &mpsc::Sender<StreamChunk>,
) -> anyhow::Result<(String, Vec<ToolCall>)> {
    let (ptx, mut prx) = mpsc::channel::<StreamChunk>(64);

    // chat_stream sends all chunks to ptx then returns
    if let Err(e) = provider.chat_stream(model, messages, tools, ptx).await {
        tracing::error!("Provider stream error: {}", e);
        return Err(e);
    }

    let mut full_text = String::new();
    let mut pending_calls: HashMap<String, PendingToolCall> = HashMap::new();

    while let Some(chunk) = prx.recv().await {
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
                return Ok((full_text, Vec::new()));
            }
            _ => {
                let _ = sse_tx.send(chunk).await;
            }
        }
    }

    // Convert pending calls to ToolCall vec
    let tool_calls: Vec<ToolCall> = pending_calls
        .into_values()
        .map(|pc| {
            let args: serde_json::Value =
                serde_json::from_str(&pc.arguments_json).unwrap_or(serde_json::Value::Null);
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

/// List all registered tools (for skill builder UI)
async fn list_tools(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let defs = state.tool_registry.list_definitions();
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
// Skills handlers
// ---------------------------------------------------------------------------

async fn list_skills(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let db = state.db.clone();
    let skills = db
        .with_conn(|conn| SkillsManager::list(conn))
        .await
        .unwrap_or_default();

    Json(serde_json::json!({"skills": skills}))
}

#[derive(Deserialize)]
struct CreateSkillRequest {
    name: String,
    description: String,
    instructions: String,
    tools: Vec<String>,
    #[serde(default)]
    max_iterations: Option<u32>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    max_tokens: Option<u32>,
}

async fn create_skill(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateSkillRequest>,
) -> impl IntoResponse {
    let skill = Skill {
        id: uuid::Uuid::new_v4().to_string(),
        name: req.name,
        description: req.description,
        instructions: req.instructions,
        tools: req.tools,
        project_path: None,
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
    let skill_id = skill.id.clone();
    match db
        .with_conn(move |conn| SkillsManager::save(conn, &skill))
        .await
    {
        Ok(()) => Json(serde_json::json!({"ok": true, "id": skill_id})).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn update_skill(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<CreateSkillRequest>,
) -> impl IntoResponse {
    let skill = Skill {
        id,
        name: req.name,
        description: req.description,
        instructions: req.instructions,
        tools: req.tools,
        project_path: None,
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
        .with_conn(move |conn| SkillsManager::save(conn, &skill))
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

async fn delete_skill(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.clone();
    match db
        .with_conn(move |conn| SkillsManager::delete(conn, &id))
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
