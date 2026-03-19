mod chat;
mod config;
mod integrations;
mod providers;
mod server;
mod agents;
mod storage;
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

    match cli.command.unwrap_or(Commands::Run) {
        Commands::Run => {
            tracing::info!("Starting Chitty Workspace v{}", env!("CARGO_PKG_VERSION"));

            // Initialize storage
            let data_dir = storage::default_data_dir();
            tracing::info!("Data directory: {:?}", data_dir);
            let db = storage::Database::new(&data_dir)?;

            // Load config (creates default if missing)
            let _config = config::AppConfig::load(&data_dir)?;

            // Create browser bridge (shared between tool system and server)
            let browser_bridge = std::sync::Arc::new(server::BrowserBridge::new());

            // Create tool registry (native tools) and runtime (native + custom + connections)
            let tool_registry = std::sync::Arc::new(tools::ToolRegistry::new(browser_bridge.clone()));
            let tool_runtime = match tools::ToolRuntime::new(&data_dir, browser_bridge.clone()) {
                Ok(rt) => std::sync::Arc::new(tokio::sync::RwLock::new(rt)),
                Err(e) => {
                    tracing::error!("Failed to initialize tool runtime: {}", e);
                    std::sync::Arc::new(tokio::sync::RwLock::new(
                        tools::ToolRuntime::new(&data_dir, browser_bridge.clone()).expect("Tool runtime init failed")
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
            let server_db = db.clone();
            let server_tools = tool_registry.clone();
            let server_runtime = tool_runtime.clone();
            let server_bridge = browser_bridge.clone();
            tokio::spawn(async move {
                if let Err(e) = server::start(server_db, server_tools, server_runtime, server_bridge, port).await {
                    tracing::error!("Server error: {}", e);
                }
            });

            // Give the server a moment to bind
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            tracing::info!("Server ready on http://127.0.0.1:{}", port);

            // Run the UI event loop (blocking — takes over the main thread)
            ui::run(port)?;
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
            let tool_registry = std::sync::Arc::new(tools::ToolRegistry::new(sb_bridge.clone()));
            let tool_runtime = std::sync::Arc::new(tokio::sync::RwLock::new(
                tools::ToolRuntime::new(&data_dir, sb_bridge.clone())?
            ));
            let port: u16 = 8770;
            let server_db = db.clone();
            let server_tools = tool_registry.clone();
            let server_runtime = tool_runtime.clone();
            tokio::spawn(async move {
                if let Err(e) = server::start(server_db, server_tools, server_runtime, sb_bridge, port).await {
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
