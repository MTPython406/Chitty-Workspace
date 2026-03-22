//! Connection Manager — manages persistent background connections for marketplace packages.
//!
//! Marketplace packages can declare long-running connections (WebSocket listeners,
//! event streams, etc.) in their `package.json`. The ConnectionManager:
//!
//! 1. Spawns connection scripts as subprocesses (Python/Node/Shell)
//! 2. Communicates via stdin/stdout using an NDJSON protocol
//! 3. Monitors health via heartbeats and restarts on failure
//! 4. Routes incoming events to configured agents via `connection_event_routes`
//!
//! ## NDJSON Protocol
//!
//! Script → Platform (stdout, newline-delimited JSON):
//! - `{"type":"ready","message":"Connected to Slack"}`
//! - `{"type":"heartbeat"}`
//! - `{"type":"event","event_id":"mention","correlation_id":"uuid","data":{...}}`
//! - `{"type":"log","level":"info","message":"..."}`
//! - `{"type":"error","message":"...","fatal":false}`
//!
//! Platform → Script (stdin, newline-delimited JSON):
//! - `{"type":"response","correlation_id":"uuid","data":{"text":"agent response"}}`
//! - `{"type":"shutdown"}`

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::RwLock;

use crate::server::AppState;
use crate::storage::Database;
use crate::tools::manifest::{PackageConnection, RuntimeType};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Current status of a managed connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    Stopped,
    Starting,
    Connected,
    Reconnecting,
    Error,
    Stopping,
}

impl std::fmt::Display for ConnectionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionStatus::Stopped => write!(f, "stopped"),
            ConnectionStatus::Starting => write!(f, "starting"),
            ConnectionStatus::Connected => write!(f, "connected"),
            ConnectionStatus::Reconnecting => write!(f, "reconnecting"),
            ConnectionStatus::Error => write!(f, "error"),
            ConnectionStatus::Stopping => write!(f, "stopping"),
        }
    }
}

/// Tracks a running connection subprocess and its metadata.
#[derive(Debug)]
pub struct ActiveConnection {
    /// Composite key: "{package_id}:{connection_id}"
    pub key: String,
    pub package_id: String,
    pub connection_id: String,
    /// The subprocess handle (None if the process has exited)
    pub child: Option<Child>,
    /// Stdin writer for sending NDJSON messages to the script
    pub stdin_tx: Option<tokio::process::ChildStdin>,
    pub status: ConnectionStatus,
    pub last_heartbeat: Option<Instant>,
    pub restart_count: u32,
    pub max_restarts: u32,
    pub restart_delay_secs: u32,
    pub health_interval_secs: u32,
    pub restart_on_failure: bool,
    pub error_message: Option<String>,
    /// Directory containing the package (for resolving script paths)
    pub package_dir: PathBuf,
    /// Connection config from the package manifest
    pub connection_config: PackageConnection,
}

/// Messages from the connection script (stdout, NDJSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScriptMessage {
    Ready {
        #[serde(default)]
        message: Option<String>,
    },
    Heartbeat,
    Event {
        event_id: String,
        #[serde(default)]
        correlation_id: Option<String>,
        #[serde(default)]
        data: serde_json::Value,
    },
    Log {
        #[serde(default = "default_log_level")]
        level: String,
        message: String,
    },
    Error {
        message: String,
        #[serde(default)]
        fatal: bool,
    },
}

fn default_log_level() -> String {
    "info".to_string()
}

/// Messages from the platform to the connection script (stdin, NDJSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlatformMessage {
    Response {
        correlation_id: String,
        data: serde_json::Value,
    },
    Shutdown,
}

/// Manages all active connections across marketplace packages.
pub struct ConnectionManager {
    /// Active connections keyed by "{package_id}:{connection_id}"
    pub connections: HashMap<String, ActiveConnection>,
    /// Reference to shared application state (set during run)
    pub state: Option<Arc<AppState>>,
}

// ---------------------------------------------------------------------------
// Connection key helper
// ---------------------------------------------------------------------------

fn connection_key(package_id: &str, connection_id: &str) -> String {
    format!("{}:{}", package_id, connection_id)
}

// ---------------------------------------------------------------------------
// ConnectionManager implementation
// ---------------------------------------------------------------------------

impl ConnectionManager {
    /// Create a new ConnectionManager.
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
            state: None,
        }
    }

    /// Main entry point — called from server.rs via `tokio::spawn`.
    ///
    /// Scans installed marketplace packages for declared connections,
    /// starts any that are enabled, and enters a monitoring loop that
    /// checks health and restarts failed connections every 10 seconds.
    pub async fn run(state: Arc<AppState>) {
        tracing::info!("ConnectionManager: starting background connection manager");

        // Set state on the manager
        {
            let mut mgr = state.connection_manager.write().await;
            mgr.state = Some(Arc::clone(&state));
        }

        // --- Initial scan: discover and start enabled connections ---
        {
            let mut mgr = state.connection_manager.write().await;
            mgr.scan_and_start_connections().await;
        }

        // --- Monitoring loop ---
        let mut interval = tokio::time::interval(Duration::from_secs(10));

        loop {
            interval.tick().await;

            // Check health of all active connections
            let mut mgr = state.connection_manager.write().await;
            mgr.monitor_connections().await;
        }
    }

    /// Scan marketplace packages for connections and start enabled ones.
    async fn scan_and_start_connections(&mut self) {
        let packages = {
            let rt = self.state.as_ref().unwrap().tool_runtime.read().await;
            rt.list_marketplace_packages()
                .into_iter()
                .filter(|pkg| !pkg.manifest.connections.is_empty())
                .map(|pkg| (pkg.manifest.clone(), pkg.dir.clone()))
                .collect::<Vec<_>>()
        };

        tracing::info!(
            "ConnectionManager: found {} packages with connections",
            packages.len()
        );

        for (manifest, pkg_dir) in packages {
            for conn_def in &manifest.connections {
                let key = connection_key(&manifest.name, &conn_def.id);

                // Check if connection should be started (feature flag check)
                let should_start = self
                    .should_connection_start(&manifest.name, conn_def)
                    .await;

                if should_start {
                    tracing::info!(
                        "ConnectionManager: starting connection {} (package: {})",
                        conn_def.id,
                        manifest.name
                    );
                    self.start_connection_internal(
                        &manifest.name,
                        &conn_def.id,
                        conn_def.clone(),
                        pkg_dir.clone(),
                    )
                    .await;
                } else {
                    tracing::info!(
                        "ConnectionManager: skipping disabled connection {} (key: {})",
                        conn_def.label,
                        key
                    );
                }
            }
        }
    }

    /// Check whether a connection should be started based on feature flags and credentials.
    async fn should_connection_start(
        &self,
        package_id: &str,
        conn_def: &PackageConnection,
    ) -> bool {
        // Check feature flag requirement
        if let Some(ref feature_id) = conn_def.requires_feature {
            let db = self.state.as_ref().unwrap().db.clone();
            let pkg_id = package_id.to_string();
            let feat_id = feature_id.clone();

            let feature_enabled = db
                .with_conn(move |conn| {
                    let enabled: bool = conn
                        .query_row(
                            "SELECT enabled FROM package_features WHERE package_id = ?1 AND feature_id = ?2",
                            rusqlite::params![pkg_id, feat_id],
                            |row| row.get(0),
                        )
                        .unwrap_or(false);
                    Ok(enabled)
                })
                .await
                .unwrap_or(false);

            if !feature_enabled {
                tracing::info!(
                    "ConnectionManager: connection {}/{} requires feature '{}' which is not enabled",
                    package_id,
                    conn_def.id,
                    feature_id
                );
                return false;
            }
        }

        // Check credential requirements
        for cred_key in &conn_def.requires_credentials {
            match keyring::Entry::new("chitty-workspace", cred_key) {
                Ok(entry) => match entry.get_password() {
                    Ok(pw) if !pw.is_empty() => {}
                    _ => {
                        tracing::info!(
                            "ConnectionManager: connection {}/{} requires credential '{}' which is not set",
                            package_id,
                            conn_def.id,
                            cred_key
                        );
                        return false;
                    }
                },
                Err(_) => {
                    tracing::info!(
                        "ConnectionManager: connection {}/{} cannot access keyring for '{}'",
                        package_id,
                        conn_def.id,
                        cred_key
                    );
                    return false;
                }
            }
        }

        true
    }

    /// Start a connection by package and connection ID.
    ///
    /// Looks up the package manifest to find the connection definition,
    /// then delegates to the internal start method.
    pub async fn start_connection(&mut self, package_id: &str, connection_id: &str) {
        let pkg_data = {
            let rt = self.state.as_ref().unwrap().tool_runtime.read().await;
            rt.list_marketplace_packages()
                .into_iter()
                .find(|p| p.manifest.name == package_id)
                .map(|p| (p.manifest.clone(), p.dir.clone()))
        };

        let (manifest, pkg_dir) = match pkg_data {
            Some(data) => data,
            None => {
                tracing::error!(
                    "ConnectionManager: package '{}' not found",
                    package_id
                );
                return;
            }
        };

        let conn_def = match manifest.connections.iter().find(|c| c.id == connection_id) {
            Some(c) => c.clone(),
            None => {
                tracing::error!(
                    "ConnectionManager: connection '{}' not found in package '{}'",
                    connection_id,
                    package_id
                );
                return;
            }
        };

        self.start_connection_internal(package_id, connection_id, conn_def, pkg_dir)
            .await;
    }

    /// Internal: spawn the connection script subprocess.
    async fn start_connection_internal(
        &mut self,
        package_id: &str,
        connection_id: &str,
        conn_def: PackageConnection,
        package_dir: PathBuf,
    ) {
        let key = connection_key(package_id, connection_id);

        // Stop existing connection if running
        if self.connections.contains_key(&key) {
            self.stop_connection(&key).await;
        }

        // Update status to Starting
        self.update_status_in_db(package_id, connection_id, ConnectionStatus::Starting, None)
            .await;

        // Resolve the script path
        let script_path = package_dir.join(&conn_def.script);
        if !script_path.exists() {
            let err = format!(
                "Connection script not found: {}",
                script_path.display()
            );
            tracing::error!("ConnectionManager: {}", err);
            self.update_status_in_db(
                package_id,
                connection_id,
                ConnectionStatus::Error,
                Some(&err),
            )
            .await;
            return;
        }

        // Determine the runtime command
        let (cmd, _ext) = conn_def.runtime.command_and_ext();
        let cmd = if conn_def.runtime == RuntimeType::Python {
            find_python().await
        } else {
            cmd.to_string()
        };

        // Build the subprocess command
        let mut command = Command::new(&cmd);
        command
            .arg(script_path.to_string_lossy().as_ref())
            .current_dir(&package_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // Set environment variables
        command.env("CHITTY_PACKAGE_ID", package_id);
        command.env("CHITTY_CONNECTION_ID", connection_id);
        command.env("PYTHONIOENCODING", "utf-8");

        // Inject credentials from keyring
        for cred_key in &conn_def.requires_credentials {
            if let Ok(entry) = keyring::Entry::new("chitty-workspace", cred_key) {
                if let Ok(password) = entry.get_password() {
                    // Convert credential key to env var name: "slack_bot_token" → "CHITTY_CRED_SLACK_BOT_TOKEN"
                    let env_name = format!("CHITTY_CRED_{}", cred_key.to_uppercase().replace('-', "_"));
                    command.env(&env_name, &password);
                }
            }
        }

        // On Windows, prevent console window from flashing
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }

        // Spawn the process
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                let err = format!(
                    "Failed to spawn connection script (is {} installed?): {}",
                    cmd, e
                );
                tracing::error!("ConnectionManager: {}", err);
                self.update_status_in_db(
                    package_id,
                    connection_id,
                    ConnectionStatus::Error,
                    Some(&err),
                )
                .await;
                return;
            }
        };

        tracing::info!(
            "ConnectionManager: spawned {} for connection {} (pid: {:?})",
            cmd,
            key,
            child.id()
        );

        // Take ownership of stdin for sending messages
        let stdin = child.stdin.take();

        // Take stdout for reading NDJSON messages
        let stdout = child.stdout.take();

        // Take stderr for logging
        let stderr = child.stderr.take();

        // Store the active connection
        let active = ActiveConnection {
            key: key.clone(),
            package_id: package_id.to_string(),
            connection_id: connection_id.to_string(),
            child: Some(child),
            stdin_tx: stdin,
            status: ConnectionStatus::Starting,
            last_heartbeat: None,
            restart_count: 0,
            max_restarts: conn_def.max_restarts,
            restart_delay_secs: conn_def.restart_delay_secs,
            health_interval_secs: conn_def.health_interval_secs,
            restart_on_failure: conn_def.restart_on_failure,
            error_message: None,
            package_dir: package_dir.clone(),
            connection_config: conn_def,
        };

        self.connections.insert(key.clone(), active);

        // Spawn stdout reader task
        if let Some(stdout) = stdout {
            let state = Arc::clone(self.state.as_ref().unwrap());
            let key_clone = key.clone();
            tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();

                while let Ok(Some(line)) = lines.next_line().await {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    handle_stdout_line(&state, &key_clone, &line).await;
                }

                tracing::info!(
                    "ConnectionManager: stdout reader exited for connection {}",
                    key_clone
                );
            });
        }

        // Spawn stderr reader task (log output only)
        if let Some(stderr) = stderr {
            let key_clone = key.clone();
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();

                while let Ok(Some(line)) = lines.next_line().await {
                    let line = line.trim().to_string();
                    if !line.is_empty() {
                        tracing::warn!(
                            "ConnectionManager [{}] stderr: {}",
                            key_clone,
                            line
                        );
                    }
                }
            });
        }
    }

    /// Stop a connection gracefully: send shutdown message, wait briefly, then kill.
    pub async fn stop_connection(&mut self, key: &str) {
        let conn = match self.connections.get_mut(key) {
            Some(c) => c,
            None => {
                tracing::warn!(
                    "ConnectionManager: cannot stop unknown connection '{}'",
                    key
                );
                return;
            }
        };

        tracing::info!("ConnectionManager: stopping connection {}", key);
        conn.status = ConnectionStatus::Stopping;

        // Send shutdown message via stdin
        if let Some(ref mut stdin) = conn.stdin_tx {
            let msg = PlatformMessage::Shutdown;
            if let Ok(json) = serde_json::to_string(&msg) {
                let line = format!("{}\n", json);
                let _ = stdin.write_all(line.as_bytes()).await;
                let _ = stdin.flush().await;
            }
        }

        // Give the script a few seconds to exit gracefully
        if let Some(ref mut child) = conn.child {
            match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                Ok(Ok(status)) => {
                    tracing::info!(
                        "ConnectionManager: connection {} exited with status: {}",
                        key,
                        status
                    );
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        "ConnectionManager: error waiting for connection {} to exit: {}",
                        key,
                        e
                    );
                }
                Err(_) => {
                    // Timeout — force kill
                    tracing::warn!(
                        "ConnectionManager: connection {} did not exit gracefully, killing",
                        key
                    );
                    let _ = child.kill().await;
                }
            }
        }

        // Update DB status
        let pkg_id = conn.package_id.clone();
        let conn_id = conn.connection_id.clone();
        self.update_status_in_db(&pkg_id, &conn_id, ConnectionStatus::Stopped, None)
            .await;

        // Remove from active connections
        self.connections.remove(key);
    }

    /// Stop all active connections (called during graceful shutdown).
    pub async fn stop_all(&mut self) {
        let keys: Vec<String> = self.connections.keys().cloned().collect();
        for key in keys {
            self.stop_connection(&key).await;
        }
    }

    /// Monitor all connections: check heartbeats, restart failed processes.
    async fn monitor_connections(&mut self) {
        let now = Instant::now();
        let mut to_restart: Vec<(String, String, PackageConnection, PathBuf)> = Vec::new();
        let mut to_remove: Vec<String> = Vec::new();
        let mut status_updates: Vec<(String, String, ConnectionStatus, Option<String>)> = Vec::new();

        for (key, conn) in &mut self.connections {
            // Skip connections that are starting, stopping, or stopped
            if matches!(
                conn.status,
                ConnectionStatus::Starting | ConnectionStatus::Stopping | ConnectionStatus::Stopped
            ) {
                continue;
            }

            // Check if the child process has exited
            let process_exited = if let Some(ref mut child) = conn.child {
                match child.try_wait() {
                    Ok(Some(_status)) => true,
                    Ok(None) => false,
                    Err(e) => {
                        tracing::error!(
                            "ConnectionManager: error checking process status for {}: {}",
                            key,
                            e
                        );
                        true
                    }
                }
            } else {
                true
            };

            // Check heartbeat timeout (only for connected status)
            let heartbeat_expired = if conn.status == ConnectionStatus::Connected {
                if let Some(last_hb) = conn.last_heartbeat {
                    let timeout = Duration::from_secs(conn.health_interval_secs as u64 * 3);
                    now.duration_since(last_hb) > timeout
                } else {
                    false
                }
            } else {
                false
            };

            if process_exited || heartbeat_expired {
                let reason = if process_exited {
                    "process exited"
                } else {
                    "heartbeat timeout"
                };

                tracing::warn!(
                    "ConnectionManager: connection {} failed ({}), restart_count={}/{}",
                    key,
                    reason,
                    conn.restart_count,
                    conn.max_restarts
                );

                if conn.restart_on_failure && conn.restart_count < conn.max_restarts {
                    conn.restart_count += 1;
                    conn.status = ConnectionStatus::Reconnecting;

                    to_restart.push((
                        conn.package_id.clone(),
                        conn.connection_id.clone(),
                        conn.connection_config.clone(),
                        conn.package_dir.clone(),
                    ));
                } else {
                    let err = format!(
                        "Connection failed ({}) after {} restarts",
                        reason, conn.restart_count
                    );
                    conn.status = ConnectionStatus::Error;
                    conn.error_message = Some(err.clone());

                    // Queue status update (can't call async self method while iterating)
                    status_updates.push((
                        conn.package_id.clone(),
                        conn.connection_id.clone(),
                        ConnectionStatus::Error,
                        Some(err),
                    ));

                    to_remove.push(key.clone());
                }
            }
        }

        // Remove permanently failed connections
        for key in to_remove {
            self.connections.remove(&key);
        }

        // Persist status updates (deferred from the iteration loop)
        for (pkg, cid, status, err) in status_updates {
            self.update_status_in_db(&pkg, &cid, status, err.as_deref())
                .await;
        }

        // Restart connections that need it (with delay)
        for (pkg_id, conn_id, conn_def, pkg_dir) in to_restart {
            let delay = conn_def.restart_delay_secs;
            tracing::info!(
                "ConnectionManager: restarting {}/{} in {}s",
                pkg_id,
                conn_id,
                delay
            );

            // Preserve restart count before removing the old connection
            let key = connection_key(&pkg_id, &conn_id);
            let restart_count = self
                .connections
                .get(&key)
                .map(|c| c.restart_count)
                .unwrap_or(0);

            // Remove the stale entry
            self.connections.remove(&key);

            self.update_status_in_db(&pkg_id, &conn_id, ConnectionStatus::Reconnecting, None)
                .await;

            // Wait before restarting
            tokio::time::sleep(Duration::from_secs(delay as u64)).await;

            // Restart the connection
            self.start_connection_internal(&pkg_id, &conn_id, conn_def, pkg_dir)
                .await;

            // Restore restart count
            if let Some(conn) = self.connections.get_mut(&key) {
                conn.restart_count = restart_count;
            }
        }
    }

    /// Send a response back to a connection script via its stdin pipe.
    pub async fn send_response(
        &mut self,
        key: &str,
        correlation_id: &str,
        data: serde_json::Value,
    ) -> anyhow::Result<()> {
        let conn = self
            .connections
            .get_mut(key)
            .ok_or_else(|| anyhow::anyhow!("Connection '{}' not found", key))?;

        let msg = PlatformMessage::Response {
            correlation_id: correlation_id.to_string(),
            data,
        };

        let json = serde_json::to_string(&msg)?;
        let line = format!("{}\n", json);

        if let Some(ref mut stdin) = conn.stdin_tx {
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
            tracing::info!(
                "ConnectionManager: sent response to {} (correlation: {})",
                key,
                correlation_id
            );
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Connection '{}' has no stdin pipe available",
                key
            ))
        }
    }

    /// Update connection status in the SQLite `connection_status` table.
    async fn update_status_in_db(
        &self,
        package_id: &str,
        connection_id: &str,
        status: ConnectionStatus,
        error_message: Option<&str>,
    ) {
        let db = self.state.as_ref().unwrap().db.clone();
        let key = connection_key(package_id, connection_id);
        let key_for_log = key.clone();
        let pkg_id = package_id.to_string();
        let conn_id = connection_id.to_string();
        let status_str = status.to_string();
        let err_msg = error_message.map(|s| s.to_string());

        let result = db
            .with_conn(move |conn| {
                conn.execute(
                    "INSERT INTO connection_status (id, package_id, connection_id, status, error_message, started_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, CASE WHEN ?4 = 'connected' THEN datetime('now') ELSE NULL END, datetime('now'))
                     ON CONFLICT(package_id, connection_id) DO UPDATE SET
                         status = ?4,
                         error_message = ?5,
                         started_at = CASE WHEN ?4 = 'connected' THEN datetime('now') ELSE started_at END,
                         updated_at = datetime('now')",
                    rusqlite::params![key, pkg_id, conn_id, status_str, err_msg],
                )?;
                Ok(())
            })
            .await;

        if let Err(e) = result {
            tracing::error!(
                "ConnectionManager: failed to update status in DB for {}: {}",
                key_for_log,
                e
            );
        }
    }

    /// Update the last_heartbeat timestamp in the database.
    async fn update_heartbeat_in_db(db: &Database, package_id: &str, connection_id: &str) {
        let db = db.clone();
        let pkg_id = package_id.to_string();
        let conn_id = connection_id.to_string();

        let _ = db
            .with_conn(move |conn| {
                conn.execute(
                    "UPDATE connection_status SET last_heartbeat = datetime('now'), updated_at = datetime('now')
                     WHERE package_id = ?1 AND connection_id = ?2",
                    rusqlite::params![pkg_id, conn_id],
                )?;
                Ok(())
            })
            .await;
    }

    /// Get the status of all connections (for API endpoints).
    pub fn list_connections(&self) -> Vec<ConnectionInfo> {
        self.connections
            .values()
            .map(|c| ConnectionInfo {
                key: c.key.clone(),
                package_id: c.package_id.clone(),
                connection_id: c.connection_id.clone(),
                status: c.status,
                restart_count: c.restart_count,
                error_message: c.error_message.clone(),
                label: c.connection_config.label.clone(),
            })
            .collect()
    }
}

/// Summary info for API responses.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionInfo {
    pub key: String,
    pub package_id: String,
    pub connection_id: String,
    pub status: ConnectionStatus,
    pub restart_count: u32,
    pub error_message: Option<String>,
    pub label: String,
}

// ---------------------------------------------------------------------------
// Stdout line handler (runs in a spawned task)
// ---------------------------------------------------------------------------

/// Parse and dispatch a single NDJSON line from a connection script's stdout.
async fn handle_stdout_line(state: &Arc<AppState>, key: &str, line: &str) {
    let msg: ScriptMessage = match serde_json::from_str(line) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                "ConnectionManager [{}]: failed to parse stdout line: {} (line: {})",
                key,
                e,
                &line[..line.len().min(200)]
            );
            return;
        }
    };

    // Parse the key back into package_id and connection_id
    let (package_id, connection_id) = match key.split_once(':') {
        Some((p, c)) => (p, c),
        None => {
            tracing::error!("ConnectionManager: invalid connection key '{}'", key);
            return;
        }
    };

    match msg {
        ScriptMessage::Ready { message } => {
            tracing::info!(
                "ConnectionManager [{}]: READY — {}",
                key,
                message.as_deref().unwrap_or("(no message)")
            );

            // Update in-memory status would require mutable access to the manager;
            // since this runs in a spawned task, we update the DB directly.
            // The monitor loop will pick up the status change.
            update_status_db(
                &state.db,
                package_id,
                connection_id,
                ConnectionStatus::Connected,
                None,
            )
            .await;
        }

        ScriptMessage::Heartbeat => {
            tracing::trace!("ConnectionManager [{}]: heartbeat", key);
            ConnectionManager::update_heartbeat_in_db(&state.db, package_id, connection_id).await;
        }

        ScriptMessage::Event {
            event_id,
            correlation_id,
            data,
        } => {
            let cid = correlation_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            tracing::info!(
                "ConnectionManager [{}]: event '{}' (correlation: {})",
                key,
                event_id,
                cid
            );
            handle_event(state, key, package_id, connection_id, &event_id, &cid, data).await;
        }

        ScriptMessage::Log { level, message } => match level.as_str() {
            "error" => tracing::error!("ConnectionManager [{}]: {}", key, message),
            "warn" | "warning" => tracing::warn!("ConnectionManager [{}]: {}", key, message),
            "debug" => tracing::debug!("ConnectionManager [{}]: {}", key, message),
            _ => tracing::info!("ConnectionManager [{}]: {}", key, message),
        },

        ScriptMessage::Error { message, fatal } => {
            if fatal {
                tracing::error!(
                    "ConnectionManager [{}]: FATAL error: {}",
                    key,
                    message
                );
                update_status_db(
                    &state.db,
                    package_id,
                    connection_id,
                    ConnectionStatus::Error,
                    Some(&message),
                )
                .await;
            } else {
                tracing::warn!(
                    "ConnectionManager [{}]: non-fatal error: {}",
                    key,
                    message
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Event routing
// ---------------------------------------------------------------------------

/// Route an incoming event to the configured agent.
///
/// Looks up the `connection_event_routes` table to find which agent should
/// handle this event, then invokes the agent and sends the response back
/// to the connection script.
async fn handle_event(
    state: &Arc<AppState>,
    key: &str,
    package_id: &str,
    connection_id: &str,
    event_id: &str,
    correlation_id: &str,
    data: serde_json::Value,
) {
    // Look up the route in the database
    let route = {
        let db = state.db.clone();
        let pkg_id = package_id.to_string();
        let conn_id = connection_id.to_string();
        let evt_id = event_id.to_string();

        db.with_conn(move |conn| {
            let result = conn.query_row(
                "SELECT agent_id, provider, model, auto_approve, enabled
                 FROM connection_event_routes
                 WHERE package_id = ?1 AND connection_id = ?2 AND event_id = ?3",
                rusqlite::params![pkg_id, conn_id, evt_id],
                |row| {
                    Ok(EventRoute {
                        agent_id: row.get(0)?,
                        provider: row.get(1)?,
                        model: row.get(2)?,
                        auto_approve: row.get::<_, bool>(3)?,
                        enabled: row.get::<_, bool>(4)?,
                    })
                },
            );

            match result {
                Ok(route) => Ok(Some(route)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(anyhow::anyhow!("DB error looking up event route: {}", e)),
            }
        })
        .await
    };

    let route = match route {
        Ok(Some(r)) => r,
        Ok(None) => {
            tracing::info!(
                "ConnectionManager [{}]: no route configured for event '{}'",
                key,
                event_id
            );
            return;
        }
        Err(e) => {
            tracing::error!(
                "ConnectionManager [{}]: failed to look up route for event '{}': {}",
                key,
                event_id,
                e
            );
            return;
        }
    };

    if !route.enabled {
        tracing::info!(
            "ConnectionManager [{}]: route for event '{}' is disabled",
            key,
            event_id
        );
        return;
    }

    let agent_id = match route.agent_id {
        Some(id) if !id.is_empty() => id,
        _ => {
            tracing::info!(
                "ConnectionManager [{}]: no agent assigned for event '{}'",
                key,
                event_id
            );
            return;
        }
    };

    tracing::info!(
        "ConnectionManager [{}]: routing event '{}' to agent '{}' (correlation: {})",
        key,
        event_id,
        agent_id,
        correlation_id
    );

    // Execute the agent for this event
    let response = execute_agent_for_event(
        state,
        &agent_id,
        route.provider.as_deref(),
        route.model.as_deref(),
        event_id,
        correlation_id,
        &data,
    )
    .await;

    // Send the response back to the connection script
    // Note: We cannot mutate the ConnectionManager from a spawned task directly.
    // The response is sent by writing to the stdin pipe stored in the manager.
    // For now, we log the response. A production implementation would use a
    // channel to send the response back to the manager's main loop.
    match response {
        Ok(response_data) => {
            tracing::info!(
                "ConnectionManager [{}]: agent response for correlation {} ready",
                key,
                correlation_id
            );
            // TODO: Send response back via stdin using a channel to the ConnectionManager.
            // The current architecture spawns stdout readers as separate tasks, so we need
            // a way to route responses back. For now, log the response.
            tracing::info!(
                "ConnectionManager [{}]: response data: {}",
                key,
                serde_json::to_string(&response_data).unwrap_or_default()
            );
        }
        Err(e) => {
            tracing::error!(
                "ConnectionManager [{}]: agent execution failed for event '{}': {}",
                key,
                event_id,
                e
            );
        }
    }
}

/// Route configuration from the `connection_event_routes` table.
#[derive(Debug)]
struct EventRoute {
    agent_id: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    auto_approve: bool,
    enabled: bool,
}

/// Execute an agent to handle a connection event.
///
/// TODO: Wire this into the ChatEngine directly. For now this is a placeholder
/// that constructs the event context and returns a simulated response. The full
/// implementation should:
/// 1. Load the agent from the database
/// 2. Create a temporary conversation context with the event data
/// 3. Run the ChatEngine with the agent's persona, skills, and tools
/// 4. Return the agent's response text
async fn execute_agent_for_event(
    state: &Arc<AppState>,
    agent_id: &str,
    _provider: Option<&str>,
    _model: Option<&str>,
    event_id: &str,
    correlation_id: &str,
    data: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    // Verify the agent exists
    let db = state.db.clone();
    let aid = agent_id.to_string();
    let agent_exists = db
        .with_conn(move |conn| {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM agents WHERE id = ?1",
                    rusqlite::params![aid],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            Ok(exists)
        })
        .await?;

    if !agent_exists {
        return Err(anyhow::anyhow!("Agent '{}' not found", agent_id));
    }

    tracing::info!(
        "ConnectionManager: executing agent '{}' for event '{}' (correlation: {})",
        agent_id,
        event_id,
        correlation_id
    );

    // TODO: Invoke the ChatEngine with the agent's configuration.
    // This requires:
    // 1. Loading the agent (persona, skills, preferred_provider, preferred_model)
    // 2. Creating a ChatEngine instance with the event data as the user message
    // 3. Running the engine and collecting the response
    // 4. Returning the response as JSON
    //
    // For now, return a placeholder acknowledging the event:
    Ok(serde_json::json!({
        "text": format!("Event '{}' received and acknowledged. Agent execution not yet implemented.", event_id),
        "event_id": event_id,
        "correlation_id": correlation_id,
        "agent_id": agent_id,
        "status": "placeholder"
    }))
}

// ---------------------------------------------------------------------------
// DB helpers (standalone functions for use in spawned tasks)
// ---------------------------------------------------------------------------

/// Update connection status in the database (standalone, for use in spawned tasks).
async fn update_status_db(
    db: &Database,
    package_id: &str,
    connection_id: &str,
    status: ConnectionStatus,
    error_message: Option<&str>,
) {
    let db = db.clone();
    let key = connection_key(package_id, connection_id);
    let pkg_id = package_id.to_string();
    let conn_id = connection_id.to_string();
    let status_str = status.to_string();
    let err_msg = error_message.map(|s| s.to_string());

    let result = db
        .with_conn(move |conn| {
            conn.execute(
                "INSERT INTO connection_status (id, package_id, connection_id, status, error_message, started_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, CASE WHEN ?4 = 'connected' THEN datetime('now') ELSE NULL END, datetime('now'))
                 ON CONFLICT(package_id, connection_id) DO UPDATE SET
                     status = ?4,
                     error_message = ?5,
                     started_at = CASE WHEN ?4 = 'connected' THEN datetime('now') ELSE started_at END,
                     updated_at = datetime('now')",
                rusqlite::params![key, pkg_id, conn_id, status_str, err_msg],
            )?;
            Ok(())
        })
        .await;

    if let Err(e) = result {
        tracing::error!(
            "ConnectionManager: failed to update status in DB for {}:{}: {}",
            package_id,
            connection_id,
            e
        );
    }
}

// ---------------------------------------------------------------------------
// Utility: find Python interpreter
// ---------------------------------------------------------------------------

/// Find the Python interpreter on this system (mirrors executor.rs logic).
async fn find_python() -> String {
    for candidate in &["python3", "python", "py"] {
        let result = Command::new(candidate)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;

        if let Ok(status) = result {
            if status.success() {
                return candidate.to_string();
            }
        }
    }

    if cfg!(target_os = "windows") {
        "python".to_string()
    } else {
        "python3".to_string()
    }
}
