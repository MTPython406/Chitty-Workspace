mod chat;
mod config;
mod connections;
mod gpu;
mod huggingface;
mod integrations;
mod media;
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
    Run {
        /// Start in headless mode (no tray icon or window — server only, access via browser)
        #[arg(long)]
        headless: bool,

        /// Port to listen on
        #[arg(long, default_value = "8770")]
        port: u16,
    },
    /// Show current configuration
    Config,
    /// List installed agents
    Agents,
    /// Test chat from CLI (headless — no UI, just terminal output)
    Test {
        /// Message to send to the LLM
        #[arg(default_value = "Hello, what can you do?")]
        message: String,
        /// Provider to use (ollama, huggingface, xai, anthropic, openai)
        #[arg(short, long, default_value = "ollama")]
        provider: String,
        /// Model to use
        #[arg(short, long)]
        model: Option<String>,
        /// Agent ID to use
        #[arg(short = 'a', long)]
        agent: Option<String>,
    },
    /// Chat with an agent from CLI (headless — supports tool calling)
    Chat {
        /// Message to send
        message: String,
        /// Provider to use (ollama, huggingface, xai, anthropic, openai)
        #[arg(short, long, default_value = "ollama")]
        provider: String,
        /// Model to use
        #[arg(short, long)]
        model: Option<String>,
        /// Agent ID or name
        #[arg(short = 'a', long)]
        agent: Option<String>,
        /// Max tool-calling iterations (default: 10)
        #[arg(long, default_value = "10")]
        max_iterations: u32,
        /// Auto-approve all tool calls (no prompts)
        #[arg(long)]
        auto_approve: bool,
        /// Project path for context
        #[arg(short = 'd', long)]
        project: Option<String>,
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
    // Ensure data directory exists (first run or after clean uninstall)
    std::fs::create_dir_all(&data_dir).expect("Failed to create data directory");
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

    match cli.command.unwrap_or(Commands::Run { headless: false, port: 8770 }) {
        Commands::Run { headless, port } => {
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

            // Copy browser extension to data directory (user-accessible location)
            // MSIX installs to protected WindowsApps — Chrome can't load unpacked from there
            let ext_dest = data_dir.join("extension");
            if !ext_dest.exists() {
                if let Ok(exe) = std::env::current_exe() {
                    if let Some(exe_dir) = exe.parent() {
                        let ext_src = exe_dir.join("extension");
                        if ext_src.exists() {
                            if let Err(e) = copy_dir_recursive(&ext_src, &ext_dest) {
                                tracing::warn!("Failed to copy browser extension: {}", e);
                            } else {
                                tracing::info!("Browser extension copied to {:?}", ext_dest);
                            }
                        }
                    }
                }
            }

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

            // Start the HTTP server on a DEDICATED thread with its own tokio runtime.
            // This prevents the tao/Win32 event loop from starving the async I/O driver.
            // Without this, external HTTP clients (curl, Flask) hang because the main
            // thread's message pump interferes with tokio's I/O on Windows.
            let port: u16 = port;
            let bound_port = std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0));
            let server_db = db.clone();
            let server_tools = tool_registry.clone();
            let server_runtime = tool_runtime.clone();
            let server_bridge = browser_bridge.clone();
            let server_skills = skill_registry.clone();
            let bp = bound_port.clone();

            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .worker_threads(4)
                    .build()
                    .expect("Failed to create server runtime");

                rt.block_on(async move {
                    if let Err(e) = server::start(server_db, server_tools, server_runtime, server_bridge, server_skills, port, bp).await {
                        tracing::error!("Server error: {}", e);
                    }
                });
            });

            // Poll for server readiness (up to 5 seconds)
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

            if headless {
                // Headless mode — no tray icon or window, just the HTTP server.
                // Users access the chat UI via browser at http://127.0.0.1:{port}
                eprintln!("Chitty Workspace v{} running at http://127.0.0.1:{}", env!("CARGO_PKG_VERSION"), actual_port);
                eprintln!("Open this URL in your browser. Press Ctrl+C to stop.");
                tracing::info!("Headless mode — waiting for Ctrl+C");
                tokio::signal::ctrl_c().await?;
                eprintln!("Shutting down.");
            } else {
                // Desktop mode — run the UI event loop (blocking — takes over the main thread).
                // The server runs on its own thread so this doesn't block HTTP requests.
                ui::run(actual_port)?;
            }
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
            let mut stmt = conn.prepare("SELECT id, name, description FROM agents ORDER BY name")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let mut count = 0;
            for row in rows {
                let (id, name, desc) = row?;
                println!("  {} ({}) - {}", id, name, desc);
                count += 1;
            }
            if count == 0 {
                println!("  (none installed)");
            }
        }
        Commands::Test { message, provider, model, agent: _ } => {
            println!("=== Chitty Workspace CLI Test ===");
            println!("Provider: {}", provider);

            // Create provider (local providers don't need API keys)
            let llm: Box<dyn providers::Provider> = create_cli_provider(&provider)?;

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
        Commands::Chat { message, provider, model, agent, max_iterations, auto_approve, project } => {
            run_cli_chat(message, provider, model, agent, max_iterations, auto_approve, project).await?;
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

/// Recursively copy a directory tree.
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

/// Create an LLM provider from a provider name string.
/// Local providers (ollama, huggingface) don't need API keys.
fn create_cli_provider(provider: &str) -> anyhow::Result<Box<dyn providers::Provider>> {
    match provider {
        "ollama" => {
            Ok(Box::new(providers::ollama::OllamaProvider::new(
                "http://localhost:11434".to_string(),
            )))
        }
        "huggingface" => {
            Ok(Box::new(providers::local_sidecar::LocalSidecarProvider::new(
                "http://localhost:8766".to_string(),
            )))
        }
        "xai" | "anthropic" | "openai" => {
            let api_key = config::get_api_key(provider)?
                .ok_or_else(|| anyhow::anyhow!("No API key found for '{}'. Set one in Settings > API Keys.", provider))?;
            match provider {
                "xai" => Ok(Box::new(providers::adaptors::xai::XaiProvider::new(api_key, None))),
                "anthropic" => Ok(Box::new(providers::cloud::AnthropicProvider::new(api_key, None))),
                "openai" => Ok(Box::new(providers::adaptors::xai::XaiProvider::new(
                    api_key,
                    Some("https://api.openai.com/v1".to_string()),
                ))),
                _ => unreachable!(),
            }
        }
        other => anyhow::bail!("Unknown provider: {}. Use: ollama, huggingface, xai, anthropic, openai", other),
    }
}

/// CLI chat with agent support and tool-calling loop.
async fn run_cli_chat(
    message: String,
    provider_name: String,
    model: Option<String>,
    agent_id: Option<String>,
    max_iterations: u32,
    auto_approve: bool,
    project_path: Option<String>,
) -> anyhow::Result<()> {
    eprintln!("=== Chitty Workspace CLI Chat ===");

    // Initialize infrastructure
    let data_dir = storage::default_data_dir();
    let db = storage::Database::new(&data_dir)?;
    let _config = config::AppConfig::load(&data_dir)?;
    let browser_bridge = std::sync::Arc::new(server::BrowserBridge::new());
    let skill_registry = std::sync::Arc::new(skills::SkillRegistry::new(&data_dir, None));
    let tool_runtime = tools::ToolRuntime::new(&data_dir, browser_bridge.clone(), skill_registry.clone())?;

    // Resolve agent (by ID or name)
    let resolved_agent_id = if let Some(ref agent_ref) = agent_id {
        let conn = db.connect()?;
        // Try loading by ID first
        if agents::AgentsManager::load(&conn, agent_ref)?.is_some() {
            Some(agent_ref.clone())
        } else {
            // Try by name
            let mut stmt = conn.prepare("SELECT id FROM agents WHERE name = ?1 COLLATE NOCASE")?;
            let id: Option<String> = stmt.query_row(rusqlite::params![agent_ref], |row| row.get(0)).ok();
            if id.is_none() {
                eprintln!("Agent '{}' not found. Run 'chitty-workspace agents' to list available agents.", agent_ref);
                std::process::exit(1);
            }
            id
        }
    } else {
        None
    };

    // Create provider
    let llm = create_cli_provider(&provider_name)?;

    // Resolve model (use agent default, explicit flag, or discover)
    let model_id = if let Some(m) = model {
        m
    } else if let Some(ref aid) = resolved_agent_id {
        let conn = db.connect()?;
        let agent = agents::AgentsManager::load(&conn, aid)?.unwrap();
        agent.preferred_model.unwrap_or_else(|| {
            eprintln!("No model specified and agent has no preferred model. Use -m to specify.");
            std::process::exit(1);
        })
    } else {
        // Discover first available model
        eprintln!("Discovering models...");
        let models = llm.list_models().await?;
        match models.into_iter().next() {
            Some(m) => {
                eprintln!("Using model: {}", m.id);
                m.id
            }
            None => {
                eprintln!("No models found for provider '{}'", provider_name);
                std::process::exit(1);
            }
        }
    };

    // Assemble context
    let all_tool_defs = tool_runtime.list_definitions();
    let conversation_id = uuid::Uuid::new_v4().to_string();

    let (ctx, exec_config, _effective_project) = {
        let conn = db.connect()?;
        chat::ChatEngine::assemble_context(
            &conn,
            &conversation_id,
            resolved_agent_id.as_deref(),
            project_path.as_deref(),
            &all_tool_defs,
            &skill_registry,
        )?
    };

    let effective_max_iter = if max_iterations != 10 { max_iterations } else { exec_config.max_iterations };

    eprintln!("Provider: {} | Model: {} | Agent: {} | Max iterations: {} | Auto-approve: {}",
        provider_name, model_id,
        resolved_agent_id.as_deref().unwrap_or("(default)"),
        effective_max_iter, auto_approve);
    eprintln!("Tools available: {} | Message: {}", ctx.tools.len(), message);
    eprintln!("---");

    // Build initial messages
    let mut messages = vec![
        providers::ChatMessage {
            role: "system".to_string(),
            content: ctx.system_prompt,
            tool_calls: None,
            tool_call_id: None,
        },
    ];
    // Add any conversation history from context
    messages.extend(ctx.messages);
    // Add user message
    messages.push(providers::ChatMessage {
        role: "user".to_string(),
        content: message,
        tool_calls: None,
        tool_call_id: None,
    });

    let tools_json = if ctx.tools.is_empty() { None } else { Some(ctx.tools.as_slice()) };

    // Tool-calling loop
    for iteration in 0..effective_max_iter {
        // Stream response from provider
        let (tx, mut rx) = tokio::sync::mpsc::channel::<providers::StreamChunk>(256);

        let model_clone = model_id.clone();
        let msgs_clone = messages.clone();
        let tools_clone = ctx.tools.clone();
        let tools_ref = if tools_clone.is_empty() { None } else { Some(tools_clone) };

        let stream_handle = tokio::spawn(async move {
            let mut full_text = String::new();
            let mut pending_calls: std::collections::HashMap<String, (String, String)> = std::collections::HashMap::new(); // id -> (name, args_json)
            let mut in_thinking = false;

            while let Some(chunk) = rx.recv().await {
                match chunk {
                    providers::StreamChunk::Thinking(t) => {
                        if !in_thinking {
                            eprint!("[thinking] ");
                            in_thinking = true;
                        }
                        let _ = t; // suppress thinking content
                        eprint!(".");
                    }
                    providers::StreamChunk::Text(t) => {
                        if in_thinking { eprintln!(); in_thinking = false; }
                        print!("{}", t);
                        full_text.push_str(&t);
                    }
                    providers::StreamChunk::ToolCallStart { id, name } => {
                        if in_thinking { eprintln!(); in_thinking = false; }
                        pending_calls.insert(id, (name, String::new()));
                    }
                    providers::StreamChunk::ToolCallDelta { id, arguments } => {
                        if let Some((_, ref mut args)) = pending_calls.get_mut(&id) {
                            args.push_str(&arguments);
                        }
                    }
                    providers::StreamChunk::ToolCallEnd { id: _ } => {}
                    providers::StreamChunk::Done => break,
                    providers::StreamChunk::Error(e) => {
                        eprintln!("\n[ERROR] {}", e);
                        break;
                    }
                    _ => {}
                }
            }
            if in_thinking { eprintln!(); }

            // Build tool calls
            let tool_calls: Vec<providers::ToolCall> = pending_calls.into_iter().map(|(id, (name, args_json))| {
                let arguments = serde_json::from_str(&args_json).unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                providers::ToolCall { id, name, arguments }
            }).collect();

            (full_text, tool_calls)
        });

        // Send to LLM
        let tools_for_call = tools_ref.as_deref();
        if let Err(e) = llm.chat_stream(&model_clone, &msgs_clone, tools_for_call, tx).await {
            eprintln!("[ERROR] Stream failed: {}", e);
            break;
        }

        let (full_text, tool_calls) = stream_handle.await?;

        if tool_calls.is_empty() {
            // No tool calls — done
            if !full_text.is_empty() { println!(); }
            break;
        }

        // Add assistant message with tool calls
        messages.push(providers::ChatMessage {
            role: "assistant".to_string(),
            content: full_text,
            tool_calls: Some(tool_calls.clone()),
            tool_call_id: None,
        });

        eprintln!("\n[Iteration {}/{}] {} tool call(s)", iteration + 1, effective_max_iter, tool_calls.len());

        // Execute each tool call
        for tc in &tool_calls {
            eprintln!("[TOOL] {}({})", tc.name, serde_json::to_string(&tc.arguments).unwrap_or_default());

            // Approval gate
            if !auto_approve {
                eprint!("  Execute? [y/N] ");
                use std::io::Write;
                std::io::stderr().flush().ok();
                let mut input = String::new();
                std::io::stdin().read_line(&mut input).ok();
                if !input.trim().eq_ignore_ascii_case("y") {
                    eprintln!("  Denied.");
                    messages.push(providers::ChatMessage {
                        role: "tool".to_string(),
                        content: "Tool call denied by user.".to_string(),
                        tool_calls: None,
                        tool_call_id: Some(tc.id.clone()),
                    });
                    continue;
                }
            }

            let tool_ctx = tools::ToolContext {
                working_dir: project_path.as_ref().map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
                db: db.clone(),
                conversation_id: conversation_id.clone(),
            };

            let (result, duration_ms) = tool_runtime.execute(&tc.name, &tc.arguments, &tool_ctx).await;

            let result_str = result.as_content_string();
            let truncated = if result_str.len() > 10_000 {
                format!("{}... [truncated, {} chars total]", &result_str[..10_000], result_str.len())
            } else {
                result_str
            };

            eprintln!("  [RESULT] success={} ({}ms) {}", result.success,
                duration_ms,
                if truncated.len() > 200 { format!("{}...", &truncated[..200]) } else { truncated.clone() });

            messages.push(providers::ChatMessage {
                role: "tool".to_string(),
                content: truncated,
                tool_calls: None,
                tool_call_id: Some(tc.id.clone()),
            });
        }
    }

    eprintln!("---\nChat complete.");
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
