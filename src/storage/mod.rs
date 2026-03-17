//! Local SQLite storage
//!
//! All data stored locally in ~/.chitty-workspace/workspace.db
//! Schema versioned with migrations.

pub mod schema;

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::PathBuf;

/// Database manager
pub struct Database {
    path: PathBuf,
}

impl Database {
    /// Create a new database manager. Creates the file and runs migrations if needed.
    pub fn new(data_dir: &PathBuf) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("Failed to create data directory: {:?}", data_dir))?;

        let path = data_dir.join("workspace.db");
        let db = Self { path };
        db.run_migrations()?;
        Ok(db)
    }

    /// Open a connection to the database
    pub fn connect(&self) -> Result<Connection> {
        let conn = Connection::open(&self.path)
            .with_context(|| format!("Failed to open database: {:?}", self.path))?;

        // Enable WAL mode for better concurrent access
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;

        Ok(conn)
    }

    /// Run all pending migrations
    fn run_migrations(&self) -> Result<()> {
        let conn = self.connect()?;
        schema::run_migrations(&conn)?;
        Ok(())
    }
}

/// Get the default data directory (~/.chitty-workspace/)
pub fn default_data_dir() -> PathBuf {
    directories::ProjectDirs::from("ai", "datavisions", "chitty-workspace")
        .map(|dirs| dirs.data_dir().to_path_buf())
        .unwrap_or_else(|| {
            let home = std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".chitty-workspace")
        })
}
