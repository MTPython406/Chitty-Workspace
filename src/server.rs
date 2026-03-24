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

use axum::extract::{Path, Query, State};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::http::StatusCode;
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
use crate::providers::ollama::OllamaProvider;
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
    pub skill_registry: Arc<crate::skills::SkillRegistry>,
    pub oauth_pending: crate::oauth::PendingFlows,
    pub connection_manager: Arc<tokio::sync::RwLock<crate::connections::ConnectionManager>>,
}

/// Start the axum server on the given port.
pub async fn start(db: Database, tool_registry: Arc<ToolRegistry>, tool_runtime: Arc<tokio::sync::RwLock<ToolRuntime>>, browser_bridge: Arc<BrowserBridge>, skill_registry: Arc<crate::skills::SkillRegistry>, port: u16, bound_port_out: Arc<std::sync::atomic::AtomicU16>) -> anyhow::Result<()> {
    // Seed marketplace packages from bundled assets
    {
        let rt = tool_runtime.read().await;
        let marketplace_dir = rt.tools_dir().join("marketplace");

        // Resolve the assets directory independent of CWD:
        // 1. Try relative to the binary location (for installed/release builds)
        // 2. Try CARGO_MANIFEST_DIR (for cargo run during development)
        // 3. Fall back to CWD (last resort)
        let assets_base = {
            let mut candidates = Vec::new();

            // Next to the binary (release installs)
            if let Ok(exe) = std::env::current_exe() {
                if let Some(exe_dir) = exe.parent() {
                    candidates.push(exe_dir.join("assets").join("marketplace"));
                    // Also check one level up (binary might be in bin/ or target/release/)
                    if let Some(parent) = exe_dir.parent() {
                        candidates.push(parent.join("assets").join("marketplace"));
                    }
                }
            }

            // Cargo workspace root (development: cargo run)
            if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
                candidates.push(std::path::PathBuf::from(manifest_dir).join("assets").join("marketplace"));
            }

            // CWD fallback
            candidates.push(std::path::PathBuf::from("assets/marketplace"));

            candidates.into_iter().find(|p| p.exists())
        };

        if let Some(assets_marketplace) = assets_base {
            tracing::info!("Marketplace assets found at: {:?}", assets_marketplace);
            // Note: web-tools is now a native system tool (web_search, web_scraper) — not seeded from marketplace
            // "chitty" is the built-in orchestrator package (persona + config, tools are native)
            let packages = ["chitty", "google-cloud", "social-media", "slack", "google-gmail", "google-calendar"];
            for pkg_name in &packages {
                let pkg_dir = marketplace_dir.join(pkg_name);
                if !pkg_dir.exists() {
                    let assets_dir = assets_marketplace.join(pkg_name);
                    if assets_dir.exists() {
                        tracing::info!("Seeding marketplace package: {}", pkg_name);
                        if let Err(e) = copy_dir_recursive(&assets_dir, &pkg_dir) {
                            tracing::warn!("Failed to seed {} package: {}", pkg_name, e);
                        }
                    }
                }
            }
        } else {
            tracing::warn!("Marketplace assets directory not found — marketplace tools won't be available. \
                Searched relative to binary, CARGO_MANIFEST_DIR, and CWD.");
        }

        drop(rt);
        // Re-scan to pick up marketplace tools
        tool_runtime.write().await.scan_and_load();
    }

    // Load package configs (allowed resources, feature flags) from DB
    tool_runtime.write().await.load_package_configs(&db);

    // Auto-create package agents (1 package = 1 agent)
    {
        let rt = tool_runtime.read().await;
        let conn = db.connect().unwrap();
        for pkg in &rt.marketplace_packages {
            match crate::agents::AgentsManager::create_from_package(&conn, &pkg.manifest) {
                Ok(agent) => tracing::info!("Package agent ready: {} ({})", agent.name, agent.id),
                Err(e) => tracing::warn!("Failed to create agent for package {}: {}", pkg.manifest.name, e),
            }
        }
    }

    let oauth_pending = crate::oauth::PendingFlows::default();
    let connection_manager = Arc::new(tokio::sync::RwLock::new(
        crate::connections::ConnectionManager::new(),
    ));
    let state = Arc::new(AppState { db, tool_registry, tool_runtime, browser_bridge, skill_registry, oauth_pending, connection_manager });

    // Start the agent scheduler in the background
    let scheduler_state = state.clone();
    tokio::spawn(async move {
        crate::scheduler::initialize_next_runs(scheduler_state.clone()).await;
        crate::scheduler::run(scheduler_state).await;
    });

    // Start the connection manager in the background
    let conn_state = state.clone();
    tokio::spawn(async move {
        crate::connections::ConnectionManager::run(conn_state).await;
    });

    let app = Router::new()
        // Health check (for extension connectivity)
        .route("/health", get(|| async { "ok" }))
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
        .route("/api/agents/system", get(get_system_agent))
        .route("/api/agents", get(list_agents))
        .route("/api/agents", post(create_agent))
        .route("/api/agents/:id", get(get_agent))
        .route("/api/agents/:id", put(update_agent))
        .route("/api/agents/:id", delete(delete_agent))
        .route("/api/agents/:id/children", get(list_agent_children))
        .route("/api/agents/:id/sub-agents", post(create_sub_agent))
        // Skills
        .route("/api/skills", get(list_skills))
        .route("/api/skills/:name", get(get_skill))
        // Agent Builder
        .route("/api/agent-builder/generate", post(agent_builder_handler))
        // Scheduled Tasks
        .route("/api/schedules", get(list_schedules))
        .route("/api/schedules", post(create_schedule))
        .route("/api/schedules/:id", get(get_schedule))
        .route("/api/schedules/:id", put(update_schedule))
        .route("/api/schedules/:id", delete(delete_schedule))
        .route("/api/schedules/:id/run", post(run_schedule_now))
        // Browser bridge WebSocket
        .route("/ws/browser", get(ws_browser_handler))
        // Extension WebSocket — dedicated endpoint for the Chitty Browser Extension
        .route("/ws/extension", get(ws_extension_handler))
        // Extension HTTP polling — alternative to WebSocket for Manifest V3 service workers
        .route("/api/browser/poll", post(browser_poll_handler))
        .route("/api/browser/result", post(browser_result_handler))
        // Action approval system
        .route("/api/approval/respond", post(approval_response_handler))
        // OAuth integration flows (PKCE — no server needed)
        .route("/oauth/start/:provider", get(oauth_start_handler))
        .route("/oauth/callback", get(oauth_callback_handler))
        .route("/api/oauth/status", get(oauth_status_handler))
        .route("/api/oauth/status/:provider", get(oauth_provider_status_handler))
        .route("/api/oauth/disconnect/:provider", post(oauth_disconnect_handler))
        // Marketplace (local installed packages)
        .route("/api/marketplace/packages", get(list_marketplace_packages))
        .route("/api/marketplace/packages/:vendor/auth-status", get(check_package_auth))
        .route("/api/marketplace/packages/:vendor/auth", post(trigger_package_auth))
        .route("/api/marketplace/packages/:vendor/setup", post(run_package_setup))
        .route("/api/marketplace/packages/:vendor/details", get(get_package_details))
        .route("/api/marketplace/packages/:vendor/config", get(get_package_config))
        .route("/api/marketplace/packages/:vendor/config", put(save_package_config))
        .route("/api/marketplace/packages/:vendor/resources/discover", post(discover_package_resources))
        .route("/api/marketplace/packages/:vendor/disconnect", post(disconnect_package_auth))
        // Persistent Connections (marketplace package background processes)
        .route("/api/connections", get(list_connections_handler))
        .route("/api/connections/:package_id/:conn_id/start", post(start_connection_handler))
        .route("/api/connections/:package_id/:conn_id/stop", post(stop_connection_handler))
        .route("/api/connections/routes", get(list_connection_routes_handler))
        .route("/api/connections/routes/:package_id/:conn_id/:event_id", put(set_connection_route_handler))
        // Marketplace registry (remote — browse/search/install from chitty.ai)
        .route("/api/marketplace/registry/packages", get(registry_list_packages))
        .route("/api/marketplace/registry/search", get(registry_search))
        .route("/api/marketplace/registry/install", post(registry_install_package))
        // Local models — GPU, Ollama, sidecar management
        .route("/api/local/gpu", get(local_gpu_handler))
        .route("/api/local/status", get(local_status_handler))
        .route("/api/local/ollama/status", get(ollama_status_handler))
        .route("/api/local/ollama/models", get(ollama_models_handler))
        .route("/api/local/ollama/running", get(ollama_running_handler))
        .route("/api/local/ollama/pull", post(ollama_pull_handler))
        .route("/api/local/ollama/unload", post(ollama_unload_handler))
        .route("/api/local/ollama/models/:name", delete(ollama_delete_model_handler))
        // HuggingFace sidecar management
        .route("/api/local/sidecar/status", get(sidecar_status_handler))
        .route("/api/local/sidecar/start", post(sidecar_start_handler))
        .route("/api/local/sidecar/stop", post(sidecar_stop_handler))
        .route("/api/local/sidecar/models", get(sidecar_models_handler))
        .route("/api/local/sidecar/models/scan", post(sidecar_scan_handler))
        .route("/api/local/sidecar/models/register", post(sidecar_register_handler))
        .route("/api/local/sidecar/models/load", post(sidecar_load_handler))
        .route("/api/local/sidecar/models/unload", post(sidecar_unload_handler))
        // Direct GGUF scanning (no sidecar needed)
        .route("/api/local/gguf/scan", get(gguf_scan_local_handler))
        .with_state(state.clone());

    // Try the requested port first, then fall back to nearby ports
    let mut bound_port = port;
    let listener = match tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port)).await {
        Ok(l) => l,
        Err(_) => {
            // Port in use — try fallback ports
            let mut fallback_listener = None;
            for offset in 1..=10 {
                let try_port = port + offset;
                if let Ok(l) = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", try_port)).await {
                    bound_port = try_port;
                    tracing::warn!("Port {} in use, using fallback port {}", port, try_port);
                    fallback_listener = Some(l);
                    break;
                }
            }
            fallback_listener.ok_or_else(|| anyhow::anyhow!("Could not bind to any port {}-{}", port, port + 10))?
        }
    };
    tracing::info!("Server listening on http://127.0.0.1:{}", bound_port);
    bound_port_out.store(bound_port, std::sync::atomic::Ordering::SeqCst);

    // Spawn an HTTPS listener on port 8771 for OAuth callbacks.
    // Some providers (e.g., Slack) require HTTPS redirect URIs.
    // This uses a self-signed certificate for localhost.
    let https_state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = start_oauth_https_listener(https_state).await {
            tracing::warn!("HTTPS OAuth listener failed to start: {} — OAuth flows requiring HTTPS will fall back to HTTP", e);
        }
    });

    axum::serve(listener, app).await?;

    Ok(())
}

/// Start a minimal HTTPS server on port 8771 for OAuth callbacks.
/// This handles providers like Slack that require HTTPS redirect URIs.
async fn start_oauth_https_listener(state: Arc<AppState>) -> anyhow::Result<()> {
    use axum_server::tls_rustls::RustlsConfig;

    let data_dir = crate::storage::default_data_dir();
    let tls_certs = crate::tls::ensure_localhost_cert(&data_dir)?;

    let rustls_config = RustlsConfig::from_pem_file(
        &tls_certs.cert_path,
        &tls_certs.key_path,
    ).await?;

    // Minimal router — only OAuth callback + a redirect for the start flow
    let oauth_app = Router::new()
        .route("/oauth/callback", get(oauth_callback_handler))
        .route("/oauth/start/:provider", get(oauth_start_handler))
        .with_state(state);

    let https_port: u16 = 8771;
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], https_port));
    tracing::info!("HTTPS OAuth listener on https://127.0.0.1:{}", https_port);

    axum_server::bind_rustls(addr, rustls_config)
        .serve(oauth_app.into_make_service())
        .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// UI handler
// ---------------------------------------------------------------------------

/// Strip screenshot base64 data from tool results to keep LLM context small.
/// Replaces the huge base64 blob with a text summary so the agent knows
/// a screenshot was taken without the 500KB+ payload in context.
fn strip_screenshot_base64(content: &str) -> String {
    // Try to parse as JSON and check for screenshot_base64
    if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(content) {
        if let Some(obj) = val.as_object_mut() {
            if obj.contains_key("screenshot_base64") {
                // Replace base64 with a marker
                obj.insert(
                    "screenshot_base64".to_string(),
                    serde_json::Value::String("[screenshot captured and displayed to user]".to_string()),
                );
                return serde_json::to_string(obj).unwrap_or_else(|_| content.to_string());
            }
        }
    }
    content.to_string()
}

async fn index_handler() -> Html<&'static str> {
    Html(CHAT_HTML)
}

// ---------------------------------------------------------------------------
// OAuth Integration Handlers
// ---------------------------------------------------------------------------

/// Start an OAuth flow — generates PKCE challenge and redirects to provider
async fn oauth_start_handler(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
) -> impl IntoResponse {
    // Check if provider template exists
    let template = crate::oauth::providers::get_template(&provider);
    if template.is_none() {
        return (StatusCode::BAD_REQUEST, Html(format!(
            "<h2>Unknown provider: {}</h2>", provider
        ))).into_response();
    }
    let template = template.unwrap();

    let config = match crate::oauth::providers::get_config(&provider) {
        Some(c) => c,
        None => {
            // Not configured — show setup instructions
            return (StatusCode::BAD_REQUEST, Html(format!(
                "<html><body style='font-family:sans-serif;padding:40px;max-width:600px;margin:0 auto;'>\
                 <h2>{} — Setup Required</h2>\
                 <p>You need to configure your own OAuth credentials for {}.</p>\
                 <h3>Steps:</h3>\
                 <pre style='background:#f5f5f5;padding:16px;border-radius:8px;white-space:pre-wrap;'>{}</pre>\
                 <p><a href='{}' target='_blank'>Open {} Developer Console →</a></p>\
                 <p>Once you have the Client ID and Secret, go to <b>Chitty Settings → Integrations</b> and paste them.</p>\
                 </body></html>",
                template.display_name, template.display_name,
                template.setup_instructions, template.setup_url, template.display_name
            ))).into_response();
        }
    };

    let code_verifier = crate::oauth::generate_code_verifier();
    let code_challenge = crate::oauth::generate_code_challenge(&code_verifier);
    let oauth_state = crate::oauth::generate_state();

    // Store pending flow
    state.oauth_pending.lock().await.insert(
        oauth_state.clone(),
        crate::oauth::PendingFlow {
            provider: provider.clone(),
            code_verifier,
            created_at: std::time::Instant::now(),
        },
    );

    let auth_url = crate::oauth::build_auth_url(&config, &oauth_state, &code_challenge);
    tracing::info!("OAuth start: {} → redirecting to auth URL", provider);

    // Redirect the user's browser to the provider's login page
    axum::response::Redirect::temporary(&auth_url).into_response()
}

/// OAuth callback — receives the authorization code from the provider
async fn oauth_callback_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let code = match params.get("code") {
        Some(c) => c.clone(),
        None => {
            let error = params.get("error").map(|s| s.as_str()).unwrap_or("unknown");
            let desc = params.get("error_description").map(|s| s.as_str()).unwrap_or("");
            return Html(format!(
                "<html><body style='font-family:sans-serif;padding:40px;text-align:center;'>\
                 <h2 style='color:#e44'>Authorization Failed</h2>\
                 <p>Error: {} {}</p>\
                 <p>You can close this tab and try again from Chitty Workspace Settings.</p>\
                 </body></html>",
                error, desc
            )).into_response();
        }
    };

    let oauth_state = match params.get("state") {
        Some(s) => s.clone(),
        None => {
            return Html("<h2>Missing state parameter</h2>".to_string()).into_response();
        }
    };

    // Look up the pending flow
    let pending = state.oauth_pending.lock().await.remove(&oauth_state);
    let pending = match pending {
        Some(p) => p,
        None => {
            return Html("<h2>Invalid or expired OAuth state</h2><p>Please try connecting again from Settings.</p>".to_string()).into_response();
        }
    };

    // Check for expired flows (> 10 minutes)
    if pending.created_at.elapsed() > std::time::Duration::from_secs(600) {
        return Html("<h2>OAuth flow expired</h2><p>Please try again.</p>".to_string()).into_response();
    }

    let config = match crate::oauth::providers::get_config(&pending.provider) {
        Some(c) => c,
        None => {
            return Html("<h2>Unknown provider</h2>".to_string()).into_response();
        }
    };

    // Exchange the authorization code for tokens
    match crate::oauth::exchange_code(&config, &code, &pending.code_verifier).await {
        Ok(tokens) => {
            // Save tokens to OS keyring
            if let Err(e) = crate::oauth::save_tokens(&pending.provider, &tokens) {
                tracing::error!("Failed to save OAuth tokens: {}", e);
                return Html(format!(
                    "<html><body style='font-family:sans-serif;padding:40px;text-align:center;'>\
                     <h2 style='color:#e44'>Failed to save credentials</h2>\
                     <p>{}</p></body></html>", e
                )).into_response();
            }

            tracing::info!("OAuth connected: {} (scopes: {:?})", pending.provider, tokens.scopes);

            Html(format!(
                "<html><body style='font-family:sans-serif;padding:40px;text-align:center;background:#0d1117;color:#e6edf3;'>\
                 <div style='max-width:400px;margin:0 auto;'>\
                 <div style='font-size:48px;margin-bottom:16px;'>✅</div>\
                 <h2 style='color:#3fb950;margin-bottom:8px;'>Connected!</h2>\
                 <p style='color:#8b949e;margin-bottom:24px;'>{} is now connected to Chitty Workspace.</p>\
                 <p style='color:#8b949e;font-size:14px;'>You can close this tab and return to Chitty.</p>\
                 <script>setTimeout(() => window.close(), 3000);</script>\
                 </div></body></html>",
                pending.provider
            )).into_response()
        }
        Err(e) => {
            tracing::error!("OAuth token exchange failed: {}", e);
            Html(format!(
                "<html><body style='font-family:sans-serif;padding:40px;text-align:center;'>\
                 <h2 style='color:#e44'>Connection Failed</h2>\
                 <p>{}</p>\
                 <p>Please try again from Chitty Workspace Settings.</p>\
                 </body></html>", e
            )).into_response()
        }
    }
}

/// Get connection status for all OAuth integrations
async fn oauth_status_handler() -> impl IntoResponse {
    let statuses = crate::oauth::get_all_status();
    Json(serde_json::json!({"integrations": statuses}))
}

/// Get connection status for a specific provider
async fn oauth_provider_status_handler(
    Path(provider): Path<String>,
) -> impl IntoResponse {
    let connected = crate::oauth::is_connected(&provider);
    Json(serde_json::json!({"provider": provider, "connected": connected}))
}

/// Disconnect an OAuth integration (remove tokens from keyring)
async fn oauth_disconnect_handler(
    Path(provider): Path<String>,
) -> impl IntoResponse {
    match crate::oauth::disconnect(&provider) {
        Ok(()) => Json(serde_json::json!({"ok": true, "provider": provider})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ).into_response(),
    }
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

async fn handle_browser_ws(mut socket: WebSocket, _state: Arc<AppState>) {
    // This endpoint is for the chat UI only (activity logging, status updates).
    // Browser commands go through /ws/extension to the Chitty Browser Extension.
    tracing::info!("Chat UI WebSocket connected on /ws/browser");

    loop {
        match socket.recv().await {
            Some(Ok(Message::Text(_))) => {
                // Chat UI might send status messages — just acknowledge
            }
            Some(Ok(Message::Close(_))) | None => break,
            Some(Err(_)) => break,
            _ => {}
        }
    }

    tracing::info!("Chat UI WebSocket disconnected");
}

// ---------------------------------------------------------------------------
// Action Approval System — sensitive actions require user consent
// ---------------------------------------------------------------------------

/// Pending approval senders, keyed by approval_id
static PENDING_APPROVALS: std::sync::LazyLock<Mutex<HashMap<String, oneshot::Sender<bool>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Deserialize)]
struct ApprovalResponse {
    approval_id: String,
    approved: bool,
}

/// User responds to an approval request (approve/deny)
async fn approval_response_handler(
    Json(resp): Json<ApprovalResponse>,
) -> impl IntoResponse {
    tracing::info!("Approval response: {} approved={}", resp.approval_id, resp.approved);
    if let Some(tx) = PENDING_APPROVALS.lock().await.remove(&resp.approval_id) {
        let _ = tx.send(resp.approved);
    }
    StatusCode::OK
}

/// Actions that require user approval before executing.
/// When `auto_approve` is true (agent configured for autonomous mode), all actions are skipped.
fn action_requires_approval(tool_name: &str, action: &str, auto_approve: bool) -> bool {
    if auto_approve {
        return false;
    }
    match tool_name {
        "browser" => matches!(action, "click" | "type" | "execute_js" | "open"),
        "terminal" => true,
        "file_writer" => true,
        "install_package" => true,
        _ => false,
    }
}

/// Build a human-readable description of what the action will do
fn describe_action(tool_name: &str, args: &serde_json::Value) -> (String, serde_json::Value) {
    let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("unknown");
    match (tool_name, action) {
        ("browser", "open") => {
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("unknown");
            (
                format!("Navigate browser to {}", url),
                serde_json::json!({ "action": "open", "url": url, "icon": "🌐" })
            )
        }
        ("browser", "click") => {
            let selector = args.get("selector").and_then(|v| v.as_str()).unwrap_or("?");
            (
                format!("Click element: {}", selector),
                serde_json::json!({ "action": "click", "selector": selector, "icon": "👆" })
            )
        }
        ("browser", "type") => {
            let selector = args.get("selector").and_then(|v| v.as_str()).unwrap_or("?");
            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let preview = if text.len() > 100 { format!("{}...", &text[..100]) } else { text.to_string() };
            (
                format!("Type text into: {}", selector),
                serde_json::json!({ "action": "type", "selector": selector, "text_preview": preview, "icon": "⌨️" })
            )
        }
        ("browser", "execute_js") => {
            let script = args.get("script").and_then(|v| v.as_str()).unwrap_or("");
            let preview = if script.len() > 80 { format!("{}...", &script[..80]) } else { script.to_string() };
            (
                format!("Execute JavaScript on page"),
                serde_json::json!({ "action": "execute_js", "script_preview": preview, "icon": "⚡" })
            )
        }
        ("terminal", _) => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("?");
            let preview = if cmd.len() > 120 { format!("{}...", &cmd[..120]) } else { cmd.to_string() };
            (
                format!("Run terminal command: {}", preview),
                serde_json::json!({ "action": "terminal", "command": preview, "icon": "💻" })
            )
        }
        ("file_writer", _) => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            (
                format!("Write file: {}", path),
                serde_json::json!({ "action": "write", "path": path, "icon": "📝" })
            )
        }
        ("install_package", _) => {
            let runtime = args.get("runtime").and_then(|v| v.as_str()).unwrap_or("?");
            let packages = args.get("packages").and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|p| p.as_str()).collect::<Vec<_>>().join(", "))
                .unwrap_or_else(|| "?".to_string());
            (
                format!("Install {} packages: {}", runtime, packages),
                serde_json::json!({ "action": "install", "runtime": runtime, "packages": packages, "icon": "📦" })
            )
        }
        _ => (
            format!("{}: {}", tool_name, action),
            serde_json::json!({ "action": action, "icon": "🔧" })
        )
    }
}

// ---------------------------------------------------------------------------
// Extension HTTP polling — for Manifest V3 service workers that can't hold WebSockets
// ---------------------------------------------------------------------------

/// Pending result senders, keyed by command ID
static PENDING_RESULTS: std::sync::LazyLock<Mutex<HashMap<String, oneshot::Sender<BrowserResponse>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Long-poll: wait for a pending browser command (up to 25s)
async fn browser_poll_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let bridge = &state.browser_bridge;
    bridge.set_connected(true);

    let mut cmd_rx = bridge.cmd_rx.lock().await;

    // Wait up to 25 seconds for a command
    match tokio::time::timeout(
        std::time::Duration::from_secs(25),
        cmd_rx.recv()
    ).await {
        Ok(Some((browser_cmd, resp_tx))) => {
            // Store the response sender so browser_result_handler can complete it
            let cmd_id = browser_cmd.id.clone();
            PENDING_RESULTS.lock().await.insert(cmd_id, resp_tx);

            tracing::info!("Extension poll: sending command {} ({})", browser_cmd.action, browser_cmd.id);
            (StatusCode::OK, Json(serde_json::json!({
                "id": browser_cmd.id,
                "action": browser_cmd.action,
                "params": browser_cmd.params,
            })))
        }
        Ok(None) => {
            // Channel closed
            (StatusCode::NO_CONTENT, Json(serde_json::json!(null)))
        }
        Err(_) => {
            // Timeout — no command available, extension should poll again
            (StatusCode::NO_CONTENT, Json(serde_json::json!(null)))
        }
    }
}

/// Receive a command result from the extension
async fn browser_result_handler(
    Json(resp): Json<BrowserResponse>,
) -> impl IntoResponse {
    tracing::info!("Extension result: {} success={}", resp.id, resp.success);

    if let Some(tx) = PENDING_RESULTS.lock().await.remove(&resp.id) {
        let _ = tx.send(resp);
    } else {
        tracing::warn!("Extension result for unknown command: {}", resp.id);
    }

    StatusCode::OK
}

// ---------------------------------------------------------------------------
// Extension WebSocket — dedicated endpoint for the Chitty Browser Extension
// This is the PRIMARY handler for browser commands. The extension connects here
// and receives all open/click/type/screenshot commands from the agent.
// ---------------------------------------------------------------------------

async fn ws_extension_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_extension_ws(socket, state))
}

async fn handle_extension_ws(mut socket: WebSocket, state: Arc<AppState>) {
    let bridge = &state.browser_bridge;
    bridge.set_connected(true);
    tracing::info!("Chitty Browser Extension connected via /ws/extension");

    let mut pending: HashMap<String, oneshot::Sender<BrowserResponse>> = HashMap::new();
    let mut cmd_rx = bridge.cmd_rx.lock().await;

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some((browser_cmd, resp_tx)) => {
                        let cmd_id = browser_cmd.id.clone();
                        tracing::info!("Extension: sending command {} ({})", browser_cmd.action, cmd_id);
                        match serde_json::to_string(&browser_cmd) {
                            Ok(json) => {
                                if socket.send(Message::Text(json.into())).await.is_err() {
                                    tracing::warn!("Extension WS send failed");
                                    let _ = resp_tx.send(BrowserResponse {
                                        id: cmd_id,
                                        success: false,
                                        data: serde_json::Value::Null,
                                        error: Some("Extension WebSocket disconnected".into()),
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
                    None => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<BrowserResponse>(&text) {
                            Ok(resp) => {
                                tracing::info!("Extension: response for {} success={}", resp.id, resp.success);
                                if let Some(tx) = pending.remove(&resp.id) {
                                    let _ = tx.send(resp);
                                }
                            }
                            Err(e) => tracing::warn!("Invalid extension response: {}", e),
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => { tracing::warn!("Extension WS error: {}", e); break; }
                    _ => {}
                }
            }
        }
    }

    bridge.set_connected(false);
    for (id, tx) in pending {
        let _ = tx.send(BrowserResponse {
            id, success: false, data: serde_json::Value::Null,
            error: Some("Extension disconnected".into()),
        });
    }
    tracing::info!("Chitty Browser Extension disconnected");
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
    // Approval fields
    #[serde(skip_serializing_if = "Option::is_none")]
    approval_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    action_description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<serde_json::Value>,
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
            StreamChunk::ApprovalRequest {
                approval_id,
                tool_name,
                action_description,
                details,
            } => Event::default()
                .event("approval_request")
                .data(
                    serde_json::to_string(&ChatEventData {
                        approval_id: Some(approval_id),
                        tool_name: Some(tool_name),
                        action_description: Some(action_description),
                        details: Some(details),
                        ..Default::default()
                    })
                    .unwrap_or_default(),
                ),
            StreamChunk::TokenUsage { input_tokens, output_tokens, cache_read_tokens, cache_write_tokens } => Event::default()
                .event("token_usage")
                .data(
                    serde_json::to_string(&ChatEventData {
                        token_usage: Some(TokenUsageResponse {
                            prompt_tokens: input_tokens,
                            completion_tokens: output_tokens,
                            total_tokens: input_tokens + output_tokens,
                        }),
                        ..Default::default()
                    })
                    .unwrap_or_default(),
                ),
            StreamChunk::ContextInfo { used_tokens, max_tokens, percentage } => Event::default()
                .event("context_info")
                .data(
                    serde_json::json!({
                        "used_tokens": used_tokens,
                        "max_tokens": max_tokens,
                        "percentage": percentage,
                    }).to_string(),
                ),
            StreamChunk::AgentStart { agent_name, agent_icon, instruction } => Event::default()
                .event("agent_start")
                .data(serde_json::json!({
                    "agent_name": agent_name,
                    "agent_icon": agent_icon,
                    "instruction": instruction,
                }).to_string()),
            StreamChunk::AgentText { agent_name, text } => Event::default()
                .event("agent_text")
                .data(serde_json::json!({
                    "agent_name": agent_name,
                    "content": text,
                }).to_string()),
            StreamChunk::AgentToolCall { agent_name, tool_name, tool_args } => Event::default()
                .event("agent_tool_call")
                .data(serde_json::json!({
                    "agent_name": agent_name,
                    "tool_name": tool_name,
                    "tool_args": tool_args,
                }).to_string()),
            StreamChunk::AgentToolResult { agent_name, tool_name, success, result_preview, duration_ms } => Event::default()
                .event("agent_tool_result")
                .data(serde_json::json!({
                    "agent_name": agent_name,
                    "tool_name": tool_name,
                    "success": success,
                    "result_preview": result_preview,
                    "duration_ms": duration_ms,
                }).to_string()),
            StreamChunk::AgentComplete { agent_name, response } => Event::default()
                .event("agent_complete")
                .data(serde_json::json!({
                    "agent_name": agent_name,
                    "response": response,
                }).to_string()),
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
    // Local providers don't need API keys
    let provider_id = provider_str.parse::<ProviderId>()?;

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

    match provider_id {
        ProviderId::Ollama => {
            let url = base_url.unwrap_or_else(|| "http://localhost:11434".to_string());
            Ok(Box::new(OllamaProvider::new(url)))
        }
        ProviderId::Huggingface => {
            let _url = base_url.unwrap_or_else(|| "http://localhost:8766".to_string());
            // LocalProvider will be added in Phase 2 — for now, use the sidecar URL
            anyhow::bail!("Local sidecar provider not yet implemented. Use Ollama for local models.")
        }
        _ => {
            // Cloud providers require API keys
            let api_key = config::get_api_key(provider_str)?
                .ok_or_else(|| anyhow::anyhow!("No API key configured for {}", provider_str))?;

            match provider_id {
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

    let skills_ref = state.skill_registry.clone();

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

            // Assemble context (with tools + skills catalog + agent instructions)
            let (ctx, exec_cfg, eff_pp) = ChatEngine::assemble_context(
                conn,
                &cid,
                sid.as_deref(),
                pp.as_deref(),
                &all_tool_defs,
                &skills_ref,
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

    // Send context assembly stats to activity log
    {
        let sp = &context.system_prompt;
        let has_project = sp.contains("## Project Context");
        let has_memories = sp.contains("## Active Memories") || sp.contains("## Memories");
        let has_skills = sp.contains("<skill ") || sp.contains("## Available Skills");
        let has_packages = sp.contains("## Available Package Agents");
        let parts: Vec<&str> = [
            if has_memories { Some("memories") } else { None },
            if has_skills { Some("skills") } else { None },
            if has_packages { Some("packages") } else { None },
            if has_project { Some("project context") } else { None },
        ].iter().filter_map(|x| *x).collect();
        let loaded_str = if parts.is_empty() { String::new() } else { format!("Loaded {}", parts.join(", ")) };
        if !loaded_str.is_empty() {
            let _ = sse_tx.send(StreamChunk::Thinking(loaded_str)).await;
        }
    }

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

    // 3b. Model-aware context budget
    let model_context_tokens = get_model_context_window(&provider_str, &model_str);
    let context_budget_pct = exec_config.context_budget_pct.max(10).min(95); // clamp to 10-95%
    let token_budget = (model_context_tokens as u64 * context_budget_pct as u64 / 100) as usize;
    let char_budget = token_budget * 4; // rough chars-to-tokens estimate
    tracing::info!(
        "Context budget: model={} tokens, budget={}% → {} tokens ({} chars)",
        model_context_tokens, context_budget_pct, token_budget, char_budget
    );

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

        // Token usage is now returned by stream_and_collect

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

        // 4. Context budget check & compaction (model-aware)
        // Count ALL content: message text + tool_calls JSON + tool definitions
        let prompt_chars: usize = current_messages.iter().map(|m| {
            let mut size = m.content.len();
            // Tool calls are serialized as JSON in the API payload — count them
            if let Some(ref tc) = m.tool_calls {
                size += serde_json::to_string(tc).map(|s| s.len()).unwrap_or(0);
            }
            size
        }).sum::<usize>()
            // Tool definitions (schemas) are also sent in every API call
            + current_tools.iter().map(|t| serde_json::to_string(t).map(|s| s.len()).unwrap_or(200)).sum::<usize>()
            // System prompt is already in current_messages[0]
            ;
        let estimated_tokens = (prompt_chars / 4) as u32;

        // Send context usage info to UI
        let usage_pct = ((estimated_tokens as u64 * 100) / model_context_tokens.max(1) as u64).min(100) as u8;
        let _ = sse_tx
            .send(StreamChunk::ContextInfo {
                used_tokens: estimated_tokens,
                max_tokens: model_context_tokens,
                percentage: usage_pct,
            })
            .await;

        // Auto-compact if prompt exceeds the dynamic budget
        if prompt_chars > char_budget && current_messages.len() > 4 {
            tracing::info!(
                "Context compaction triggered: {} chars (~{} tokens) > {} char budget ({}% of {} model limit)",
                prompt_chars, estimated_tokens, char_budget, context_budget_pct, model_context_tokens
            );

            if exec_config.compaction_strategy == "summarize" {
                let _ = sse_tx
                    .send(StreamChunk::Thinking(format!(
                        "Context at {}% — summarizing older messages...",
                        usage_pct
                    )))
                    .await;

                summarize_compact(&mut current_messages, &state, &provider_str).await;
            } else {
                let _ = sse_tx
                    .send(StreamChunk::Thinking(format!(
                        "Context at {}% — compacting older messages...",
                        usage_pct
                    )))
                    .await;

                truncate_compact(&mut current_messages, char_budget);
            }

            let new_chars: usize = current_messages.iter().map(|m| m.content.len()).sum();
            tracing::info!("Context compacted: {} -> {} chars ({} messages)", prompt_chars, new_chars, current_messages.len());
        }

        // Pre-flight validation: if still over model's HARD limit, aggressive compact
        let prompt_chars: usize = current_messages.iter().map(|m| m.content.len()).sum();
        let hard_limit_chars = (model_context_tokens as usize) * 4; // full model context in chars
        if prompt_chars > hard_limit_chars {
            tracing::warn!(
                "Context still exceeds model hard limit after compaction: {} chars > {} limit. Aggressive compact.",
                prompt_chars, hard_limit_chars
            );
            let _ = sse_tx
                .send(StreamChunk::Thinking("Context exceeds model limit — aggressive compaction...".to_string()))
                .await;

            aggressive_compact(&mut current_messages);

            let final_chars: usize = current_messages.iter().map(|m| m.content.len()).sum();
            if final_chars > hard_limit_chars {
                // Even aggressive compact wasn't enough — error out gracefully instead of API crash
                let _ = sse_tx
                    .send(StreamChunk::Error(format!(
                        "Context ({} tokens) still exceeds model limit ({} tokens) after compaction. Please start a new conversation.",
                        final_chars / 4, model_context_tokens
                    )))
                    .await;
                break;
            }
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

        let (full_text, tool_calls, iter_input_tokens, iter_output_tokens, iter_cache_read, iter_cache_write) = stream_and_collect(
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

            // Use real token usage from provider if available, fall back to estimate
            let real_input = iter_input_tokens;
            let real_output = iter_output_tokens;
            let (final_input, final_output) = if real_input > 0 || real_output > 0 {
                (real_input, real_output)
            } else {
                // Fallback estimate (chars / 4)
                let est_prompt: u32 = current_messages.iter().map(|m| m.content.len() as u32).sum::<u32>() / 4;
                let est_completion = (resp.len() as u32) / 4;
                (est_prompt, est_completion)
            };
            tracing::info!("Token usage: input={}, output={} (real={}) cache_read={}, cache_write={}",
                final_input, final_output, real_input > 0, iter_cache_read, iter_cache_write);

            let _ = db
                .with_conn(move |conn| {
                    let msg = ChatEngine::save_message(
                        conn, &cid, "assistant", &resp, None, None,
                    )?;

                    // Track token usage
                    let usage_id = uuid::Uuid::new_v4().to_string();
                    conn.execute(
                        "INSERT INTO token_usage (id, conversation_id, message_id, provider_id, model_id, prompt_tokens, completion_tokens, total_tokens) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        rusqlite::params![usage_id, msg.conversation_id, msg.id, prov_id, mdl_id, final_input, final_output, final_input + final_output],
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

        // Save token usage for this tool-call iteration
        let tc_input = iter_input_tokens;
        let tc_output = iter_output_tokens;
        let tc_prov = provider_str.clone();
        let tc_mdl = model_str.clone();

        db.with_conn(move |conn| {
            let msg = ChatEngine::save_message(
                conn,
                &cid,
                "assistant",
                &resp,
                Some(&tc_json),
                None,
            )?;

            // Track token usage for this iteration (tool call iterations count too!)
            if tc_input > 0 || tc_output > 0 {
                let usage_id = uuid::Uuid::new_v4().to_string();
                conn.execute(
                    "INSERT INTO token_usage (id, conversation_id, message_id, provider_id, model_id, prompt_tokens, completion_tokens, total_tokens) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    rusqlite::params![usage_id, msg.conversation_id, msg.id, tc_prov, tc_mdl, tc_input, tc_output, tc_input + tc_output],
                )?;
                tracing::info!("Token usage (tool iter): input={}, output={}", tc_input, tc_output);
            }

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

            // ── Sub-agent locked_params merge ────────────────────────
            // If this agent has scoped tools, auto-merge locked_params into tool arguments
            let mut merged_args = tc.arguments.clone();
            for sat in &exec_config.sub_agent_tools {
                if sat.tool_name == tc.name {
                    if let (Some(locked), Some(args_obj)) = (sat.locked_params.as_object(), merged_args.as_object_mut()) {
                        for (k, v) in locked {
                            args_obj.entry(k.clone()).or_insert(v.clone());
                        }
                        tracing::info!("  Merged locked_params for sub-agent tool {}: {:?}", tc.name, locked.keys().collect::<Vec<_>>());
                    }
                    break;
                }
            }

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

            // ── Action Approval Gate ──────────────────────────────────
            // For sensitive tool actions (browser click/type/open/js), pause and
            // ask the user for approval before executing.
            let action_str = merged_args.get("action").and_then(|v| v.as_str()).unwrap_or("");
            if action_requires_approval(&tc.name, action_str, exec_config.auto_approve) {
                let approval_id = uuid::Uuid::new_v4().to_string();
                let (description, details) = describe_action(&tc.name, &merged_args);

                // Send approval request to frontend via SSE
                let _ = sse_tx
                    .send(StreamChunk::ApprovalRequest {
                        approval_id: approval_id.clone(),
                        tool_name: tc.name.clone(),
                        action_description: description.clone(),
                        details,
                    })
                    .await;

                // Wait for user response (up to 120 seconds)
                let (approval_tx, approval_rx) = oneshot::channel::<bool>();
                PENDING_APPROVALS.lock().await.insert(approval_id.clone(), approval_tx);

                let approved = match tokio::time::timeout(
                    std::time::Duration::from_secs(120),
                    approval_rx,
                ).await {
                    Ok(Ok(true)) => true,
                    _ => false,
                };
                PENDING_APPROVALS.lock().await.remove(&approval_id);

                if !approved {
                    tracing::info!("  Action denied by user: {} {}", tc.name, action_str);
                    let result_content = format!("Action denied by user. The user chose not to allow: {}. Ask the user what they'd like instead.", description);

                    // Send denied result
                    let _ = sse_tx
                        .send(StreamChunk::ToolResult {
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                            content: result_content.clone(),
                            success: false,
                            duration_ms: 0,
                        })
                        .await;

                    // Save denied result to DB so conversation stays valid
                    let db = state.db.clone();
                    let cid = conversation_id.clone();
                    let tc_id = tc.id.clone();
                    let rc = result_content.clone();
                    db.with_conn(move |conn| {
                        ChatEngine::save_message(conn, &cid, "tool", &rc, None, Some(&tc_id))?;
                        Ok(())
                    }).await?;

                    current_messages.push(ChatMessage {
                        role: "tool".to_string(),
                        content: result_content,
                        tool_calls: None,
                        tool_call_id: Some(tc.id.clone()),
                    });
                    continue; // skip execution, move to next tool call
                }
                tracing::info!("  Action approved by user: {} {}", tc.name, action_str);
            }

            // Handle frontend-only tools (UI commands the frontend intercepts)
            if tc.name == "open_agent_panel" {
                let agent_id = merged_args.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
                let message = merged_args.get("message").and_then(|v| v.as_str()).unwrap_or("");
                let result_content = format!(
                    r#"{{"success":true,"action":"open_agent_panel","agent_id":"{}","message":"{}"}}"#,
                    agent_id, message.replace('"', "\\\"")
                );
                let _ = sse_tx
                    .send(StreamChunk::ToolResult {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        content: result_content.clone(),
                        success: true,
                        duration_ms: 0,
                    })
                    .await;
                // Save tool result and add to messages
                let db = state.db.clone();
                let cid = conversation_id.clone();
                let tc_id = tc.id.clone();
                let rc = result_content.clone();
                db.with_conn(move |conn| {
                    ChatEngine::save_message(conn, &cid, "tool", &rc, None, Some(&tc_id))?;
                    Ok(())
                }).await?;
                current_messages.push(ChatMessage {
                    role: "tool".to_string(),
                    content: result_content,
                    tool_calls: None,
                    tool_call_id: Some(tc.id.clone()),
                });
                continue;
            }

            // Handle dispatch_agents (orchestrator dispatches to package agents)
            if tc.name == "dispatch_agents" {
                let start = std::time::Instant::now();
                let dispatch_result = run_dispatch(&state, &merged_args, &sse_tx).await;
                let duration_ms = start.elapsed().as_millis() as u64;
                let result_content = serde_json::to_string(&dispatch_result).unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string());
                let success = dispatch_result.get("error").is_none();

                let _ = sse_tx
                    .send(StreamChunk::ToolResult {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        content: result_content.clone(),
                        success,
                        duration_ms,
                    })
                    .await;
                let db = state.db.clone();
                let cid = conversation_id.clone();
                let tc_id = tc.id.clone();
                let rc = result_content.clone();
                db.with_conn(move |conn| {
                    ChatEngine::save_message(conn, &cid, "tool", &rc, None, Some(&tc_id))?;
                    Ok(())
                }).await?;
                current_messages.push(ChatMessage {
                    role: "tool".to_string(),
                    content: result_content,
                    tool_calls: None,
                    tool_call_id: Some(tc.id.clone()),
                });
                continue;
            }

            // Handle execute_package_tool (Tier 1: direct tool execution, no LLM)
            if tc.name == "execute_package_tool" {
                let start = std::time::Instant::now();
                let pkg_name = merged_args.get("package").and_then(|v| v.as_str()).unwrap_or("");
                let tool_name_raw = merged_args.get("tool").and_then(|v| v.as_str()).unwrap_or("");
                let tool_args = merged_args.get("arguments").cloned().unwrap_or(serde_json::json!({}));

                // Resolve tool name: try as-is, then with package prefix, then kebab→snake
                let tool_name = tool_name_raw.replace('-', "_");
                let display_pkg = pkg_name.replace('-', " ");

                // Stream: Agent start
                let _ = sse_tx.send(StreamChunk::AgentStart {
                    agent_name: display_pkg.clone(),
                    agent_icon: "📦".to_string(),
                    instruction: format!("Direct call: {}", tool_name),
                }).await;

                // Stream: Tool call
                let _ = sse_tx.send(StreamChunk::AgentToolCall {
                    agent_name: display_pkg.clone(),
                    tool_name: tool_name.clone(),
                    tool_args: tool_args.clone(),
                }).await;

                // Execute the tool directly via runtime
                let tool_runtime = state.tool_runtime.read().await;
                let (result, dur) = tool_runtime.execute(&tool_name, &tool_args, &ctx).await;
                drop(tool_runtime);

                let result_content = result.as_content_string();
                let result_preview = if result_content.len() > 300 {
                    format!("{}...", &result_content[..300])
                } else {
                    result_content.clone()
                };

                // Stream: Tool result
                let _ = sse_tx.send(StreamChunk::AgentToolResult {
                    agent_name: display_pkg.clone(),
                    tool_name: tool_name.clone(),
                    success: result.success,
                    result_preview,
                    duration_ms: dur,
                }).await;

                // Stream: Agent complete
                let _ = sse_tx.send(StreamChunk::AgentComplete {
                    agent_name: display_pkg.clone(),
                    response: if result.success { "Tool executed successfully".to_string() } else { "Tool execution failed".to_string() },
                }).await;

                let duration_ms = start.elapsed().as_millis() as u64;

                // Send ToolResult for the parent tool call
                let _ = sse_tx.send(StreamChunk::ToolResult {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    content: result_content.clone(),
                    success: result.success,
                    duration_ms,
                }).await;

                // Save to DB
                let db = state.db.clone();
                let cid = conversation_id.clone();
                let tc_id = tc.id.clone();
                let rc = result_content.clone();
                db.with_conn(move |conn| {
                    ChatEngine::save_message(conn, &cid, "tool", &rc, None, Some(&tc_id))?;
                    Ok(())
                }).await?;

                current_messages.push(ChatMessage {
                    role: "tool".to_string(),
                    content: result_content,
                    tool_calls: None,
                    tool_call_id: Some(tc.id.clone()),
                });
                continue;
            }

            // Dispatch via tool_runtime (native + custom + connection tools)
            let tool_runtime = state.tool_runtime.read().await;
            let (result, duration_ms) = tool_runtime
                .execute(&tc.name, &merged_args, &ctx)
                .await;
            drop(tool_runtime);

            let mut result_content = result.as_content_string();

            // Size-limit tool results to prevent context bloat
            // Browser screenshots (base64) can be 100K+ chars
            if result_content.contains("data:image/") || result_content.contains("base64,") {
                if let Some(start) = result_content.find("data:image/") {
                    let preview = result_content[..start.min(500)].to_string();
                    result_content = format!("{}[screenshot captured — image data stripped from context to save tokens]", preview);
                }
            }
            // Cap any single tool result at 20K chars (prevents full HTML pages, huge JSON, etc.)
            if result_content.len() > 20_000 {
                let safe_end = result_content.char_indices().nth(15_000).map(|(i,_)|i).unwrap_or(15_000.min(result_content.len()));
                result_content = format!(
                    "{}\n\n[... result truncated: {} chars total, showing first 15000 to save context]",
                    &result_content[..safe_end], result_content.len()
                );
            }

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

            // Add tool result to current_messages for next LLM call.
            // Strip large base64 data (screenshots) to avoid blowing up context window.
            let llm_content = strip_screenshot_base64(&result_content);
            current_messages.push(ChatMessage {
                role: "tool".to_string(),
                content: llm_content,
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
/// Returns (full_text, tool_calls, input_tokens, output_tokens, cache_read, cache_write)
async fn stream_and_collect(
    provider: &dyn Provider,
    model: &str,
    messages: &[ChatMessage],
    tools: Option<&[serde_json::Value]>,
    sse_tx: &mpsc::Sender<StreamChunk>,
) -> anyhow::Result<(String, Vec<ToolCall>, u32, u32, u32, u32)> {
    // Use a larger channel buffer to reduce backpressure on the provider
    let (ptx, mut prx) = mpsc::channel::<StreamChunk>(256);

    let mut full_text = String::new();
    let mut pending_calls: HashMap<String, PendingToolCall> = HashMap::new();

    // Run chat_stream and chunk processing concurrently.
    // chat_stream writes to ptx; we read from prx in parallel.
    // When chat_stream finishes, ptx is dropped, prx.recv() returns None.
    let idle_timeout = std::time::Duration::from_secs(120);
    let mut chunk_count: u64 = 0;
    let mut collected_input_tokens: u32 = 0;
    let mut collected_output_tokens: u32 = 0;
    let mut collected_cache_read: u32 = 0;
    let mut collected_cache_write: u32 = 0;

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
                            StreamChunk::TokenUsage { input_tokens, output_tokens, cache_read_tokens, cache_write_tokens } => {
                                // Accumulate real token usage from provider
                                collected_input_tokens += input_tokens;
                                collected_output_tokens += output_tokens;
                                collected_cache_read += cache_read_tokens;
                                collected_cache_write += cache_write_tokens;
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
            let args: serde_json::Value = if pc.arguments_json.is_empty() {
                tracing::warn!("Tool call '{}' ({}) has empty arguments — no input_json_delta received",
                    pc.name, pc.id);
                serde_json::json!({})
            } else {
                match serde_json::from_str(&pc.arguments_json) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!("Failed to parse tool call arguments for '{}': {} — raw: {:?}",
                            pc.name, e, pc.arguments_json);
                        // Try to salvage partial JSON by closing any open braces
                        let mut salvaged = pc.arguments_json.clone();
                        let open_braces = salvaged.chars().filter(|c| *c == '{').count();
                        let close_braces = salvaged.chars().filter(|c| *c == '}').count();
                        for _ in 0..(open_braces.saturating_sub(close_braces)) {
                            salvaged.push('}');
                        }
                        serde_json::from_str(&salvaged).unwrap_or_else(|_| {
                            tracing::error!("Salvage attempt also failed for '{}'", pc.name);
                            serde_json::json!({})
                        })
                    }
                }
            };
            ToolCall {
                id: pc.id,
                name: pc.name,
                arguments: args,
            }
        })
        .collect();

    tracing::info!("Cache stats: {} read, {} write, {} uncached",
        collected_cache_read, collected_cache_write,
        collected_input_tokens.saturating_sub(collected_cache_read + collected_cache_write));

    Ok((full_text, tool_calls, collected_input_tokens, collected_output_tokens, collected_cache_read, collected_cache_write))
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
    agent_id: Option<String>,
    provider: String,
    model: String,
    created_at: String,
    updated_at: String,
}

#[derive(Deserialize)]
struct ListConversationsQuery {
    agent_id: Option<String>,
}

async fn list_conversations(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListConversationsQuery>,
) -> Json<Vec<ConversationResponse>> {
    let db = state.db.clone();
    let agent_filter = query.agent_id.clone();
    let convs = db
        .with_conn(move |conn| {
            ChatEngine::list_conversations(conn, agent_filter.as_deref())
        })
        .await
        .unwrap_or_default();

    Json(
        convs
            .into_iter()
            .map(|c| ConversationResponse {
                id: c.id,
                title: c.title,
                agent_id: c.agent_id,
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
    // ── Ollama: local provider, no API key needed ──
    if provider_id == "ollama" {
        let data_dir = crate::storage::default_data_dir();
        let config = crate::config::AppConfig::load(&data_dir).unwrap_or_default();
        let ollama = OllamaProvider::new(config.ollama.base_url.clone());
        match ollama.list_ollama_models().await {
            Ok(models) => {
                let discovered: Vec<serde_json::Value> = models
                    .iter()
                    .map(|m| {
                        let supports_tools = OllamaProvider::model_supports_tools_static(&m.name, &m.details);
                        serde_json::json!({
                            "id": m.name,
                            "display_name": m.name,
                            "context_window": null,
                            "supports_tools": supports_tools,
                            "supports_streaming": true,
                            "supports_vision": false,
                            "size": m.size,
                            "family": m.details.as_ref().and_then(|d| d.family.clone()),
                            "parameter_size": m.details.as_ref().and_then(|d| d.parameter_size.clone()),
                            "quantization": m.details.as_ref().and_then(|d| d.quantization_level.clone()),
                        })
                    })
                    .collect();
                return Json(serde_json::json!({"models": discovered})).into_response();
            }
            Err(e) => {
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": format!("Cannot connect to Ollama: {}", e)})),
                )
                    .into_response();
            }
        }
    }

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

    // ── Hardcoded model lists for providers without dynamic discovery ──
    // These run BEFORE create_provider() so they work even without a full Provider impl.
    if provider_id == "openai" {
        return Json(serde_json::json!({"models": [
            {"id": "gpt-4.1", "display_name": "GPT-4.1", "context_window": 1047576, "supports_tools": true, "supports_streaming": true, "supports_vision": true},
            {"id": "gpt-4.1-mini", "display_name": "GPT-4.1 Mini", "context_window": 1047576, "supports_tools": true, "supports_streaming": true, "supports_vision": true},
            {"id": "gpt-4.1-nano", "display_name": "GPT-4.1 Nano", "context_window": 1047576, "supports_tools": true, "supports_streaming": true, "supports_vision": false},
            {"id": "o3", "display_name": "o3 (Reasoning)", "context_window": 200000, "supports_tools": true, "supports_streaming": true, "supports_vision": true},
            {"id": "o4-mini", "display_name": "o4-mini (Reasoning)", "context_window": 200000, "supports_tools": true, "supports_streaming": true, "supports_vision": true},
        ]})).into_response();
    }

    if provider_id == "google" {
        return Json(serde_json::json!({"models": [
            {"id": "gemini-3.1-pro-preview", "display_name": "Gemini 3.1 Pro (Preview)", "context_window": 1000000, "supports_tools": true, "supports_streaming": true, "supports_vision": true},
            {"id": "gemini-3-flash-preview", "display_name": "Gemini 3 Flash (Preview)", "context_window": 1000000, "supports_tools": true, "supports_streaming": true, "supports_vision": true},
            {"id": "gemini-2.5-pro", "display_name": "Gemini 2.5 Pro", "context_window": 1000000, "supports_tools": true, "supports_streaming": true, "supports_vision": true},
            {"id": "gemini-2.5-flash", "display_name": "Gemini 2.5 Flash", "context_window": 1000000, "supports_tools": true, "supports_streaming": true, "supports_vision": true},
            {"id": "gemini-2.5-flash-lite", "display_name": "Gemini 2.5 Flash Lite (Budget)", "context_window": 1000000, "supports_tools": true, "supports_streaming": true, "supports_vision": true},
        ]})).into_response();
    }

    // ── Dynamic discovery for providers with full implementations ──
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
            let mut obj = serde_json::json!({
                "name": d.name,
                "display_name": d.display_name,
                "description": d.description,
                "category": d.category,
                "has_instructions": d.instructions.is_some(),
            });
            if let Some(ref v) = d.vendor {
                obj["vendor"] = serde_json::json!(v);
            }
            obj
        })
        .collect();

    Json(serde_json::json!({"tools": tools}))
}

// ---------------------------------------------------------------------------
// Agents handlers
// ---------------------------------------------------------------------------

/// Returns the built-in Chitty system agent metadata (not stored in DB).
async fn get_system_agent() -> impl IntoResponse {
    Json(serde_json::json!({
        "id": "__chitty__",
        "name": "Chitty",
        "description": "System administrator & AI assistant — manages tools, packages, agents, providers, and local models",
        "is_system": true,
        "tools": [],
        "tags": ["system", "admin"],
        "max_iterations": 25,
        "approval_mode": "prompt"
    }))
}

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
    /// Agent persona (who the agent IS). Accepts "instructions" for backward compat.
    #[serde(alias = "instructions")]
    persona: String,
    /// Skills this agent uses. Accepts "tools" for backward compat.
    #[serde(alias = "tools")]
    skills: Vec<String>,
    #[serde(default)]
    project_path: Option<String>,
    #[serde(default)]
    preferred_provider: Option<String>,
    #[serde(default)]
    preferred_model: Option<String>,
    #[serde(default)]
    max_iterations: Option<u32>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default = "default_approval_mode_req")]
    approval_mode: String,
    // Context management
    #[serde(default)]
    context_budget_pct: Option<u32>,
    #[serde(default)]
    compaction_strategy: Option<String>,
    #[serde(default)]
    max_conversation_turns: Option<u32>,
}

fn default_approval_mode_req() -> String {
    "prompt".to_string()
}

async fn create_agent(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateAgentRequest>,
) -> impl IntoResponse {
    let agent = Agent {
        id: uuid::Uuid::new_v4().to_string(),
        name: req.name,
        description: req.description,
        persona: req.persona,
        skills: req.skills,
        project_path: req.project_path,
        preferred_provider: req.preferred_provider,
        preferred_model: req.preferred_model,
        tags: Vec::new(),
        version: "1.0".to_string(),
        ai_generated: false,
        max_iterations: req.max_iterations,
        temperature: req.temperature,
        max_tokens: req.max_tokens,
        approval_mode: req.approval_mode,
        context_budget_pct: req.context_budget_pct,
        compaction_strategy: req.compaction_strategy,
        max_conversation_turns: req.max_conversation_turns,
        package_id: None, // User-created agents have no package link
        parent_agent_id: None,
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
        persona: req.persona,
        skills: req.skills,
        project_path: req.project_path,
        preferred_provider: req.preferred_provider,
        preferred_model: req.preferred_model,
        tags: Vec::new(),
        version: "1.0".to_string(),
        ai_generated: false,
        max_iterations: req.max_iterations,
        temperature: req.temperature,
        max_tokens: req.max_tokens,
        approval_mode: req.approval_mode,
        context_budget_pct: req.context_budget_pct,
        compaction_strategy: req.compaction_strategy,
        max_conversation_turns: req.max_conversation_turns,
        package_id: None, // Preserved on update — package agents keep their link
        parent_agent_id: None,
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
// Sub-Agent API
// ---------------------------------------------------------------------------

async fn list_agent_children(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.clone();
    match db
        .with_conn(move |conn| AgentsManager::list_children(conn, &id))
        .await
    {
        Ok(children) => Json(children).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct CreateSubAgentRequest {
    name: String,
    description: String,
    persona: String,
    scoped_tools: Vec<SubAgentToolRequest>,
    preferred_provider: Option<String>,
    preferred_model: Option<String>,
}

#[derive(Deserialize)]
struct SubAgentToolRequest {
    tool_name: String,
    display_name: Option<String>,
    locked_params: serde_json::Value,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool { true }

async fn create_sub_agent(
    State(state): State<Arc<AppState>>,
    Path(parent_id): Path<String>,
    Json(req): Json<CreateSubAgentRequest>,
) -> impl IntoResponse {
    use crate::agents::SubAgentTool;

    let scoped_tools: Vec<SubAgentTool> = req.scoped_tools.iter().map(|t| SubAgentTool {
        id: String::new(), // Will be generated in create_sub_agent
        agent_id: String::new(),
        tool_name: t.tool_name.clone(),
        display_name: t.display_name.clone(),
        locked_params: t.locked_params.clone(),
        enabled: t.enabled,
    }).collect();

    let db = state.db.clone();
    let prov = req.preferred_provider.clone();
    let model = req.preferred_model.clone();
    match db
        .with_conn(move |conn| {
            AgentsManager::create_sub_agent(
                conn,
                &parent_id,
                &req.name,
                &req.description,
                &req.persona,
                &scoped_tools,
                prov,
                model,
            )
        })
        .await
    {
        Ok(agent) => Json(serde_json::json!({
            "ok": true,
            "id": agent.id,
            "name": agent.name,
            "parent_agent_id": agent.parent_agent_id,
        })).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Skills API
// ---------------------------------------------------------------------------

async fn list_skills(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let skills: Vec<crate::skills::SkillSummary> = state
        .skill_registry
        .list()
        .iter()
        .map(|s| crate::skills::SkillSummary::from(*s))
        .collect();

    Json(serde_json::json!(skills))
}

async fn get_skill(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.skill_registry.get(&name) {
        Some(skill) => {
            let content = state.skill_registry.load_skill_content(&name);
            Json(serde_json::json!({
                "name": skill.name,
                "description": skill.description,
                "allowed_tools": skill.allowed_tools,
                "source": skill.source.to_string(),
                "compatibility": skill.compatibility,
                "license": skill.license,
                "path": skill.skill_path.display().to_string(),
                "content": content,
            }))
            .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Skill '{}' not found", name)})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Context Compaction — keep conversation within context budget
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Context Management — model-aware compaction with pre-flight validation
// ---------------------------------------------------------------------------

/// Get the context window size (in tokens) for a given model.
/// Uses known provider defaults. User-configured context windows from user_models
/// table can override these when queried via the async path.
fn get_model_context_window(provider_id: &str, model_id: &str) -> u32 {
    match provider_id {
        "anthropic" => {
            if model_id.contains("sonnet-4-6") || model_id.contains("1m") {
                1_000_000
            } else {
                200_000 // claude-sonnet-4, claude-opus-4, claude-haiku-4.5
            }
        }
        "xai" => 131_072,   // grok models
        "openai" => {
            if model_id.contains("gpt-4o") {
                128_000
            } else if model_id.contains("o1") || model_id.contains("o3") {
                200_000
            } else {
                128_000
            }
        }
        "google" => 1_000_000, // gemini models
        "ollama" => 8_192,     // conservative default for local models
        _ => 128_000,          // safe fallback
    }
}

/// Truncation-based compaction (fast, no LLM call).
/// Preserves system prompt + last N messages, truncates older content.
fn truncate_compact(messages: &mut Vec<ChatMessage>, target_chars: usize) {
    if messages.len() <= 4 {
        return;
    }

    let preserve_last = 5.min(messages.len() - 1);
    let compact_end = messages.len() - preserve_last;

    // Pass 0: Immediately strip base64/screenshot data from ALL messages (even recent)
    // These are the #1 context bloaters — screenshots can be 100K+ chars each
    for msg in messages.iter_mut() {
        // Strip base64 image data
        if msg.content.contains("data:image/") || msg.content.contains("base64,") {
            if let Some(start) = msg.content.find("data:image/") {
                let preview = &msg.content[..start.min(200)];
                msg.content = format!("{}\n[screenshot/image data removed — {} chars]", preview, msg.content.len());
            }
        }
        // Strip very large JSON blobs in tool results (e.g., full HTML pages)
        if msg.role == "tool" && msg.content.len() > 10_000 {
            let safe_end = msg.content.char_indices().nth(2000).map(|(i, _)| i).unwrap_or(2000.min(msg.content.len()));
            msg.content = format!(
                "{}\n\n[... truncated: {} chars total, showing first 2000]",
                &msg.content[..safe_end],
                msg.content.len()
            );
        }
    }

    // Pass 1: Truncate older tool results and assistant messages
    for i in 1..compact_end {
        let msg = &mut messages[i];
        let content_len = msg.content.len();

        if msg.role == "tool" && content_len > 500 {
            let safe_end = msg.content.char_indices().nth(300).map(|(i, _)| i).unwrap_or(300.min(content_len));
            msg.content = format!(
                "{}\n\n[... compacted: {} chars total, showing first 300]",
                &msg.content[..safe_end],
                content_len
            );
        } else if msg.role == "assistant" && content_len > 1000 {
            let safe_end = msg.content.char_indices().nth(500).map(|(i, _)| i).unwrap_or(500.min(content_len));
            msg.content = format!(
                "{}\n\n[... compacted: {} chars total]",
                &msg.content[..safe_end],
                content_len
            );
        }
        // Also strip tool_calls from old assistant messages (they're huge JSON)
        if msg.role == "assistant" && i < compact_end {
            msg.tool_calls = None;
        }
    }

    // Pass 2: If still over budget, replace oldest tool results with placeholders
    let mut current_chars: usize = messages.iter().map(|m| m.content.len()).sum();
    if current_chars > target_chars && messages.len() > 6 {
        let mut i = 1;
        while current_chars > target_chars && i < messages.len() - preserve_last {
            if messages[i].role == "tool" {
                current_chars -= messages[i].content.len();
                messages[i].content = "[compacted — tool result removed to fit context]".to_string();
                current_chars += messages[i].content.len();
            }
            i += 1;
        }
    }

    // Pass 3: If STILL over budget, remove entire old message blocks
    let mut current_chars: usize = messages.iter().map(|m| m.content.len()).sum();
    if current_chars > target_chars && messages.len() > 6 {
        let mut i = 1;
        while current_chars > target_chars && i < messages.len() - preserve_last {
            current_chars -= messages[i].content.len();
            messages[i].content = "[compacted]".to_string();
            current_chars += 11;
            i += 1;
        }
    }
}

/// Summarize-based compaction — uses a fast LLM to summarize older messages.
/// Preserves key decisions, file paths, and context while dramatically reducing tokens.
async fn summarize_compact(
    messages: &mut Vec<ChatMessage>,
    state: &Arc<AppState>,
    provider_str: &str,
) {
    if messages.len() <= 4 {
        return;
    }

    let preserve_last = 5.min(messages.len() - 1);
    let compact_end = messages.len() - preserve_last;

    // Build the conversation text to summarize (skip system prompt at index 0)
    let mut conversation_text = String::new();
    for i in 1..compact_end {
        let msg = &messages[i];
        let role = &msg.role;
        // Truncate very large individual messages for the summary prompt itself
        let content = if msg.content.len() > 2000 {
            format!("{}... [truncated, {} chars total]", &msg.content[..msg.content.char_indices().nth(2000).map(|(i,_)|i).unwrap_or(2000)], msg.content.len())
        } else {
            msg.content.clone()
        };
        conversation_text.push_str(&format!("[{}]: {}\n\n", role, content));
    }

    if conversation_text.is_empty() {
        return;
    }

    // Pick a fast/cheap model for summarization based on provider
    let summary_model = match provider_str {
        "anthropic" => "claude-haiku-4-5-20251001",
        "xai" => "grok-3-mini-fast-beta",
        "openai" => "gpt-4o-mini",
        _ => {
            // Fallback to truncate if we can't determine a cheap model
            tracing::info!("No cheap model available for provider '{}', falling back to truncate", provider_str);
            truncate_compact(messages, messages.iter().map(|m| m.content.len()).sum::<usize>() / 2);
            return;
        }
    };

    let summary_prompt = format!(
        "Summarize this conversation history into a concise recap (max 500 words).\n\
         Preserve: key decisions made, file paths mentioned, tool results and outcomes, user preferences, important context.\n\
         Drop: verbose tool output, repeated attempts, intermediate steps, raw file contents.\n\
         Format as a clear, structured summary.\n\n\
         ---\n{}\n---",
        conversation_text
    );

    // Make the summarization LLM call
    let provider = match create_provider(state, provider_str).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("Failed to create provider for summarization: {}, falling back to truncate", e);
            truncate_compact(messages, messages.iter().map(|m| m.content.len()).sum::<usize>() / 2);
            return;
        }
    };

    let summary_messages = vec![ChatMessage {
        role: "user".to_string(),
        content: summary_prompt,
        tool_calls: None,
        tool_call_id: None,
    }];

    match provider.chat(summary_model, &summary_messages, None).await {
        Ok(response) => {
            let summary = response.content;
            tracing::info!("Context summarized: {} messages → {} char summary", compact_end - 1, summary.len());

            // Replace old messages with the summary
            messages.drain(1..compact_end);
            messages.insert(1, ChatMessage {
                role: "system".to_string(),
                content: format!("[Context Summary — earlier conversation summarized to fit context]\n\n{}", summary),
                tool_calls: None,
                tool_call_id: None,
            });

            // Clean up orphaned tool_result messages in the preserved set.
            // After draining, some preserved "tool" messages may reference tool_call_ids
            // whose corresponding "assistant" message (with tool_calls) was in the drained set.
            // The Anthropic API requires every tool_result to have a matching tool_use block.
            let mut tool_call_ids_in_assistants: std::collections::HashSet<String> = std::collections::HashSet::new();
            for msg in messages.iter() {
                if let Some(ref tcs) = msg.tool_calls {
                    for tc in tcs {
                        tool_call_ids_in_assistants.insert(tc.id.clone());
                    }
                }
            }
            messages.retain(|msg| {
                if msg.role == "tool" {
                    if let Some(ref tcid) = msg.tool_call_id {
                        return tool_call_ids_in_assistants.contains(tcid);
                    }
                }
                true // keep non-tool messages
            });

            // Also strip tool_calls from assistant messages if their tool_results were drained
            let tool_result_ids: std::collections::HashSet<String> = messages.iter()
                .filter(|m| m.role == "tool")
                .filter_map(|m| m.tool_call_id.clone())
                .collect();
            for msg in messages.iter_mut() {
                if let Some(ref mut tcs) = msg.tool_calls {
                    tcs.retain(|tc| tool_result_ids.contains(&tc.id));
                    if tcs.is_empty() {
                        msg.tool_calls = None;
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!("Summarization LLM call failed: {}, falling back to truncate", e);
            truncate_compact(messages, messages.iter().map(|m| m.content.len()).sum::<usize>() / 2);
        }
    }
}

/// Aggressive fallback compaction — keeps only system + last 3 messages.
/// Used when normal compaction isn't enough to fit the model's context window.
fn aggressive_compact(messages: &mut Vec<ChatMessage>) {
    let keep_last = 3.min(messages.len().saturating_sub(1));
    let compact_end = messages.len() - keep_last;
    if compact_end > 1 {
        let summary = "[Earlier conversation compacted to fit context window. Key context may have been lost. Consider starting a new conversation for complex tasks.]".to_string();
        messages.drain(1..compact_end);
        messages.insert(1, ChatMessage {
            role: "system".to_string(),
            content: summary,
            tool_calls: None,
            tool_call_id: None,
        });
    }
}

// ---------------------------------------------------------------------------
// Scheduled Tasks — CRUD for autonomous agent scheduling
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateScheduleRequest {
    name: String,
    #[serde(default)]
    agent_id: Option<String>,
    prompt: String,
    cron_expression: String,
    #[serde(default)]
    project_path: Option<String>,
    #[serde(default = "default_auto_approve")]
    auto_approve: bool,
}
fn default_auto_approve() -> bool { true }

#[derive(Serialize)]
struct ScheduleResponse {
    id: String,
    name: String,
    agent_id: Option<String>,
    prompt: String,
    cron_expression: String,
    project_path: Option<String>,
    enabled: bool,
    auto_approve: bool,
    last_run_at: Option<String>,
    next_run_at: Option<String>,
    created_at: String,
    updated_at: String,
}

async fn list_schedules(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let db = state.db.clone();
    let schedules = db
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, agent_id, prompt, cron_expression, project_path, \
                 enabled, auto_approve, last_run_at, next_run_at, created_at, updated_at \
                 FROM scheduled_tasks ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(ScheduleResponse {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    agent_id: row.get(2)?,
                    prompt: row.get(3)?,
                    cron_expression: row.get(4)?,
                    project_path: row.get(5)?,
                    enabled: row.get::<_, i32>(6)? != 0,
                    auto_approve: row.get::<_, i32>(7)? != 0,
                    last_run_at: row.get(8)?,
                    next_run_at: row.get(9)?,
                    created_at: row.get(10)?,
                    updated_at: row.get(11)?,
                })
            })?;
            let mut result = Vec::new();
            for row in rows {
                result.push(row?);
            }
            Ok(result)
        })
        .await
        .unwrap_or_default();

    Json(schedules)
}

async fn create_schedule(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateScheduleRequest>,
) -> impl IntoResponse {
    // Validate cron expression
    let full_expr = format!("0 {} *", req.cron_expression);
    if cron::Schedule::from_str(&full_expr).is_err() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "error": format!("Invalid cron expression: {}", req.cron_expression)
        }))).into_response();
    }

    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    // Compute initial next_run_at
    let next_run = cron::Schedule::from_str(&full_expr)
        .ok()
        .and_then(|s| s.upcoming(chrono::Local).next())
        .map(|dt| dt.to_rfc3339());

    let db = state.db.clone();
    let schedule_id = id.clone();
    let created = now.clone();
    let name_for_log = req.name.clone();
    let next_run_for_response = next_run.clone();
    match db
        .with_conn(move |conn| {
            conn.execute(
                "INSERT INTO scheduled_tasks (id, name, agent_id, prompt, cron_expression, project_path, enabled, auto_approve, next_run_at, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?9)",
                rusqlite::params![
                    schedule_id, req.name, req.agent_id, req.prompt, req.cron_expression,
                    req.project_path, req.auto_approve as i32, next_run, created
                ],
            )?;
            Ok(())
        })
        .await
    {
        Ok(()) => {
            tracing::info!("Created scheduled task '{}' ({})", name_for_log, id);
            Json(serde_json::json!({ "id": id, "name": name_for_log, "next_run_at": next_run_for_response })).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response()
        }
    }
}

async fn get_schedule(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.clone();
    match db
        .with_conn(move |conn| {
            let s = conn.query_row(
                "SELECT id, name, agent_id, prompt, cron_expression, project_path, \
                 enabled, auto_approve, last_run_at, next_run_at, created_at, updated_at \
                 FROM scheduled_tasks WHERE id = ?1",
                rusqlite::params![id],
                |row| {
                    Ok(ScheduleResponse {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        agent_id: row.get(2)?,
                        prompt: row.get(3)?,
                        cron_expression: row.get(4)?,
                        project_path: row.get(5)?,
                        enabled: row.get::<_, i32>(6)? != 0,
                        auto_approve: row.get::<_, i32>(7)? != 0,
                        last_run_at: row.get(8)?,
                        next_run_at: row.get(9)?,
                        created_at: row.get(10)?,
                        updated_at: row.get(11)?,
                    })
                },
            )?;
            Ok(s)
        })
        .await
    {
        Ok(schedule) => Json(serde_json::json!(schedule)).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
struct UpdateScheduleRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    cron_expression: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    auto_approve: Option<bool>,
}

async fn update_schedule(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<UpdateScheduleRequest>,
) -> impl IntoResponse {
    let db = state.db.clone();
    let now = chrono::Utc::now().to_rfc3339();

    // If cron changed, validate and recompute next_run
    let new_next_run = if let Some(ref cron_expr) = req.cron_expression {
        let full_expr = format!("0 {} *", cron_expr);
        match cron::Schedule::from_str(&full_expr) {
            Ok(s) => s.upcoming(chrono::Local).next().map(|dt| dt.to_rfc3339()),
            Err(_) => {
                return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                    "error": format!("Invalid cron expression: {}", cron_expr)
                }))).into_response();
            }
        }
    } else {
        None
    };

    match db
        .with_conn(move |conn| {
            if let Some(name) = req.name {
                conn.execute("UPDATE scheduled_tasks SET name = ?1, updated_at = ?2 WHERE id = ?3",
                    rusqlite::params![name, now, id])?;
            }
            if let Some(prompt) = req.prompt {
                conn.execute("UPDATE scheduled_tasks SET prompt = ?1, updated_at = ?2 WHERE id = ?3",
                    rusqlite::params![prompt, now, id])?;
            }
            if let Some(cron_expr) = req.cron_expression {
                conn.execute("UPDATE scheduled_tasks SET cron_expression = ?1, next_run_at = ?2, updated_at = ?3 WHERE id = ?4",
                    rusqlite::params![cron_expr, new_next_run, now, id])?;
            }
            if let Some(enabled) = req.enabled {
                conn.execute("UPDATE scheduled_tasks SET enabled = ?1, updated_at = ?2 WHERE id = ?3",
                    rusqlite::params![enabled as i32, now, id])?;
            }
            if let Some(auto_approve) = req.auto_approve {
                conn.execute("UPDATE scheduled_tasks SET auto_approve = ?1, updated_at = ?2 WHERE id = ?3",
                    rusqlite::params![auto_approve as i32, now, id])?;
            }
            Ok(())
        })
        .await
    {
        Ok(()) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

async fn delete_schedule(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let db = state.db.clone();
    match db
        .with_conn(move |conn| {
            conn.execute("DELETE FROM scheduled_tasks WHERE id = ?1", rusqlite::params![id])?;
            Ok(())
        })
        .await
    {
        Ok(()) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

async fn run_schedule_now(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Load the task and trigger it
    let db = state.db.clone();
    let task = db
        .with_conn(move |conn| {
            let t = conn.query_row(
                "SELECT id, name, agent_id, prompt, cron_expression, project_path, enabled, auto_approve, last_run_at, next_run_at \
                 FROM scheduled_tasks WHERE id = ?1",
                rusqlite::params![id],
                |row| {
                    Ok(crate::scheduler::ScheduledTask {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        agent_id: row.get(2)?,
                        prompt: row.get(3)?,
                        cron_expression: row.get(4)?,
                        project_path: row.get(5)?,
                        enabled: row.get::<_, i32>(6)? != 0,
                        auto_approve: row.get::<_, i32>(7)? != 0,
                        last_run_at: row.get(8)?,
                        next_run_at: row.get(9)?,
                    })
                },
            )?;
            Ok(t)
        })
        .await;

    match task {
        Ok(task) => {
            let task_state = state.clone();
            tokio::spawn(async move {
                if let Err(e) = crate::scheduler::execute_scheduled_task(task_state, task).await {
                    tracing::error!("Manual schedule run failed: {}", e);
                }
            });
            Json(serde_json::json!({ "success": true, "message": "Task triggered" })).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

use std::str::FromStr;

// ---------------------------------------------------------------------------
// Agent Builder — AI-powered agent generation with agent loop
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AgentBuilderRequest {
    message: String,
    #[serde(default)]
    history: Vec<BuilderMessage>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
struct BuilderMessage {
    role: String,
    content: String,
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
                "description": "List all native tools currently available in Chitty Workspace (file_reader, file_writer, terminal, code_search, code_analyzer, etc.). Returns name, display name, description, and category for each tool.",
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
                "name": "list_marketplace_packages",
                "description": "List installed marketplace packages with their tools, auth requirements, configuration options, and agent setup hints. These are community-developed, security-reviewed packages.",
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
                "name": "check_package_status",
                "description": "Check the authentication and configuration status of a specific marketplace package. Returns whether it is authenticated, configured, and what setup steps remain.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "package_name": {
                            "type": "string",
                            "description": "The package name (e.g., 'google-cloud', 'web-tools')"
                        }
                    },
                    "required": ["package_name"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "ask_user_questions",
                "description": "Present ALL your questions to the user at once as interactive cards. The user answers each one sequentially, then all answers are returned together in a single message. This saves tokens by avoiding multiple round-trips. Always batch all your questions into ONE call. First option in each question should be your recommended choice.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "questions": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "question": { "type": "string", "description": "The question to ask" },
                                    "options": {
                                        "type": "array",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "label": { "type": "string", "description": "Short label (2-5 words)" },
                                                "description": { "type": "string", "description": "Brief explanation" }
                                            },
                                            "required": ["label", "description"]
                                        },
                                        "minItems": 2,
                                        "maxItems": 4,
                                        "description": "2-4 options per question. First = recommended."
                                    }
                                },
                                "required": ["question", "options"]
                            },
                            "minItems": 1,
                            "maxItems": 6,
                            "description": "1-6 questions to ask. All presented sequentially, answers returned together."
                        }
                    },
                    "required": ["questions"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "update_agent_draft",
                "description": "Update a field on the agent being built. The user sees changes in real-time on the preview panel. Call this incrementally as you discuss and agree on parts of the agent with the user.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "field": {
                            "type": "string",
                            "enum": ["name", "description", "instructions", "add_tool", "remove_tool", "add_package", "remove_package", "tags", "max_iterations", "temperature"],
                            "description": "Which field to update"
                        },
                        "value": {
                            "type": "string",
                            "description": "The value to set. For add_tool/remove_tool: tool name. For add_package/remove_package: package name. For tags: comma-separated. For max_iterations/temperature: numeric string."
                        }
                    },
                    "required": ["field", "value"]
                }
            }
        }),
    ]
}

/// Execute an agent-builder tool call and return the result string.
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
        "list_marketplace_packages" => {
            let rt = state.tool_runtime.read().await;
            let packages: Vec<serde_json::Value> = rt.list_marketplace_packages()
                .iter()
                .map(|pkg| {
                    // Load tool details from each tool's manifest.json
                    let tools: Vec<serde_json::Value> = pkg.manifest.tools.iter().map(|tool_name| {
                        let tool_dir = pkg.dir.join(tool_name);
                        let manifest_path = tool_dir.join("manifest.json");
                        if let Ok(content) = std::fs::read_to_string(&manifest_path) {
                            if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&content) {
                                return serde_json::json!({
                                    "name": manifest.get("name").and_then(|n| n.as_str()).unwrap_or(tool_name),
                                    "display_name": manifest.get("display_name").and_then(|n| n.as_str()).unwrap_or(tool_name),
                                    "description": manifest.get("description").and_then(|n| n.as_str()).unwrap_or(""),
                                });
                            }
                        }
                        serde_json::json!({ "name": tool_name, "display_name": tool_name, "description": "" })
                    }).collect();

                    // Build configurable_resources summary
                    let resources: Vec<serde_json::Value> = pkg.manifest.configurable_resources.iter().map(|r| {
                        serde_json::json!({ "id": r.id, "label": r.label, "description": r.description })
                    }).collect();

                    // Build feature_flags summary
                    let features: Vec<serde_json::Value> = pkg.manifest.feature_flags.iter().map(|f| {
                        serde_json::json!({ "id": f.id, "label": f.label, "default_enabled": f.default_enabled })
                    }).collect();

                    // Build agent_config
                    let ac = &pkg.manifest.agent_config;
                    let agent_config = serde_json::json!({
                        "default_instructions": ac.default_instructions,
                        "suggested_prompts": ac.suggested_prompts,
                        "recommended_model": ac.recommended_model,
                        "capabilities": ac.capabilities,
                    });

                    serde_json::json!({
                        "name": pkg.manifest.name,
                        "display_name": pkg.manifest.display_name,
                        "description": pkg.manifest.description,
                        "version": pkg.manifest.version,
                        "auth_type": pkg.manifest.auth.as_ref().map(|a| a.auth_type.as_str()).unwrap_or("none"),
                        "tools": tools,
                        "has_config": !pkg.manifest.configurable_resources.is_empty() || !pkg.manifest.feature_flags.is_empty(),
                        "configurable_resources": resources,
                        "feature_flags": features,
                        "agent_config": agent_config,
                        "setup_steps_count": pkg.manifest.setup_steps.len(),
                    })
                })
                .collect();
            drop(rt);
            serde_json::to_string_pretty(&packages).unwrap_or_default()
        }
        "check_package_status" => {
            let package_name = args.get("package_name").and_then(|v| v.as_str()).unwrap_or("");

            // Clone the package data we need while the lock is held
            let pkg_data = {
                let rt = state.tool_runtime.read().await;
                rt.list_marketplace_packages()
                    .into_iter()
                    .find(|p| p.manifest.name == package_name)
                    .map(|p| (
                        p.manifest.setup_steps.clone(),
                        p.manifest.configurable_resources.len(),
                        p.manifest.feature_flags.len(),
                    ))
            };

            match pkg_data {
                Some((setup_steps, resource_count, flag_count)) => {
                    // Check setup steps (auth status)
                    let mut all_ok = true;
                    let mut step_results = Vec::new();
                    for step in &setup_steps {
                        if let Some(check_cmd) = &step.check_command {
                            let result = run_shell_command(check_cmd).await;
                            step_results.push(serde_json::json!({
                                "step_id": step.id,
                                "label": step.label,
                                "ok": result.success,
                                "help_text": step.help_text,
                            }));
                            if !result.success && step.required {
                                all_ok = false;
                            }
                        }
                    }

                    // Check if config exists in DB
                    let db = state.db.clone();
                    let pkg_name = package_name.to_string();
                    let config = db.with_conn(move |conn| {
                        Ok(load_package_config(conn, &pkg_name))
                    }).await.unwrap_or_else(|_| serde_json::json!({ "resources": {}, "features": {} }));
                    let has_saved_config = config.get("resources")
                        .and_then(|r| r.as_object())
                        .map(|r| !r.is_empty())
                        .unwrap_or(false);

                    serde_json::to_string_pretty(&serde_json::json!({
                        "package": package_name,
                        "authenticated": all_ok,
                        "configured": has_saved_config,
                        "has_config": resource_count > 0 || flag_count > 0,
                        "steps": step_results,
                    })).unwrap_or_default()
                }
                None => {
                    serde_json::json!({
                        "package": package_name,
                        "error": format!("Package '{}' not found", package_name)
                    }).to_string()
                }
            }
        }
        "ask_user_question" => {
            // Return the question data as JSON — the frontend renders it as an interactive card.
            // The LLM receives this as the tool result and should stop to wait for the user's answer
            // which comes as the next message in the conversation.
            let question = args.get("question").and_then(|v| v.as_str()).unwrap_or("");
            let options = args.get("options").cloned().unwrap_or(serde_json::json!([]));
            serde_json::json!({
                "type": "question",
                "question": question,
                "options": options,
                "status": "waiting_for_user",
                "instruction": "The question has been displayed to the user as an interactive card. STOP here. Do NOT ask another question or continue. Wait for the user's next message which will contain their selection."
            }).to_string()
        }
        "update_agent_draft" => {
            let field = args.get("field").and_then(|v| v.as_str()).unwrap_or("unknown");
            let value = args.get("value").and_then(|v| v.as_str()).unwrap_or("");
            // Return JSON so the client can parse field/value and update the preview panel.
            let msg = match field {
                "name" => format!("Updated agent name to '{}'", value),
                "description" => "Updated agent description".to_string(),
                "instructions" => format!("Updated agent instructions ({} chars)", value.len()),
                "add_tool" => format!("Added tool '{}' to agent", value),
                "remove_tool" => format!("Removed tool '{}' from agent", value),
                "add_package" => format!("Added marketplace package '{}' to agent", value),
                "remove_package" => format!("Removed marketplace package '{}' from agent", value),
                "tags" => format!("Updated tags to: {}", value),
                "max_iterations" => format!("Set max iterations to {}", value),
                "temperature" => format!("Set temperature to {}", value),
                _ => format!("Updated {} = {}", field, value),
            };
            serde_json::json!({
                "message": msg,
                "field": field,
                "value": value,
            }).to_string()
        }
        _ => format!("Unknown tool: {}", tool_name),
    }
}

/// Build the system prompt for the agent builder agent.
fn build_agent_builder_prompt() -> String {
    r#"You are the Agent Builder for Chitty Workspace, a local-first AI assistant.

Your job is to have a conversation with the user to understand what they need, then collaboratively design an agent. You build the agent incrementally by calling `update_agent_draft` as you discuss and agree on parts of the design.

## Your Process

1. FIRST, call `list_system_tools` and `list_marketplace_packages` to see what is available.
2. Based on what the user asked for, use `ask_user_question` to ask ONE clarifying question at a time. Wait for the answer before asking the next question. Each question should have 3-4 clickable options with the recommended option listed first.
3. When recommending a marketplace package, call `check_package_status` to verify it is set up. Warn the user if authentication or configuration is needed (they can complete setup from the preview panel).
4. Use `update_agent_draft` to incrementally set the agent's name, description, instructions, tools, and packages as you agree on them with the user.
5. Continue the conversation — refine the agent based on user feedback.

## Important Rules

- ALWAYS use `ask_user_question` when you need user input. NEVER ask questions as plain text. Questions must be presented as interactive cards with clickable options.
- Ask ONE question at a time. Wait for the user's answer before presenting the next question.
- The first option in each question should be your recommended choice. Mark it clearly.
- NEVER try to create custom tools. All tools come from native system tools or installed marketplace packages.
- Tools from marketplace packages are real, community-developed, security-reviewed integrations.
- Keep text responses brief. Use `ask_user_question` for decisions, use `update_agent_draft` for building.
- Do NOT output raw JSON. Use tool calls for questions and draft updates.

## update_agent_draft Fields

Call `update_agent_draft` with these field values:
- `name` — Short agent name (2-5 words)
- `description` — One-sentence summary
- `instructions` — Detailed system prompt defining the agent's role, approach, constraints, and quality standards. Do NOT include tool documentation — that is injected automatically.
- `add_tool` / `remove_tool` — Add or remove a native system tool by name
- `add_package` / `remove_package` — Add or remove a marketplace package by name (this includes all its tools)
- `tags` — Comma-separated categorization tags
- `max_iterations` — Tool call rounds: 5 for simple Q&A, 10 for standard, 20-25 for complex multi-step
- `temperature` — 0.0-0.3 for precise/coding, 0.7-1.0 for creative

## Conversation Guidelines

1. On the first message, call `list_system_tools` and `list_marketplace_packages` to understand what's available. Then use `ask_user_question` for your first clarifying question.
2. After each answer, either ask the next question with `ask_user_question` or call `update_agent_draft` to build parts of the agent.
3. When adding a marketplace package, always call `check_package_status` first so you can tell the user if setup is needed.
4. Write thorough instructions — be specific about the agent's persona, approach, constraints, and quality standards.
5. After setting up the initial draft, use `ask_user_question` to ask if the user wants any changes before they save.
6. Keep text responses SHORT (1-2 sentences max). The interactive question cards do the heavy lifting."#.to_string()
}

/// Core agent builder processing — conversational agent loop with tool calls.
async fn process_agent_builder(
    state: Arc<AppState>,
    req: AgentBuilderRequest,
    sse_tx: mpsc::Sender<StreamChunk>,
) -> anyhow::Result<()> {
    // 1. Resolve default agent
    let (_provider_id, model_id, provider) = resolve_default_agent(&state).await?;

    let system_prompt = build_agent_builder_prompt();
    let builder_tools = agent_builder_tools();

    // 2. Build messages from conversation history
    let mut current_messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: system_prompt,
            tool_calls: None,
            tool_call_id: None,
        },
    ];

    // Add conversation history from previous turns
    for msg in &req.history {
        current_messages.push(ChatMessage {
            role: msg.role.clone(),
            content: msg.content.clone(),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    // Add the current user message
    current_messages.push(ChatMessage {
        role: "user".to_string(),
        content: req.message,
        tool_calls: None,
        tool_call_id: None,
    });

    let max_iterations: u32 = 10;

    // 3. Agent loop — process tool calls within this turn
    for iteration in 1..=max_iterations {
        let tools_for_call = if iteration == max_iterations {
            None // force text-only on last iteration
        } else {
            Some(builder_tools.as_slice())
        };

        // Stream from LLM — use interceptor to also capture reasoning/thinking text
        let (intercept_tx, mut intercept_rx) = mpsc::channel::<StreamChunk>(64);
        let sse_tx_clone = sse_tx.clone();

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

        let (full_text, tool_calls, _iter_input, _iter_output, _cache_r, _cache_w) = stream_and_collect(
            provider.as_ref(),
            &model_id,
            &current_messages,
            tools_for_call,
            &intercept_tx,
        )
        .await?;
        drop(intercept_tx);
        let _reasoning_text = interceptor.await.unwrap_or_default();

        // No tool calls → conversational response (end of this turn)
        if tool_calls.is_empty() {
            // Text was already streamed to client via SSE text events.
            // No JSON parsing needed — the builder is conversational.
            tracing::info!("Agent builder response ({} chars)", full_text.len());
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
                "categories": pkg.manifest.categories,
                "long_description": pkg.manifest.long_description,
                "setup_steps": setup_steps,
                "tools": tools,
                "has_config": !pkg.manifest.configurable_resources.is_empty() || !pkg.manifest.feature_flags.is_empty(),
                "tool_count": pkg.manifest.tools.len(),
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
            // Check all setup steps — check_commands + credential keys in keyring
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
                } else if let Some(cred_key) = &step.credentials_key {
                    // Check if credential exists in OS keyring
                    let has_key = crate::config::get_api_key(cred_key)
                        .map(|v| v.is_some())
                        .unwrap_or(false);
                    step_results.push(serde_json::json!({
                        "step_id": step.id,
                        "label": step.label,
                        "ok": has_key,
                    }));
                    if !has_key && step.required {
                        all_ok = false;
                    }
                } else if step.id == "oauth_connect" {
                    // Check OAuth status
                    if let Some(provider) = pkg.manifest.auth.as_ref().and_then(|a| a.oauth_provider.as_ref()) {
                        let connected = crate::config::get_api_key(&format!("oauth_{}_access_token", provider))
                            .map(|v| v.is_some())
                            .unwrap_or(false);
                        step_results.push(serde_json::json!({
                            "step_id": step.id,
                            "label": step.label,
                            "ok": connected,
                        }));
                        if !connected && step.required {
                            all_ok = false;
                        }
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

    let mode = body.get("mode").and_then(|v| v.as_str()).unwrap_or("run_all");
    let target_step = body.get("step_id").and_then(|v| v.as_str()).unwrap_or("");

    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut all_success = true;

    for step in &pkg.manifest.setup_steps {
        // In execute_step mode, skip steps that aren't the target
        if mode == "execute_step" && step.id != target_step {
            continue;
        }
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

        // In check_only mode, don't execute anything — just report status
        if mode == "check_only" {
            results.push(serde_json::json!({
                "step_id": step_id,
                "label": label,
                "status": "pending",
            }));
            all_success = false;
            continue;
        }

        // 2a. If step has credentials_key, check keyring first, then store if provided
        if let Some(cred_key) = &step.credentials_key {
            // Check if already stored in keyring
            if let Ok(Some(_)) = crate::config::get_api_key(cred_key) {
                results.push(serde_json::json!({
                    "step_id": step_id,
                    "label": label,
                    "status": "already_done",
                    "message": "Credential already saved",
                }));
                continue;
            }
            if let Some(val) = user_values.get(step_id).and_then(|v| v.as_str()) {
                if !val.is_empty() {
                    match crate::config::set_api_key(cred_key, val) {
                        Ok(()) => {
                            tracing::info!("Package setup [{}]: stored credential '{}'", step_id, cred_key);
                            results.push(serde_json::json!({
                                "step_id": step_id,
                                "label": label,
                                "status": "completed",
                                "message": "Credential saved securely",
                            }));
                            continue;
                        }
                        Err(e) => {
                            results.push(serde_json::json!({
                                "step_id": step_id,
                                "label": label,
                                "status": "failed",
                                "message": format!("Failed to save credential: {}", e),
                            }));
                            if step.required { all_success = false; break; }
                            continue;
                        }
                    }
                }
            }
            // No value provided — prompt user
            results.push(serde_json::json!({
                "step_id": step_id,
                "label": label,
                "status": "needs_input",
                "prompt_label": step.prompt_label,
                "prompt_placeholder": step.prompt_placeholder,
                "prompt_help": step.prompt_help,
                "message": "Credential required",
            }));
            all_success = false;
            continue;
        }

        // 2b. Determine the install command
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

/// POST /api/marketplace/packages/:vendor/disconnect — Clear OAuth tokens for a package
async fn disconnect_package_auth(
    Path(vendor): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let rt = state.tool_runtime.read().await;
    let pkg = rt.list_marketplace_packages()
        .into_iter()
        .find(|p| p.manifest.name == vendor)
        .cloned();
    drop(rt);

    let pkg = match pkg {
        Some(p) => p,
        None => return Json(serde_json::json!({ "success": false, "error": "Package not found" })),
    };

    // If package has an OAuth provider, disconnect it
    if let Some(provider) = pkg.manifest.auth.as_ref().and_then(|a| a.oauth_provider.as_deref()) {
        // Clear tokens from keyring
        let keys = [
            format!("oauth_{}_access_token", provider),
            format!("oauth_{}_refresh_token", provider),
            format!("oauth_{}_expires_at", provider),
            format!("oauth_{}_scopes", provider),
        ];
        for key in &keys {
            let _ = crate::config::delete_api_key(key);
        }
        tracing::info!("Disconnected OAuth for package '{}' (provider: {})", vendor, provider);
        Json(serde_json::json!({ "success": true, "message": format!("Disconnected {}", provider) }))
    } else {
        Json(serde_json::json!({ "success": false, "error": "Package has no OAuth provider" }))
    }
}

struct CommandResult {
    success: bool,
    stdout: String,
    stderr: String,
}

async fn run_shell_command(cmd: &str) -> CommandResult {
    let shell = if cfg!(target_os = "windows") { "cmd" } else { "sh" };
    let flag = if cfg!(target_os = "windows") { "/C" } else { "-c" };

    // Extend PATH with common tool locations (gcloud, python, node, etc.)
    let mut path_env = std::env::var("PATH").unwrap_or_default();
    if cfg!(target_os = "windows") {
        let extra_paths = [
            r"C:\Program Files (x86)\Google\Cloud SDK\google-cloud-sdk\bin",
            r"C:\Program Files\Google\Cloud SDK\google-cloud-sdk\bin",
            r"C:\Users\Default\AppData\Local\Google\Cloud SDK\google-cloud-sdk\bin",
        ];
        for p in &extra_paths {
            if std::path::Path::new(p).exists() && !path_env.contains(p) {
                path_env = format!("{};{}", p, path_env);
            }
        }
    }

    match tokio::process::Command::new(shell)
        .arg(flag)
        .arg(cmd)
        .env("PATH", &path_env)
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
// Package Details & Configuration handlers
// ---------------------------------------------------------------------------

/// Full package details — manifest + tools + config + auth status
async fn get_package_details(
    Path(vendor): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let rt = state.tool_runtime.read().await;
    let pkg = rt.list_marketplace_packages()
        .into_iter()
        .find(|p| p.manifest.name == vendor)
        .cloned();
    drop(rt);

    let pkg = match pkg {
        Some(p) => p,
        None => return Json(serde_json::json!({ "error": format!("Package '{}' not found", vendor) })),
    };

    // Read tool manifests
    let tools: Vec<serde_json::Value> = pkg.manifest.tools.iter().map(|tool_name| {
        let tool_dir = pkg.dir.join(tool_name);
        let manifest_path = tool_dir.join("manifest.json");
        if let Ok(content) = std::fs::read_to_string(&manifest_path) {
            if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&content) {
                return manifest;
            }
        }
        serde_json::json!({ "name": tool_name, "display_name": tool_name })
    }).collect();

    // Load saved config from DB
    let db = state.db.clone();
    let vendor_clone = vendor.clone();
    let config = db.with_conn(move |conn| {
        Ok(load_package_config(conn, &vendor_clone))
    }).await.unwrap_or_else(|_| serde_json::json!({ "resources": {}, "features": {} }));

    // Serialize the full manifest (includes new fields)
    let manifest_json = serde_json::to_value(&pkg.manifest).unwrap_or_default();

    Json(serde_json::json!({
        "package": manifest_json,
        "tools": tools,
        "config": config,
    }))
}

/// Get saved configuration for a package (allowed resources + feature flags)
async fn get_package_config(
    Path(vendor): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let db = state.db.clone();
    let config = db.with_conn(move |conn| {
        Ok(load_package_config(conn, &vendor))
    }).await.unwrap_or_else(|_| serde_json::json!({ "resources": {}, "features": {} }));
    Json(config)
}

/// Save configuration for a package
async fn save_package_config(
    Path(vendor): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let db = state.db.clone();
    let result = db.with_conn(move |conn| {
        // Ensure the package record exists
        conn.execute(
            "INSERT OR IGNORE INTO marketplace_packages (id, name, display_name, vendor, version, status)
             VALUES (?1, ?1, ?1, '', '', 'installed')",
            rusqlite::params![vendor],
        )?;

        // Save resources
        if let Some(resources) = body.get("resources").and_then(|r| r.as_object()) {
            for (resource_type, items) in resources {
                // Clear existing resources of this type
                conn.execute(
                    "DELETE FROM package_resources WHERE package_id = ?1 AND resource_type = ?2",
                    rusqlite::params![vendor, resource_type],
                )?;

                // Insert new ones
                if let Some(arr) = items.as_array() {
                    for item in arr {
                        let resource_id = item.get("id").and_then(|v| v.as_str())
                            .or_else(|| item.as_str())
                            .unwrap_or_default();
                        let display_name = item.get("display_name").and_then(|v| v.as_str()).map(|s| s.to_string());
                        let config_str = item.get("config").map(|v| v.to_string()).unwrap_or_else(|| "{}".to_string());

                        if !resource_id.is_empty() {
                            let id = format!("res-{}-{}-{}", vendor, resource_type, resource_id);
                            conn.execute(
                                "INSERT OR REPLACE INTO package_resources (id, package_id, resource_type, resource_id, display_name, config)
                                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                                rusqlite::params![id, vendor, resource_type, resource_id, display_name, config_str],
                            )?;
                        }
                    }
                }
            }
        }

        // Save feature flags
        if let Some(features) = body.get("features").and_then(|f| f.as_object()) {
            for (feature_id, enabled) in features {
                let enabled_int: i32 = if enabled.as_bool().unwrap_or(false) { 1 } else { 0 };
                let id = format!("feat-{}-{}", vendor, feature_id);
                conn.execute(
                    "INSERT OR REPLACE INTO package_features (id, package_id, feature_id, enabled, updated_at)
                     VALUES (?1, ?2, ?3, ?4, datetime('now'))",
                    rusqlite::params![id, vendor, feature_id, enabled_int],
                )?;
            }
        }

        Ok(serde_json::json!({ "success": true }))
    }).await;

    // Refresh cached package configs in the tool runtime
    if let Ok(ref v) = result {
        if v.get("success").and_then(|s| s.as_bool()).unwrap_or(false) {
            state.tool_runtime.write().await.load_package_configs(&state.db);
        }
    }

    match result {
        Ok(v) => Json(v),
        Err(e) => Json(serde_json::json!({ "success": false, "error": format!("{}", e) })),
    }
}

/// Discover available resources for a configurable resource type
async fn discover_package_resources(
    Path(vendor): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let resource_type = body.get("resource_type").and_then(|v| v.as_str()).unwrap_or_default().to_string();

    let rt = state.tool_runtime.read().await;
    let pkg = rt.list_marketplace_packages()
        .into_iter()
        .find(|p| p.manifest.name == vendor)
        .cloned();
    drop(rt);

    let pkg = match pkg {
        Some(p) => p,
        None => return Json(serde_json::json!({ "success": false, "error": "Package not found" })),
    };

    // Find the configurable resource definition
    let resource_def = pkg.manifest.configurable_resources.iter()
        .find(|r| r.id == resource_type);

    let discover_cmd = match resource_def.and_then(|r| r.discover_command.as_deref()) {
        Some(cmd) => cmd.to_string(),
        None => return Json(serde_json::json!({
            "success": true,
            "resources": [],
            "message": "No discover command configured — enter resource names manually"
        })),
    };

    let result = run_shell_command(&discover_cmd).await;

    if !result.success {
        return Json(serde_json::json!({
            "success": false,
            "error": format!("Discovery failed: {}", result.stderr.chars().take(300).collect::<String>()),
        }));
    }

    // Try to parse as JSON array, fallback to line-separated
    let resources: Vec<serde_json::Value> = if let Ok(parsed) = serde_json::from_str::<Vec<serde_json::Value>>(&result.stdout) {
        parsed
    } else {
        // Parse line-by-line (e.g., gsutil ls output like "gs://bucket-name/")
        result.stdout.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let name = l.trim().trim_start_matches("gs://").trim_end_matches('/');
                serde_json::json!({ "id": name, "name": name })
            })
            .collect()
    };

    Json(serde_json::json!({
        "success": true,
        "resources": resources,
    }))
}

/// Load package config (resources + features) from the database
fn load_package_config(conn: &rusqlite::Connection, package_id: &str) -> serde_json::Value {
    // Load resources
    let mut resources: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT resource_type, resource_id, display_name, config FROM package_resources WHERE package_id = ?1"
    ) {
        if let Ok(rows) = stmt.query_map(rusqlite::params![package_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        }) {
            for row in rows.flatten() {
                let (rtype, rid, display_name, config_str) = row;
                let config: serde_json::Value = serde_json::from_str(&config_str).unwrap_or_default();
                resources.entry(rtype).or_default().push(serde_json::json!({
                    "id": rid,
                    "display_name": display_name,
                    "config": config,
                }));
            }
        }
    }

    // Load features
    let mut features: HashMap<String, bool> = HashMap::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT feature_id, enabled FROM package_features WHERE package_id = ?1"
    ) {
        if let Ok(rows) = stmt.query_map(rusqlite::params![package_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i32>(1)?))
        }) {
            for row in rows.flatten() {
                features.insert(row.0, row.1 != 0);
            }
        }
    }

    serde_json::json!({
        "resources": resources,
        "features": features,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Marketplace Registry handlers (remote — browse/search/install from chitty.ai)
// ---------------------------------------------------------------------------

async fn registry_list_packages() -> impl IntoResponse {
    let client = crate::tools::MarketplaceClient::new();
    match client.list_packages().await {
        Ok(packages) => Json(serde_json::json!({ "packages": packages })),
        Err(e) => {
            tracing::error!("Registry list failed: {}", e);
            Json(serde_json::json!({
                "packages": [],
                "error": format!("Failed to connect to marketplace: {}", e),
            }))
        }
    }
}

async fn registry_search(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let query = params.get("q").cloned().unwrap_or_default();
    let client = crate::tools::MarketplaceClient::new();
    match client.search(&query).await {
        Ok(packages) => Json(serde_json::json!({ "packages": packages })),
        Err(e) => {
            tracing::error!("Registry search failed: {}", e);
            Json(serde_json::json!({
                "packages": [],
                "error": format!("Search failed: {}", e),
            }))
        }
    }
}

async fn registry_install_package(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let name = match body.get("name").and_then(|n| n.as_str()) {
        Some(n) => n.to_string(),
        None => return Json(serde_json::json!({ "success": false, "error": "Missing 'name'" })),
    };
    let version = body.get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("latest")
        .to_string();

    let rt = state.tool_runtime.read().await;
    let marketplace_dir = rt.tools_dir().join("marketplace");
    drop(rt);

    let client = crate::tools::MarketplaceClient::new();

    // If version is "latest", fetch package detail to get the actual latest version
    let actual_version = if version == "latest" {
        match client.get_package(&name).await {
            Ok(detail) => detail.versions.last()
                .map(|v| v.version.clone())
                .unwrap_or_else(|| "1.0.0".to_string()),
            Err(e) => return Json(serde_json::json!({
                "success": false,
                "error": format!("Failed to fetch package info: {}", e),
            })),
        }
    } else {
        version
    };

    match client.install_package(&name, &actual_version, &marketplace_dir).await {
        Ok(pkg_dir) => {
            // Re-scan to pick up the new tools
            state.tool_runtime.write().await.scan_and_load();

            tracing::info!("Installed {}@{} from registry to {:?}", name, actual_version, pkg_dir);
            Json(serde_json::json!({
                "success": true,
                "message": format!("Installed {}@{}", name, actual_version),
                "path": pkg_dir.to_string_lossy(),
            }))
        }
        Err(e) => {
            tracing::error!("Registry install failed for {}@{}: {}", name, actual_version, e);
            Json(serde_json::json!({
                "success": false,
                "error": format!("Installation failed: {}", e),
            }))
        }
    }
}

// ---------------------------------------------------------------------------
// Local models — GPU, Ollama, sidecar
// ---------------------------------------------------------------------------

/// GET /api/local/gpu — GPU statistics
async fn local_gpu_handler() -> impl IntoResponse {
    let stats = crate::gpu::get_gpu_stats().await;
    Json(serde_json::json!(stats))
}

/// GET /api/local/status — Combined local status (GPU + Ollama)
async fn local_status_handler(
    State(_state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let gpu = crate::gpu::get_gpu_stats().await;

    // Check Ollama status
    let data_dir = crate::storage::default_data_dir();
    let config = crate::config::AppConfig::load(&data_dir).unwrap_or_default();
    let ollama = OllamaProvider::new(config.ollama.base_url.clone());
    let ollama_status = ollama.check_status().await;

    let ollama_model_count = if ollama_status.running {
        ollama.list_ollama_models().await.map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };

    Json(serde_json::json!({
        "gpu": gpu,
        "ollama": {
            "available": ollama_status.running,
            "version": ollama_status.version,
            "model_count": ollama_model_count,
            "error": ollama_status.error,
        },
        "sidecar": {
            "running": false,
            "loaded_model": null,
        }
    }))
}

/// GET /api/local/ollama/status
async fn ollama_status_handler() -> impl IntoResponse {
    let data_dir = crate::storage::default_data_dir();
    let config = crate::config::AppConfig::load(&data_dir).unwrap_or_default();
    let ollama = OllamaProvider::new(config.ollama.base_url.clone());
    let status = ollama.check_status().await;
    Json(serde_json::json!(status))
}

/// GET /api/local/ollama/models
async fn ollama_models_handler() -> impl IntoResponse {
    let data_dir = crate::storage::default_data_dir();
    let config = crate::config::AppConfig::load(&data_dir).unwrap_or_default();
    let ollama = OllamaProvider::new(config.ollama.base_url.clone());

    match ollama.list_ollama_models().await {
        Ok(models) => Json(serde_json::json!({ "models": models })),
        Err(e) => Json(serde_json::json!({ "error": e.to_string(), "models": [] })),
    }
}

/// GET /api/local/ollama/running
async fn ollama_running_handler() -> impl IntoResponse {
    let data_dir = crate::storage::default_data_dir();
    let config = crate::config::AppConfig::load(&data_dir).unwrap_or_default();
    let ollama = OllamaProvider::new(config.ollama.base_url.clone());

    match ollama.running_models().await {
        Ok(models) => Json(serde_json::json!({ "models": models })),
        Err(e) => Json(serde_json::json!({ "error": e.to_string(), "models": [] })),
    }
}

/// POST /api/local/ollama/pull — Pull/download a model (streaming progress)
async fn ollama_pull_handler(
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let model_name = match body.get("name").and_then(|n| n.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return Json(serde_json::json!({ "success": false, "error": "Missing 'name'" }))
        }
    };

    let data_dir = crate::storage::default_data_dir();
    let config = crate::config::AppConfig::load(&data_dir).unwrap_or_default();
    let ollama = OllamaProvider::new(config.ollama.base_url.clone());

    match ollama.pull_model(&model_name).await {
        Ok(result) => Json(serde_json::json!({
            "success": true,
            "result": result,
        })),
        Err(e) => Json(serde_json::json!({
            "success": false,
            "error": e.to_string(),
        })),
    }
}

/// POST /api/local/ollama/unload — Unload a model from VRAM
async fn ollama_unload_handler(
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let model_name = match body.get("name").and_then(|n| n.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return Json(serde_json::json!({ "success": false, "error": "Missing 'name'" }))
        }
    };

    let data_dir = crate::storage::default_data_dir();
    let config = crate::config::AppConfig::load(&data_dir).unwrap_or_default();
    let ollama = OllamaProvider::new(config.ollama.base_url.clone());

    match ollama.unload_model(&model_name).await {
        Ok(()) => Json(serde_json::json!({ "success": true })),
        Err(e) => Json(serde_json::json!({ "success": false, "error": e.to_string() })),
    }
}

/// DELETE /api/local/ollama/models/:name — Delete a model
async fn ollama_delete_model_handler(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> impl IntoResponse {
    let data_dir = crate::storage::default_data_dir();
    let config = crate::config::AppConfig::load(&data_dir).unwrap_or_default();
    let ollama = OllamaProvider::new(config.ollama.base_url.clone());

    match ollama.delete_model(&name).await {
        Ok(()) => Json(serde_json::json!({ "success": true })),
        Err(e) => Json(serde_json::json!({ "success": false, "error": e.to_string() })),
    }
}

// ---------------------------------------------------------------------------
// HuggingFace Sidecar handlers
// ---------------------------------------------------------------------------

/// GET /api/local/sidecar/status
async fn sidecar_status_handler() -> impl IntoResponse {
    let data_dir = crate::storage::default_data_dir();
    let config = crate::config::AppConfig::load(&data_dir).unwrap_or_default();
    let base_url = format!("http://127.0.0.1:{}", config.huggingface.sidecar_port);

    let status = crate::huggingface::check_status(&base_url).await;

    // Also check if sidecar script is installed
    let installed = crate::huggingface::is_sidecar_installed(&data_dir);
    let python_found = crate::huggingface::find_python(&data_dir).is_some();

    Json(serde_json::json!({
        "running": status.running,
        "loaded_model": status.loaded_model,
        "models_registered": status.models_registered,
        "vram_free_mb": status.vram_free_mb,
        "sidecar_installed": installed,
        "python_found": python_found,
        "error": status.error,
    }))
}

/// POST /api/local/sidecar/start — Start the Python sidecar process
async fn sidecar_start_handler() -> impl IntoResponse {
    let data_dir = crate::storage::default_data_dir();
    let config = crate::config::AppConfig::load(&data_dir).unwrap_or_default();

    // Find Python
    let python = match crate::huggingface::find_python(&data_dir) {
        Some(p) => p,
        None => {
            return Json(serde_json::json!({
                "success": false,
                "error": "Python not found. Install Python 3.10+ and ensure it's on PATH."
            }));
        }
    };

    // Find sidecar script
    let script = match crate::huggingface::find_sidecar_script(&data_dir) {
        Some(s) => s,
        None => {
            return Json(serde_json::json!({
                "success": false,
                "error": "Sidecar script not found. Ensure inference_server.py is in the sidecar directory."
            }));
        }
    };

    // Collect model directories
    let mut extra_dirs: Vec<String> = Vec::new();
    if let Some(ref dir) = config.huggingface.models_dir {
        extra_dirs.push(dir.clone());
    }

    // Start sidecar
    match crate::huggingface::start_sidecar(
        &python,
        &script,
        config.huggingface.sidecar_port,
        &extra_dirs,
    )
    .await
    {
        Ok(_child) => {
            // Note: we don't store the child handle here — in production,
            // store it in AppState for clean shutdown. For now, it runs detached.
            Json(serde_json::json!({
                "success": true,
                "port": config.huggingface.sidecar_port,
                "python": python.display().to_string(),
            }))
        }
        Err(e) => Json(serde_json::json!({
            "success": false,
            "error": e.to_string(),
        })),
    }
}

/// POST /api/local/sidecar/stop — Stop the sidecar (kill process on port)
async fn sidecar_stop_handler() -> impl IntoResponse {
    let data_dir = crate::storage::default_data_dir();
    let config = crate::config::AppConfig::load(&data_dir).unwrap_or_default();
    let port = config.huggingface.sidecar_port;

    // Kill process on the sidecar port
    #[cfg(target_os = "windows")]
    {
        let _ = tokio::process::Command::new("cmd")
            .args(["/C", &format!("for /f \"tokens=5\" %a in ('netstat -aon ^| findstr :{port}') do taskkill /PID %a /F")])
            .output()
            .await;
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = tokio::process::Command::new("sh")
            .args(["-c", &format!("lsof -ti:{port} | xargs kill -9")])
            .output()
            .await;
    }

    Json(serde_json::json!({ "success": true }))
}

/// GET /api/local/sidecar/models — List models from running sidecar
async fn sidecar_models_handler() -> impl IntoResponse {
    let data_dir = crate::storage::default_data_dir();
    let config = crate::config::AppConfig::load(&data_dir).unwrap_or_default();
    let base_url = format!("http://127.0.0.1:{}", config.huggingface.sidecar_port);

    match crate::huggingface::list_models(&base_url).await {
        Ok(models) => Json(serde_json::json!({ "models": models })),
        Err(e) => Json(serde_json::json!({ "error": e.to_string(), "models": [] })),
    }
}

/// POST /api/local/sidecar/models/scan — Re-scan model directories
async fn sidecar_scan_handler() -> impl IntoResponse {
    let data_dir = crate::storage::default_data_dir();
    let config = crate::config::AppConfig::load(&data_dir).unwrap_or_default();
    let base_url = format!("http://127.0.0.1:{}", config.huggingface.sidecar_port);

    match crate::huggingface::scan_models(&base_url).await {
        Ok(result) => Json(result),
        Err(e) => Json(serde_json::json!({ "success": false, "error": e.to_string() })),
    }
}

/// POST /api/local/sidecar/models/register — Register a GGUF file
async fn sidecar_register_handler(
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let path = match body.get("path").and_then(|p| p.as_str()) {
        Some(p) => p.to_string(),
        None => return Json(serde_json::json!({ "success": false, "error": "Missing 'path'" })),
    };
    let name = body.get("name").and_then(|n| n.as_str());

    let data_dir = crate::storage::default_data_dir();
    let config = crate::config::AppConfig::load(&data_dir).unwrap_or_default();
    let base_url = format!("http://127.0.0.1:{}", config.huggingface.sidecar_port);

    match crate::huggingface::register_model(&base_url, &path, name).await {
        Ok(result) => Json(result),
        Err(e) => Json(serde_json::json!({ "success": false, "error": e.to_string() })),
    }
}

/// POST /api/local/sidecar/models/load — Load a model into GPU
async fn sidecar_load_handler(
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let model = match body.get("model").and_then(|m| m.as_str()) {
        Some(m) => m.to_string(),
        None => return Json(serde_json::json!({ "success": false, "error": "Missing 'model'" })),
    };
    let gpu_layers = body.get("gpu_layers").and_then(|v| v.as_i64()).map(|v| v as i32);
    let context_length = body.get("context_length").and_then(|v| v.as_u64()).map(|v| v as u32);

    let data_dir = crate::storage::default_data_dir();
    let config = crate::config::AppConfig::load(&data_dir).unwrap_or_default();
    let base_url = format!("http://127.0.0.1:{}", config.huggingface.sidecar_port);

    match crate::huggingface::load_model(&base_url, &model, gpu_layers, context_length).await {
        Ok(result) => Json(result),
        Err(e) => Json(serde_json::json!({ "success": false, "error": e.to_string() })),
    }
}

/// POST /api/local/sidecar/models/unload — Unload model from GPU
async fn sidecar_unload_handler() -> impl IntoResponse {
    let data_dir = crate::storage::default_data_dir();
    let config = crate::config::AppConfig::load(&data_dir).unwrap_or_default();
    let base_url = format!("http://127.0.0.1:{}", config.huggingface.sidecar_port);

    match crate::huggingface::unload_model(&base_url).await {
        Ok(result) => Json(result),
        Err(e) => Json(serde_json::json!({ "success": false, "error": e.to_string() })),
    }
}

/// GET /api/local/gguf/scan — Scan for GGUF files locally (no sidecar needed)
/// Scans ~/.chitty-workspace/models/ and configured extra directories
async fn gguf_scan_local_handler() -> impl IntoResponse {
    let data_dir = crate::storage::default_data_dir();
    let config = crate::config::AppConfig::load(&data_dir).unwrap_or_default();

    let mut dirs_to_scan: Vec<std::path::PathBuf> = vec![data_dir.join("models")];
    if let Some(ref dir) = config.huggingface.models_dir {
        dirs_to_scan.push(std::path::PathBuf::from(dir));
    }

    let mut models: Vec<serde_json::Value> = Vec::new();
    for dir in &dirs_to_scan {
        if !dir.exists() {
            let _ = std::fs::create_dir_all(dir);
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("gguf") {
                    let name = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    models.push(serde_json::json!({
                        "name": name,
                        "path": path.display().to_string(),
                        "filename": path.file_name().unwrap_or_default().to_string_lossy(),
                        "size_bytes": size,
                        "size_gb": (size as f64) / (1024.0 * 1024.0 * 1024.0),
                        "directory": dir.display().to_string(),
                    }));
                }
            }
        }
    }

    Json(serde_json::json!({
        "models": models,
        "directories_scanned": dirs_to_scan.iter().map(|d| d.display().to_string()).collect::<Vec<_>>(),
    }))
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

// ═══════════════════════════════════════════════════════════════════════════
// Persistent Connections API
// ═══════════════════════════════════════════════════════════════════════════

/// List all declared connections across marketplace packages with their runtime status.
async fn list_connections_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let mut connections = Vec::new();

    // Scan packages for connection declarations
    let packages = {
        let rt = state.tool_runtime.read().await;
        rt.list_marketplace_packages()
            .into_iter()
            .filter(|pkg| !pkg.manifest.connections.is_empty())
            .map(|pkg| pkg.manifest.clone())
            .collect::<Vec<_>>()
    };

    let mgr = state.connection_manager.read().await;

    for manifest in &packages {
        for conn_def in &manifest.connections {
            let key = format!("{}:{}", manifest.name, conn_def.id);
            let active = mgr.connections.get(&key);

            let status = active
                .map(|a| a.status.to_string())
                .unwrap_or_else(|| "stopped".to_string());

            let error = active.and_then(|a| a.error_message.clone());

            connections.push(serde_json::json!({
                "package_id": manifest.name,
                "package_name": manifest.display_name,
                "connection_id": conn_def.id,
                "label": conn_def.label,
                "description": conn_def.description,
                "status": status,
                "error": error,
                "requires_feature": conn_def.requires_feature,
                "events": conn_def.events.iter().map(|e| serde_json::json!({
                    "id": e.id,
                    "label": e.label,
                    "description": e.description,
                    "agent_configurable": e.agent_configurable,
                })).collect::<Vec<_>>(),
            }));
        }
    }

    axum::Json(serde_json::json!({ "connections": connections }))
}

/// Start a specific connection.
async fn start_connection_handler(
    State(state): State<Arc<AppState>>,
    Path((package_id, conn_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let mut mgr = state.connection_manager.write().await;
    mgr.start_connection(&package_id, &conn_id).await;
    axum::Json(serde_json::json!({"ok": true, "message": format!("Connection {}:{} starting", package_id, conn_id)}))
}

/// Stop a specific connection.
async fn stop_connection_handler(
    State(state): State<Arc<AppState>>,
    Path((package_id, conn_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let key = format!("{}:{}", package_id, conn_id);
    let mut mgr = state.connection_manager.write().await;
    mgr.stop_connection(&key).await;
    axum::Json(serde_json::json!({"ok": true, "message": format!("Connection {} stopped", key)}))
}

/// List all event route configurations.
async fn list_connection_routes_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let db = state.db.clone();
    let routes = db.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id, package_id, connection_id, event_id, agent_id, provider, model, auto_approve, enabled
             FROM connection_event_routes"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "package_id": row.get::<_, String>(1)?,
                "connection_id": row.get::<_, String>(2)?,
                "event_id": row.get::<_, String>(3)?,
                "agent_id": row.get::<_, Option<String>>(4)?,
                "provider": row.get::<_, Option<String>>(5)?,
                "model": row.get::<_, Option<String>>(6)?,
                "auto_approve": row.get::<_, bool>(7)?,
                "enabled": row.get::<_, bool>(8)?,
            }))
        })?;
        let result: Vec<_> = rows.filter_map(|r| r.ok()).collect();
        Ok(result)
    }).await.unwrap_or_default();

    axum::Json(serde_json::json!({ "routes": routes }))
}

/// Set (or create) an event route — maps a connection event to an agent.
async fn set_connection_route_handler(
    State(state): State<Arc<AppState>>,
    Path((package_id, conn_id, event_id)): Path<(String, String, String)>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    let agent_id = body.get("agent_id").and_then(|v| v.as_str()).map(|s| s.to_string());
    let provider = body.get("provider").and_then(|v| v.as_str()).map(|s| s.to_string());
    let model = body.get("model").and_then(|v| v.as_str()).map(|s| s.to_string());
    let enabled = body.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);

    let db = state.db.clone();
    let id = format!("{}:{}:{}", package_id, conn_id, event_id);

    let result = db.with_conn(move |conn| {
        conn.execute(
            "INSERT INTO connection_event_routes (id, package_id, connection_id, event_id, agent_id, provider, model, enabled, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, datetime('now'))
             ON CONFLICT(package_id, connection_id, event_id) DO UPDATE SET
                 agent_id = ?5,
                 provider = ?6,
                 model = ?7,
                 enabled = ?8,
                 updated_at = datetime('now')",
            rusqlite::params![id, package_id, conn_id, event_id, agent_id, provider, model, enabled],
        )?;
        Ok(())
    }).await;

    match result {
        Ok(()) => axum::Json(serde_json::json!({"ok": true})),
        Err(e) => axum::Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

/// Dispatch tasks to package agents (parallel or sequential).
/// Each task runs as a mini LLM conversation with the target agent's persona + tools.
async fn run_dispatch(
    state: &Arc<AppState>,
    args: &serde_json::Value,
    sse_tx: &tokio::sync::mpsc::Sender<crate::providers::StreamChunk>,
) -> serde_json::Value {
    let tasks = match args.get("tasks").and_then(|t| t.as_array()) {
        Some(tasks) => tasks.clone(),
        None => return serde_json::json!({"error": "tasks array is required"}),
    };
    let mode = args.get("mode").and_then(|m| m.as_str()).unwrap_or("parallel");

    // Resolve agents
    let agents_list = {
        let conn = match state.db.connect() {
            Ok(c) => c,
            Err(e) => return serde_json::json!({"error": format!("DB error: {}", e)}),
        };
        match crate::agents::AgentsManager::list(&conn) {
            Ok(list) => list,
            Err(e) => return serde_json::json!({"error": format!("Failed to list agents: {}", e)}),
        }
    };

    let mut results = Vec::new();

    if mode == "sequential" {
        // Sequential: run one at a time, accumulate context
        let mut prior_results = String::new();
        for task in &tasks {
            let agent_name = task.get("agent").and_then(|a| a.as_str()).unwrap_or("");
            let mut instruction = task.get("instruction").and_then(|i| i.as_str()).unwrap_or("").to_string();
            if !prior_results.is_empty() {
                instruction = format!("{}\n\nContext from prior agents:\n{}", instruction, prior_results);
            }

            let result = dispatch_single_agent(state, &agents_list, agent_name, &instruction, sse_tx).await;
            let response = result.get("response").and_then(|r| r.as_str()).unwrap_or("(no response)");
            prior_results.push_str(&format!("[{}]: {}\n", agent_name, response));
            results.push(result);
        }
    } else {
        // Parallel: dispatch all at once
        let mut handles = Vec::new();
        for task in &tasks {
            let agent_name = task.get("agent").and_then(|a| a.as_str()).unwrap_or("").to_string();
            let instruction = task.get("instruction").and_then(|i| i.as_str()).unwrap_or("").to_string();
            let state = state.clone();
            let agents = agents_list.clone();
            let tx = sse_tx.clone();

            handles.push(tokio::spawn(async move {
                dispatch_single_agent(&state, &agents, &agent_name, &instruction, &tx).await
            }));
        }

        for handle in handles {
            match handle.await {
                Ok(result) => results.push(result),
                Err(e) => results.push(serde_json::json!({"status": "error", "error": e.to_string()})),
            }
        }
    }

    serde_json::json!({ "results": results })
}

/// Execute a single agent dispatch — runs a mini LLM conversation with the agent's tools.
/// Streams agent activity (thinking, tool calls, results) through the parent SSE channel.
async fn dispatch_single_agent(
    state: &Arc<AppState>,
    agents: &[crate::agents::AgentSummary],
    agent_name: &str,
    instruction: &str,
    parent_sse: &tokio::sync::mpsc::Sender<crate::providers::StreamChunk>,
) -> serde_json::Value {
    // Find the agent by name or ID (case-insensitive)
    let agent_match = agents.iter().find(|a| {
        a.name.eq_ignore_ascii_case(agent_name)
            || a.id.eq_ignore_ascii_case(agent_name)
            || a.package_id.as_deref().map(|p| p.eq_ignore_ascii_case(agent_name)).unwrap_or(false)
    });

    let agent_summary = match agent_match {
        Some(a) => a,
        None => return serde_json::json!({
            "agent": agent_name,
            "status": "error",
            "error": format!("No agent found matching '{}'", agent_name)
        }),
    };

    // Load full agent
    let agent = {
        let conn = match state.db.connect() {
            Ok(c) => c,
            Err(e) => return serde_json::json!({"agent": agent_name, "status": "error", "error": e.to_string()}),
        };
        match crate::agents::AgentsManager::load(&conn, &agent_summary.id) {
            Ok(Some(a)) => a,
            _ => return serde_json::json!({"agent": agent_name, "status": "error", "error": "Agent not found"}),
        }
    };

    // Get this agent's tools (for package agents: the package's tools)
    let all_tool_defs = {
        let rt = state.tool_runtime.read().await;
        rt.list_definitions()
    };

    // Filter to this agent's package tools + base tools
    let agent_tools: Vec<serde_json::Value> = if let Some(ref pkg_id) = agent.package_id {
        let rt = state.tool_runtime.read().await;
        // Collect tool names from package manifest (kebab→snake)
        let mut pkg_tool_names: Vec<String> = rt.marketplace_packages.iter()
            .filter(|p| p.manifest.name == *pkg_id)
            .flat_map(|p| p.manifest.tools.iter())
            .map(|t| t.replace('-', "_"))
            .collect();
        drop(rt);

        // Also match by vendor field on tool definitions (handles prefix mismatches like
        // package lists "bigquery" but manifest name is "gcloud_bigquery" with vendor "google-cloud")
        for d in &all_tool_defs {
            if let Some(ref vendor) = d.vendor {
                let vendor_lower = vendor.to_lowercase();
                let pkg_lower = pkg_id.to_lowercase();
                if vendor_lower == pkg_lower
                    || vendor_lower == pkg_lower.replace('-', "_")
                    || vendor_lower == pkg_lower.replace("google-", "")
                {
                    pkg_tool_names.push(d.name.clone());
                }
            }
        }
        pkg_tool_names.sort();
        pkg_tool_names.dedup();

        all_tool_defs.iter()
            .filter(|d| pkg_tool_names.contains(&d.name) || ["load_skill", "save_memory", "file_reader"].contains(&d.name.as_str()))
            .map(|d| serde_json::json!({
                "type": "function",
                "function": {
                    "name": d.name,
                    "description": d.description,
                    "parameters": d.parameters,
                }
            }))
            .collect()
    } else {
        // Non-package agent: all tools
        all_tool_defs.iter().map(|d| serde_json::json!({
            "type": "function",
            "function": {
                "name": d.name,
                "description": d.description,
                "parameters": d.parameters,
            }
        })).collect()
    };

    // Build messages with EXECUTION MODE directive
    let exec_persona = format!(
        "{}\n\n## EXECUTION MODE\n\
        You are in execution mode — dispatched by the Chitty orchestrator to complete a specific task.\n\
        - Execute the requested task IMMEDIATELY using your tools\n\
        - Do NOT ask for confirmation or clarification\n\
        - Do NOT explain what you're about to do — just do it\n\
        - Call the appropriate tool(s) right away\n\
        - Return a concise summary of what you did and the results\n\
        - If a tool fails, report the error concisely\n\
        - Do NOT call dispatch_agents — you cannot sub-dispatch",
        agent.persona
    );
    let messages = vec![
        crate::providers::ChatMessage {
            role: "system".to_string(),
            content: exec_persona,
            tool_calls: None,
            tool_call_id: None,
        },
        crate::providers::ChatMessage {
            role: "user".to_string(),
            content: instruction.to_string(),
            tool_calls: None,
            tool_call_id: None,
        },
    ];

    // Create provider for the agent
    let provider_str = agent.preferred_provider.as_deref()
        .unwrap_or("anthropic");
    let provider = match create_provider(state, provider_str).await {
        Ok(p) => p,
        Err(e) => return serde_json::json!({
            "agent": agent.name,
            "status": "error",
            "error": format!("Provider error: {}", e)
        }),
    };

    let model = agent.preferred_model.clone()
        .unwrap_or_else(|| "claude-sonnet-4-20250514".to_string());

    // Tool call loop — runs up to max_iterations, executing tools and feeding results back
    let max_iterations = agent.max_iterations.unwrap_or(10) as usize;
    let mut current_messages = messages;
    let mut tool_trace: Vec<serde_json::Value> = Vec::new();
    let mut final_text = String::new();
    let display_name = agent.name.clone();
    let agent_icon = agent.package_id.as_deref().unwrap_or("📦").to_string();

    // Stream: Agent started
    let _ = parent_sse.send(StreamChunk::AgentStart {
        agent_name: display_name.clone(),
        agent_icon: agent_icon.clone(),
        instruction: instruction.to_string(),
    }).await;

    for iteration in 0..max_iterations {
        // Call LLM
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::providers::StreamChunk>(256);
        let chat_result = provider.chat_stream(&model, &current_messages, Some(&agent_tools), tx).await;
        if let Err(e) = chat_result {
            return serde_json::json!({"agent": display_name, "status": "error", "error": format!("Chat error: {}", e)});
        }

        // Collect response and stream agent text to parent
        let mut text_parts = String::new();
        let mut tool_calls: Vec<crate::providers::ToolCall> = Vec::new();
        let mut current_tc: Option<(String, String, String)> = None;

        while let Some(chunk) = rx.recv().await {
            match chunk {
                crate::providers::StreamChunk::Text(t) => {
                    // Stream agent text to parent SSE in real-time
                    let _ = parent_sse.send(StreamChunk::AgentText {
                        agent_name: display_name.clone(),
                        text: t.clone(),
                    }).await;
                    text_parts.push_str(&t);
                }
                crate::providers::StreamChunk::ToolCallStart { id, name } => {
                    current_tc = Some((id, name, String::new()));
                }
                crate::providers::StreamChunk::ToolCallDelta { arguments, .. } => {
                    if let Some((_, _, ref mut buf)) = current_tc {
                        buf.push_str(&arguments);
                    }
                }
                crate::providers::StreamChunk::ToolCallEnd { .. } => {
                    if let Some((id, name, args_str)) = current_tc.take() {
                        let arguments = serde_json::from_str(&args_str).unwrap_or(serde_json::json!({}));
                        // Stream: Agent is calling a tool
                        let _ = parent_sse.send(StreamChunk::AgentToolCall {
                            agent_name: display_name.clone(),
                            tool_name: name.clone(),
                            tool_args: arguments.clone(),
                        }).await;
                        tool_calls.push(crate::providers::ToolCall { id, name, arguments });
                    }
                }
                crate::providers::StreamChunk::Error(e) => {
                    return serde_json::json!({"agent": display_name, "status": "error", "error": e});
                }
                _ => {}
            }
        }

        // If no tool calls, we're done
        if tool_calls.is_empty() {
            final_text = text_parts;
            break;
        }

        // Add assistant message with tool calls to conversation
        current_messages.push(crate::providers::ChatMessage {
            role: "assistant".to_string(),
            content: text_parts,
            tool_calls: Some(tool_calls.clone()),
            tool_call_id: None,
        });

        // Execute each tool call
        for tc in &tool_calls {
            tracing::info!("  Dispatch[{}] iter={}: tool={} args={}", display_name, iteration, tc.name, tc.arguments);
            let tool_runtime = state.tool_runtime.read().await;
            let ctx = crate::tools::ToolContext {
                working_dir: std::env::current_dir().unwrap_or_default(),
                conversation_id: "dispatch".to_string(),
                db: state.db.clone(),
            };
            let (result, dur) = tool_runtime.execute(&tc.name, &tc.arguments, &ctx).await;
            drop(tool_runtime);

            let result_content = result.as_content_string();
            let result_preview = if result_content.len() > 300 {
                format!("{}...", &result_content[..300])
            } else {
                result_content.clone()
            };

            // Stream: Agent tool result
            let _ = parent_sse.send(StreamChunk::AgentToolResult {
                agent_name: display_name.clone(),
                tool_name: tc.name.clone(),
                success: result.success,
                result_preview: result_preview.clone(),
                duration_ms: dur,
            }).await;

            tool_trace.push(serde_json::json!({
                "tool": tc.name,
                "args": tc.arguments,
                "success": result.success,
                "duration_ms": dur,
                "result_preview": result_preview,
            }));

            current_messages.push(crate::providers::ChatMessage {
                role: "tool".to_string(),
                content: result_content,
                tool_calls: None,
                tool_call_id: Some(tc.id.clone()),
            });
        }
    }

    // Stream: Agent completed
    let _ = parent_sse.send(StreamChunk::AgentComplete {
        agent_name: display_name.clone(),
        response: if final_text.len() > 500 { format!("{}...", &final_text[..500]) } else { final_text.clone() },
    }).await;

    serde_json::json!({
        "agent": display_name,
        "status": "success",
        "response": final_text,
        "tool_calls": tool_trace,
    })
}
