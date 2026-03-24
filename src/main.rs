mod chat;
mod config;
mod connections;
mod gpu;
mod huggingface;
mod integrations;
mod oauth;
mod providers;
mod scheduler;
mod server;
mod agents;
mod skills;
mod storage;
mod tls;
mod tools;
mod ui;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "chitty-workspace")]
#[command(about = "Chitty Workspace - Local AI assistant with agents, tools, and BYOK providers")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Handle a chitty:// protocol URL (e.g. chitty://install/web-tools)
    #[arg(long = "protocol", hide = true)]
    protocol_url: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start Chitty Workspace (default)
    Run,
    /// Show current configuration
    Config,
    /// List installed agents
    Agents,
    /// Test chat from CLI (headless — no UI, just terminal output)
    Test {
        /// Message to send to the LLM
        #[arg(default_value = "Hello, what can you do?")]
        message: String,
        /// Provider to use (xai, anthropic, openai)
        #[arg(short, long, default_value = "xai")]
        provider: String,
        /// Model to use
        #[arg(short, long)]
        model: Option<String>,
        /// Agent ID to use
        #[arg(short = 'a', long)]
        agent: Option<String>,
    },
    /// Test agent builder from CLI
    TestAgentBuilder {
        /// Description for the agent to generate
        description: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Set up logging — write to file for reliable debugging on Windows
    let data_dir = crate::storage::default_data_dir();
    let log_path = data_dir.join("chitty.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
        .expect("Failed to open log file");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("chitty_workspace=info".parse()?)
                .add_directive("chitty_workspace::server=debug".parse()?),
        )
        .with_writer(std::sync::Mutex::new(log_file))
        .with_ansi(false)
        .init();

    tracing::info!("Log file: {}", log_path.display());

    let cli = Cli::parse();

    // Handle chitty:// protocol URLs (launched by clicking Install on chitty.ai)
    if let Some(url) = &cli.protocol_url {
        return handle_protocol_url(url).await;
    }

    match cli.command.unwrap_or(Commands::Run) {
        Commands::Run => {
            tracing::info!("Starting Chitty Workspace v{}", env!("CARGO_PKG_VERSION"));

            // Register chitty:// protocol handler (Windows)
            #[cfg(target_os = "windows")]
            register_protocol_handler();

            // Initialize storage
            let data_dir = storage::default_data_dir();
            tracing::info!("Data directory: {:?}", data_dir);
            let db = storage::Database::new(&data_dir)?;

            // Load config (creates default if missing)
            let _config = config::AppConfig::load(&data_dir)?;

            // Browser bridge — connects to the Chitty Browser Extension via WebSocket
            let browser_bridge = std::sync::Arc::new(server::BrowserBridge::new());

            // Create skill registry (discovers SKILL.md files from all paths)
            let skill_registry = std::sync::Arc::new(skills::SkillRegistry::new(&data_dir, None));

            // Create tool registry (native tools) and runtime (native + custom + connections)
            let tool_registry = std::sync::Arc::new(tools::ToolRegistry::new(browser_bridge.clone(), skill_registry.clone()));
            let tool_runtime = match tools::ToolRuntime::new(&data_dir, browser_bridge.clone(), skill_registry.clone()) {
                Ok(rt) => std::sync::Arc::new(tokio::sync::RwLock::new(rt)),
                Err(e) => {
                    tracing::error!("Failed to initialize tool runtime: {}", e);
                    std::sync::Arc::new(tokio::sync::RwLock::new(
                        tools::ToolRuntime::new(&data_dir, browser_bridge.clone(), skill_registry.clone()).expect("Tool runtime init failed")
                    ))
                }
            };
            {
                let rt = tool_runtime.read().await;
                let defs = rt.list_definitions();
                tracing::info!("Registered {} tools ({} native + custom/connections)",
                    defs.len(), tool_registry.list_definitions().len());
            }

            // Start the HTTP server in the background
            let port: u16 = 8770;
            let bound_port = std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0));
            let server_db = db.clone();
            let server_tools = tool_registry.clone();
            let server_runtime = tool_runtime.clone();
            let server_bridge = browser_bridge.clone();
            let server_skills = skill_registry.clone();
            let bp = bound_port.clone();
            tokio::spawn(async move {
                if let Err(e) = server::start(server_db, server_tools, server_runtime, server_bridge, server_skills, port, bp).await {
                    tracing::error!("Server error: {}", e);
                }
            });

            // Poll for server readiness (up to 5 seconds)
            // Wait for the server to store its actual bound port
            let mut actual_port = port;
            let mut ready = false;
            for attempt in 0..100 {
                let stored = bound_port.load(std::sync::atomic::Ordering::SeqCst);
                if stored > 0 {
                    actual_port = stored;
                    // Verify we can connect
                    if tokio::net::TcpStream::connect(format!("127.0.0.1:{}", actual_port)).await.is_ok() {
                        tracing::info!("Server ready on http://127.0.0.1:{} (after {}ms)", actual_port, attempt * 50);
                        ready = true;
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            if !ready {
                tracing::error!("Server failed to start within 5 seconds");
            }

            // Run the UI event loop (blocking — takes over the main thread)
            ui::run(actual_port)?;
        }
        Commands::Config => {
            let data_dir = storage::default_data_dir();
            let cfg = config::AppConfig::load(&data_dir)?;
            println!("Data directory: {}", data_dir.display());
            println!(
                "{}",
                toml::to_string_pretty(&cfg).unwrap_or_default()
            );
        }
        Commands::Agents => {
            let data_dir = storage::default_data_dir();
            let db = storage::Database::new(&data_dir)?;
            let conn = db.connect()?;
            let mut stmt = conn.prepare("SELECT name, description FROM agents ORDER BY name")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                ))
            })?;
            let mut count = 0;
            for row in rows {
                let (name, desc) = row?;
                println!("  {} - {}", name, desc);
                count += 1;
            }
            if count == 0 {
                println!("  (none installed)");
            }
        }
        Commands::Test { message, provider, model, agent: _ } => {
            println!("=== Chitty Workspace CLI Test ===");
            println!("Provider: {}", provider);

            // Resolve provider
            let provider_id = match provider.as_str() {
                "xai" => providers::ProviderId::Xai,
                "anthropic" => providers::ProviderId::Anthropic,
                "openai" => providers::ProviderId::Openai,
                other => {
                    eprintln!("Unknown provider: {}", other);
                    std::process::exit(1);
                }
            };

            // Get API key from keyring (same format as config::get_api_key)
            let api_key = match config::get_api_key(&provider) {
                Ok(Some(k)) => k,
                Ok(None) => {
                    eprintln!("No API key found for '{}'. Set one in Settings > API Keys.", provider);
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("Failed to read API key for '{}': {}", provider, e);
                    std::process::exit(1);
                }
            };

            // Create provider
            let llm: Box<dyn providers::Provider> = match provider_id {
                providers::ProviderId::Xai => {
                    Box::new(providers::adaptors::xai::XaiProvider::new(api_key, None))
                }
                providers::ProviderId::Anthropic => {
                    Box::new(providers::cloud::AnthropicProvider::new(api_key, None))
                }
                _ => {
                    eprintln!("Provider '{}' not yet supported in CLI test", provider);
                    std::process::exit(1);
                }
            };

            // Resolve model
            let model_id = match model {
                Some(m) => m,
                None => {
                    // Discover models and pick first chat-capable one
                    println!("Discovering models...");
                    let models = llm.list_models().await?;
                    let first = models.into_iter().next();
                    match first {
                        Some(m) => {
                            println!("Using model: {}", m.id);
                            m.id
                        }
                        None => {
                            eprintln!("No models found");
                            std::process::exit(1);
                        }
                    }
                }
            };

            println!("Model: {}", model_id);
            println!("Message: {}", message);
            println!("---");

            // Send chat request with streaming
            let messages = vec![
                providers::ChatMessage {
                    role: "system".to_string(),
                    content: "You are a helpful assistant. Respond concisely.".to_string(),
                    tool_calls: None,
                    tool_call_id: None,
                },
                providers::ChatMessage {
                    role: "user".to_string(),
                    content: message,
                    tool_calls: None,
                    tool_call_id: None,
                },
            ];

            let (tx, mut rx) = tokio::sync::mpsc::channel::<providers::StreamChunk>(256);

            let model_clone = model_id.clone();
            let stream_handle = tokio::spawn(async move {
                // Read chunks and print them
                let mut text_chars = 0;
                let mut thinking_chars = 0;
                let mut in_thinking = false;
                while let Some(chunk) = rx.recv().await {
                    match chunk {
                        providers::StreamChunk::Thinking(t) => {
                            if !in_thinking {
                                eprint!("[THINKING] ");
                                in_thinking = true;
                            }
                            thinking_chars += t.len();
                            // Don't print thinking content — just show progress
                            if thinking_chars % 500 < t.len() {
                                eprint!(".");
                            }
                        }
                        providers::StreamChunk::Text(t) => {
                            if in_thinking {
                                eprintln!(" ({} chars)", thinking_chars);
                                in_thinking = false;
                            }
                            text_chars += t.len();
                            print!("{}", t);
                        }
                        providers::StreamChunk::ToolCallStart { id, name } => {
                            if in_thinking {
                                eprintln!(" ({} chars)", thinking_chars);
                                in_thinking = false;
                            }
                            println!("\n[TOOL CALL] {} ({})", name, id);
                        }
                        providers::StreamChunk::ToolCallDelta { id: _, arguments } => {
                            print!("{}", arguments);
                        }
                        providers::StreamChunk::ToolCallEnd { id } => {
                            println!("\n[TOOL CALL END] {}", id);
                        }
                        providers::StreamChunk::Done => {
                            if in_thinking {
                                eprintln!(" ({} chars)", thinking_chars);
                            }
                            println!("\n---");
                            println!("Done. {} text chars, {} thinking chars", text_chars, thinking_chars);
                            break;
                        }
                        providers::StreamChunk::Error(e) => {
                            eprintln!("\n[ERROR] {}", e);
                            break;
                        }
                        _ => {}
                    }
                }
            });

            println!("Sending to {} / {}...", provider, model_clone);
            let start = std::time::Instant::now();
            match llm.chat_stream(&model_clone, &messages, None, tx).await {
                Ok(()) => {
                    println!("Stream completed in {:.1}s", start.elapsed().as_secs_f64());
                }
                Err(e) => {
                    eprintln!("Stream error: {}", e);
                }
            }
            let _ = stream_handle.await;
        }
        Commands::TestAgentBuilder { description } => {
            println!("=== Chitty Workspace Agent Builder CLI Test ===");
            println!("Description: {}", description);
            println!("---");

            // Start the server in the background
            let data_dir = storage::default_data_dir();
            let db = storage::Database::new(&data_dir)?;
            let sb_bridge = std::sync::Arc::new(server::BrowserBridge::new());
            let sb_skills = std::sync::Arc::new(skills::SkillRegistry::new(&data_dir, None));
            let tool_registry = std::sync::Arc::new(tools::ToolRegistry::new(sb_bridge.clone(), sb_skills.clone()));
            let tool_runtime = std::sync::Arc::new(tokio::sync::RwLock::new(
                tools::ToolRuntime::new(&data_dir, sb_bridge.clone(), sb_skills.clone())?
            ));
            let port: u16 = 8770;
            let server_db = db.clone();
            let server_tools = tool_registry.clone();
            let server_runtime = tool_runtime.clone();
            let server_skills = sb_skills.clone();
            let bp = std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0));
            tokio::spawn(async move {
                if let Err(e) = server::start(server_db, server_tools, server_runtime, sb_bridge, server_skills, port, bp).await {
                    tracing::error!("Server error: {}", e);
                }
            });
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;

            // Call the agent builder endpoint
            let client = reqwest::Client::new();
            let resp = client
                .post(format!("http://127.0.0.1:{}/api/agent-builder/generate", port))
                .json(&serde_json::json!({ "description": description }))
                .send()
                .await?;

            println!("Status: {}", resp.status());

            // Read SSE stream
            let mut stream = resp.bytes_stream();
            use futures::StreamExt;
            let mut buffer = String::new();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(&bytes);
                        buffer.push_str(&text);
                        // Process complete SSE events
                        while let Some(pos) = buffer.find("\n\n") {
                            let event_str = buffer[..pos].to_string();
                            buffer = buffer[pos + 2..].to_string();
                            for line in event_str.lines() {
                                if let Some(data) = line.strip_prefix("data: ") {
                                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
                                        if let Some(t) = v.get("type").and_then(|t| t.as_str()) {
                                            match t {
                                                "thinking" => eprint!("."),
                                                "text" => {
                                                    if let Some(c) = v.get("content").and_then(|c| c.as_str()) {
                                                        print!("{}", c);
                                                    }
                                                }
                                                "tool_call_start" => {
                                                    let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                                                    println!("\n[TOOL] {} ...", name);
                                                }
                                                "tool_result" => {
                                                    let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                                                    let success = v.get("success").and_then(|s| s.as_bool()).unwrap_or(false);
                                                    println!("[RESULT] {} success={}", name, success);
                                                }
                                                "error" => {
                                                    let msg = v.get("message").and_then(|m| m.as_str()).unwrap_or("?");
                                                    eprintln!("[ERROR] {}", msg);
                                                }
                                                "done" => {
                                                    println!("\n[DONE]");
                                                }
                                                other => {
                                                    println!("[{}] {:?}", other, v);
                                                }
                                            }
                                        }
                                    } else {
                                        println!("[RAW] {}", data);
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[STREAM ERROR] {}", e);
                        break;
                    }
                }
            }
            println!("\nAgent builder test complete.");
        }
    }

    Ok(())
}

/// Handle a chitty:// protocol URL.
/// Called when the user clicks an Install button on chitty.ai.
/// URL format: chitty://install/{package-name}
async fn handle_protocol_url(url: &str) -> anyhow::Result<()> {
    tracing::info!("Protocol URL received: {}", url);

    // Parse: chitty://install/web-tools → ("install", "web-tools")
    let path = url
        .strip_prefix("chitty://").unwrap_or(url)
        .trim_end_matches('/');

    let parts: Vec<&str> = path.splitn(2, '/').collect();
    let (action, target) = match parts.as_slice() {
        [action, target] => (*action, *target),
        [action] => (*action, ""),
        _ => {
            eprintln!("Invalid protocol URL: {}", url);
            return Ok(());
        }
    };

    match action {
        "install" => {
            if target.is_empty() {
                eprintln!("Missing package name: chitty://install/<package-name>");
                return Ok(());
            }
            println!("Installing package: {}", target);

            // Send install request to the running Chitty Workspace server
            let client = reqwest::Client::new();
            let resp = client
                .post("http://127.0.0.1:8770/api/marketplace/registry/install")
                .json(&serde_json::json!({
                    "name": target,
                    "version": "latest"
                }))
                .send()
                .await;

            match resp {
                Ok(r) => {
                    let body: serde_json::Value = r.json().await.unwrap_or_default();
                    if body.get("success").and_then(|s| s.as_bool()).unwrap_or(false) {
                        println!("Installed {} successfully.", target);
                    } else {
                        let err = body.get("error").and_then(|e| e.as_str()).unwrap_or("unknown error");
                        eprintln!("Install failed: {}", err);
                    }
                }
                Err(e) => {
                    eprintln!("Could not connect to Chitty Workspace (is it running?): {}", e);
                }
            }
        }
        other => {
            eprintln!("Unknown protocol action: {}", other);
        }
    }

    Ok(())
}

/// Register the chitty:// protocol handler on Windows.
/// When a user clicks `chitty://install/web-tools` on the website,
/// Windows launches this binary with the URL as an argument.
#[cfg(target_os = "windows")]
fn register_protocol_handler() {
    use std::process::Command;

    let exe_path = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(_) => return,
    };

    // Register HKEY_CURRENT_USER\Software\Classes\chitty (no admin needed)
    let commands = [
        format!(r#"reg add "HKCU\Software\Classes\chitty" /ve /d "URL:Chitty Workspace" /f"#),
        format!(r#"reg add "HKCU\Software\Classes\chitty" /v "URL Protocol" /d "" /f"#),
        format!(r#"reg add "HKCU\Software\Classes\chitty\shell\open\command" /ve /d "\"{exe_path}\" \"--protocol\" \"%1\"" /f"#, exe_path = exe_path),
    ];

    for cmd in &commands {
        let _ = Command::new("cmd")
            .args(["/C", cmd])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    tracing::info!("Registered chitty:// protocol handler → {}", exe_path);
}
