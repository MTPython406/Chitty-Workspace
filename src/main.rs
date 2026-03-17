mod chat;
mod config;
mod integrations;
mod providers;
mod server;
mod skills;
mod storage;
mod tools;
mod ui;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "chitty-workspace")]
#[command(about = "Chitty Workspace - Local AI assistant with skills, tools, and BYOK providers")]
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
    /// List installed skills
    Skills,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("chitty_workspace=info".parse()?),
        )
        .init();

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

            // Create tool registry with all native tools
            let tool_registry = std::sync::Arc::new(tools::ToolRegistry::new());
            tracing::info!(
                "Registered {} native tools",
                tool_registry.list_definitions().len()
            );

            // Start the HTTP server in the background
            let port: u16 = 8770;
            let server_db = db.clone();
            let server_tools = tool_registry.clone();
            tokio::spawn(async move {
                if let Err(e) = server::start(server_db, server_tools, port).await {
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
        Commands::Skills => {
            let data_dir = storage::default_data_dir();
            let db = storage::Database::new(&data_dir)?;
            let conn = db.connect()?;
            let mut stmt = conn.prepare("SELECT name, description FROM skills ORDER BY name")?;
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
    }

    Ok(())
}
