mod chat;
mod config;
mod integrations;
mod providers;
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
            // TODO: Initialize storage, load config, start UI
            println!("Chitty Workspace v{}", env!("CARGO_PKG_VERSION"));
        }
        Commands::Config => {
            println!("Configuration:");
            // TODO: Show config
        }
        Commands::Skills => {
            println!("Installed skills:");
            // TODO: List skills
        }
    }

    Ok(())
}
