//! Tool Runtime — unified dispatch for native, custom, and connection tools
//!
//! Wraps the existing ToolRegistry (native tools) and adds support for:
//! - Custom tools loaded from ~/.chitty-workspace/tools/custom/
//! - Connection tools loaded from ~/.chitty-workspace/tools/connections/
//!
//! The runtime scans the filesystem for manifest.json files and makes all
//! discovered tools available through a single execute() interface.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::server::BrowserBridge;
use crate::tools::executor;
use crate::tools::manifest::{PackageManifest, ToolManifest, ToolType};
use crate::tools::{ToolCategory, ToolContext, ToolDefinition, ToolRegistry, ToolResult};

/// A custom tool loaded from disk
#[derive(Debug, Clone)]
pub struct LoadedCustomTool {
    pub manifest: ToolManifest,
    pub dir: PathBuf,
}

/// A connection tool loaded from disk
#[derive(Debug, Clone)]
pub struct LoadedConnection {
    pub manifest: ToolManifest,
    pub dir: PathBuf,
}

/// A loaded marketplace package (vendor bundle of tools)
#[derive(Debug, Clone)]
pub struct MarketplacePackage {
    pub manifest: PackageManifest,
    pub dir: PathBuf,
}

/// Unified tool runtime — dispatches to native, custom, or connection tools
pub struct ToolRuntime {
    /// Native tools (compiled into the binary)
    pub native_registry: ToolRegistry,
    /// Custom tools (scripts on disk)
    custom_tools: HashMap<String, LoadedCustomTool>,
    /// Connection tools (API integrations)
    connections: HashMap<String, LoadedConnection>,
    /// Marketplace packages (vendor bundles)
    pub marketplace_packages: Vec<MarketplacePackage>,
    /// Maps marketplace tool name → package vendor name (for categorization)
    marketplace_tool_vendors: HashMap<String, String>,
    /// Maps marketplace tool name → package name (for workspace lookup)
    marketplace_tool_packages: HashMap<String, String>,
    /// Cached package configs (allowed resources + feature flags) keyed by tool name → config JSON
    package_configs: HashMap<String, String>,
    /// Root tools directory
    tools_dir: PathBuf,
    /// Sandbox temp directory for custom tool execution
    sandbox_dir: PathBuf,
    /// Packages directory for isolated dependencies
    packages_dir: PathBuf,
    /// Persistent workspace directory for package file storage
    workspace_dir: PathBuf,
}

impl ToolRuntime {
    /// Create a new ToolRuntime, scanning the filesystem for tools
    pub fn new(data_dir: &Path, browser_bridge: Arc<BrowserBridge>, skill_registry: Arc<crate::skills::SkillRegistry>) -> anyhow::Result<Self> {
        let tools_dir = data_dir.join("tools");
        let sandbox_dir = data_dir.join("sandbox");
        let packages_dir = data_dir.join("packages");
        let workspace_dir = data_dir.join("workspaces");

        // Ensure directories exist
        std::fs::create_dir_all(tools_dir.join("custom"))?;
        std::fs::create_dir_all(tools_dir.join("connections"))?;
        std::fs::create_dir_all(&sandbox_dir)?;
        std::fs::create_dir_all(packages_dir.join("python"))?;
        std::fs::create_dir_all(packages_dir.join("node"))?;
        std::fs::create_dir_all(&workspace_dir)?;

        // Media directories for generated images, videos, and audio
        let media_dir = data_dir.join("media");
        std::fs::create_dir_all(media_dir.join("images"))?;
        std::fs::create_dir_all(media_dir.join("videos"))?;
        std::fs::create_dir_all(media_dir.join("audio"))?;

        let mut runtime = Self {
            native_registry: ToolRegistry::new(browser_bridge, skill_registry),
            custom_tools: HashMap::new(),
            connections: HashMap::new(),
            marketplace_packages: Vec::new(),
            marketplace_tool_vendors: HashMap::new(),
            marketplace_tool_packages: HashMap::new(),
            package_configs: HashMap::new(),
            tools_dir,
            sandbox_dir,
            packages_dir,
            workspace_dir,
        };

        // Scan for custom and connection tools
        runtime.scan_and_load();

        Ok(runtime)
    }

    /// Scan the tools directory for manifest.json files and load them
    pub fn scan_and_load(&mut self) {
        self.custom_tools.clear();
        self.connections.clear();
        self.marketplace_packages.clear();
        self.marketplace_tool_vendors.clear();
        self.marketplace_tool_packages.clear();

        // Scan custom tools
        let custom_dir = self.tools_dir.join("custom");
        if custom_dir.exists() {
            self.scan_directory(&custom_dir, ToolType::Custom);
        }

        // Scan connection tools
        let connections_dir = self.tools_dir.join("connections");
        if connections_dir.exists() {
            self.scan_directory(&connections_dir, ToolType::Connection);
        }

        // Scan marketplace packages
        self.marketplace_packages.clear();
        self.marketplace_tool_vendors.clear();
        self.marketplace_tool_packages.clear();
        let marketplace_dir = self.tools_dir.join("marketplace");
        if marketplace_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&marketplace_dir) {
                for entry in entries.flatten() {
                    let vendor_dir = entry.path();
                    if !vendor_dir.is_dir() { continue; }

                    let pkg_path = vendor_dir.join("package.json");
                    if !pkg_path.exists() { continue; }

                    match std::fs::read_to_string(&pkg_path) {
                        Ok(content) => {
                            match serde_json::from_str::<PackageManifest>(&content) {
                                Ok(pkg_manifest) => {
                                    tracing::info!("Loaded marketplace package: {} ({} tools)",
                                        pkg_manifest.display_name, pkg_manifest.tools.len());

                                    // Load each tool in the package directly
                                    for tool_name in &pkg_manifest.tools {
                                        let tool_dir = vendor_dir.join(tool_name);
                                        let manifest_path = tool_dir.join("manifest.json");
                                        if manifest_path.exists() {
                                            match std::fs::read_to_string(&manifest_path) {
                                                Ok(content) => match serde_json::from_str::<ToolManifest>(&content) {
                                                    Ok(manifest) => {
                                                        tracing::info!("Loaded marketplace tool: {} ({})", manifest.display_name, manifest.name);
                                                        self.marketplace_tool_vendors.insert(
                                                            manifest.name.clone(),
                                                            pkg_manifest.vendor.clone(),
                                                        );
                                                        self.marketplace_tool_packages.insert(
                                                            manifest.name.clone(),
                                                            pkg_manifest.name.clone(),
                                                        );
                                                        self.custom_tools.insert(
                                                            manifest.name.clone(),
                                                            LoadedCustomTool {
                                                                manifest,
                                                                dir: tool_dir,
                                                            },
                                                        );
                                                    }
                                                    Err(e) => tracing::warn!("Failed to parse tool manifest {:?}: {}", manifest_path, e),
                                                }
                                                Err(e) => tracing::warn!("Failed to read tool manifest {:?}: {}", manifest_path, e),
                                            }
                                        }
                                    }

                                    // Create persistent workspace folder for this package
                                    let pkg_workspace = self.workspace_dir.join(&pkg_manifest.name);
                                    if let Err(e) = std::fs::create_dir_all(&pkg_workspace) {
                                        tracing::warn!("Failed to create package workspace {:?}: {}", pkg_workspace, e);
                                    }

                                    self.marketplace_packages.push(MarketplacePackage {
                                        manifest: pkg_manifest,
                                        dir: vendor_dir.clone(),
                                    });
                                }
                                Err(e) => tracing::warn!("Failed to parse package.json in {:?}: {}", vendor_dir, e),
                            }
                        }
                        Err(e) => tracing::warn!("Failed to read package.json in {:?}: {}", vendor_dir, e),
                    }
                }
            }
        }

        let custom_count = self.custom_tools.len();
        let conn_count = self.connections.len();
        if custom_count > 0 || conn_count > 0 {
            tracing::info!(
                "Loaded {} custom tools, {} connections from {}",
                custom_count, conn_count, self.tools_dir.display()
            );
        }
    }

    fn scan_directory(&mut self, dir: &Path, expected_type: ToolType) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("Failed to read tools directory {}: {}", dir.display(), e);
                return;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            let tool_dir = entry.path();
            if !tool_dir.is_dir() {
                continue;
            }

            let manifest_path = tool_dir.join("manifest.json");
            if !manifest_path.exists() {
                continue;
            }

            match std::fs::read_to_string(&manifest_path) {
                Ok(content) => match serde_json::from_str::<ToolManifest>(&content) {
                    Ok(manifest) => {
                        if let Err(e) = manifest.validate() {
                            tracing::warn!("Invalid manifest in {}: {}", tool_dir.display(), e);
                            continue;
                        }

                        match manifest.tool_type {
                            ToolType::Custom => {
                                tracing::info!("Loaded custom tool: {} ({})", manifest.display_name, manifest.name);
                                self.custom_tools.insert(
                                    manifest.name.clone(),
                                    LoadedCustomTool {
                                        manifest,
                                        dir: tool_dir,
                                    },
                                );
                            }
                            ToolType::Connection => {
                                // For connections, register each action as a separate tool
                                tracing::info!("Loaded connection: {} ({})", manifest.display_name, manifest.name);
                                self.connections.insert(
                                    manifest.name.clone(),
                                    LoadedConnection {
                                        manifest,
                                        dir: tool_dir,
                                    },
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse manifest {}: {}", manifest_path.display(), e);
                    }
                },
                Err(e) => {
                    tracing::warn!("Failed to read manifest {}: {}", manifest_path.display(), e);
                }
            }
        }
    }

    /// List all tool definitions (native + custom + connection actions)
    pub fn list_definitions(&self) -> Vec<ToolDefinition> {
        let mut defs = self.native_registry.list_definitions();

        // Add custom tools (distinguish marketplace vs user-created)
        for (_, tool) in &self.custom_tools {
            let vendor = self.marketplace_tool_vendors.get(&tool.manifest.name).cloned();
            let category = if vendor.is_some() {
                ToolCategory::Marketplace
            } else {
                ToolCategory::Custom
            };
            defs.push(ToolDefinition {
                name: tool.manifest.name.clone(),
                display_name: tool.manifest.display_name.clone(),
                description: tool.manifest.description.clone(),
                parameters: tool.manifest.to_json_schema(),
                instructions: tool.manifest.instructions.clone(),
                category,
                vendor,
            });
        }

        // Add connection tool actions
        for (_, conn) in &self.connections {
            if let Some(ref actions) = conn.manifest.actions {
                for action in actions {
                    let tool_name = format!("{}.{}", conn.manifest.name, action.name);
                    let description = format!("{} — {}", conn.manifest.display_name, action.description);

                    // Build JSON schema from action parameters
                    let mut properties = serde_json::Map::new();
                    let mut required = Vec::new();
                    for (name, param) in &action.parameters {
                        let mut prop = serde_json::Map::new();
                        prop.insert("type".to_string(), serde_json::Value::String(param.param_type.clone()));
                        if !param.description.is_empty() {
                            prop.insert("description".to_string(), serde_json::Value::String(param.description.clone()));
                        }
                        properties.insert(name.clone(), serde_json::Value::Object(prop));
                        if param.required {
                            required.push(serde_json::Value::String(name.clone()));
                        }
                    }

                    defs.push(ToolDefinition {
                        name: tool_name,
                        display_name: format!("{}: {}", conn.manifest.display_name, action.name),
                        description,
                        parameters: serde_json::json!({
                            "type": "object",
                            "properties": properties,
                            "required": required,
                        }),
                        instructions: conn.manifest.instructions.clone(),
                        category: ToolCategory::Integration,
                        vendor: None,
                    });
                }
            }
        }

        // Add the open_agent_panel UI command tool (handled by frontend, not tool_runtime)
        defs.push(ToolDefinition {
            name: "open_agent_panel".to_string(),
            display_name: "Open Agent Panel".to_string(),
            description: "Open a SPECIFIC EXISTING agent in a NEW chat panel. ONLY use when the user EXPLICITLY asks to open or launch a different agent in a new panel. NEVER use this for normal conversation, answering questions about agents, or when the user asks you to do something yourself.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": {
                        "type": "string",
                        "description": "The ID of the agent to open. Leave empty to open the default Chitty agent."
                    },
                    "message": {
                        "type": "string",
                        "description": "An optional message to automatically send to the agent once the panel opens."
                    }
                }
            }),
            instructions: Some("IMPORTANT: Only use open_agent_panel when the user EXPLICITLY requests opening a specific agent in a new panel (e.g. 'open the Code Reviewer in a new panel', 'launch my Data Agent'). Do NOT use this tool to answer questions about agents, list agents, describe agents, or handle any task yourself. If the user asks 'can you see my agents' or 'what agents do I have', use file_reader or terminal to check — do NOT open panels.".to_string()),
            category: ToolCategory::Native,
            vendor: None,
        });

        defs
    }

    /// Get definitions for specific tool names
    pub fn get_definitions(&self, names: &[String]) -> Vec<ToolDefinition> {
        let all = self.list_definitions();
        all.into_iter()
            .filter(|d| names.contains(&d.name))
            .collect()
    }

    /// Convert tool definitions to OpenAI function calling format
    pub fn to_openai_format(&self, names: Option<&[String]>) -> Vec<serde_json::Value> {
        let defs = match names {
            Some(n) => self.get_definitions(n),
            None => self.list_definitions(),
        };

        defs.into_iter()
            .map(|d| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": d.name,
                        "description": d.description,
                        "parameters": d.parameters,
                    }
                })
            })
            .collect()
    }

    /// Build agent instructions for the system prompt
    pub fn build_agent_instructions(&self, names: Option<&[String]>) -> String {
        let defs = match names {
            Some(n) => self.get_definitions(n),
            None => self.list_definitions(),
        };

        let parts: Vec<String> = defs
            .iter()
            .filter_map(|d| {
                d.instructions
                    .as_ref()
                    .map(|inst| format!("### {}\n{}", d.display_name, inst))
            })
            .collect();

        if parts.is_empty() {
            return String::new();
        }

        format!("\n\n## Tool Instructions\n\n{}", parts.join("\n\n"))
    }

    /// Execute a tool by name — dispatches to native, custom, or connection
    pub async fn execute(
        &self,
        name: &str,
        args: &serde_json::Value,
        ctx: &ToolContext,
    ) -> (ToolResult, u64) {
        let start = Instant::now();

        let result = if self.native_registry.has_tool(name) {
            // Native tool
            let (res, _) = self.native_registry.execute(name, args, ctx).await;
            res
        } else if let Some(tool) = self.custom_tools.get(name) {
            // Custom tool — execute script
            tracing::info!("Executing custom tool: {}", name);

            // Pre-refresh OAuth tokens for marketplace tools (so Python SDK gets fresh tokens)
            if let Some(pkg_name) = self.marketplace_tool_packages.get(name) {
                let provider = if pkg_name.contains("google") || pkg_name.contains("gmail") || pkg_name.contains("calendar") || pkg_name.contains("cloud") {
                    Some("google")
                } else if pkg_name.contains("slack") {
                    Some("slack")
                } else {
                    None
                };
                if let Some(p) = provider {
                    match crate::oauth::get_access_token(p).await {
                        Ok(_) => tracing::debug!("OAuth token refreshed for {} (package: {})", p, pkg_name),
                        Err(e) => tracing::warn!("OAuth token refresh failed for {}: {}", p, e),
                    }
                }
            }

            let pkg_config = self.package_configs.get(name).map(|s| s.as_str());
            // Resolve package workspace: if this is a marketplace tool, use its package workspace
            let pkg_workspace = self.marketplace_tool_packages.get(name)
                .map(|pkg_name| self.workspace_dir.join(pkg_name));
            executor::execute_custom(
                &tool.manifest,
                &tool.dir,
                args,
                &self.sandbox_dir,
                &self.packages_dir,
                pkg_config,
                pkg_workspace.as_deref(),
            )
            .await
        } else if name.contains('.') {
            // Connection tool action (format: connection_name.action_name)
            let parts: Vec<&str> = name.splitn(2, '.').collect();
            if parts.len() == 2 {
                let conn_name = parts[0];
                let action_name = parts[1];
                self.execute_connection(conn_name, action_name, args).await
            } else {
                ToolResult::err(format!("Unknown tool: {}", name))
            }
        } else {
            // Suggest closest matching tool name
            let all_names = self.list_definitions().into_iter().map(|d| d.name).collect::<Vec<_>>();
            let suggestion = all_names.iter()
                .filter(|n| n.contains(name) || name.contains(n.as_str()))
                .next()
                .cloned()
                .unwrap_or_default();
            let hint = if !suggestion.is_empty() {
                format!(". Did you mean '{}'?", suggestion)
            } else {
                let available = all_names.iter()
                    .filter(|n| {
                        // Simple similarity: shared prefix or contains
                        let n_lower = n.to_lowercase();
                        let name_lower = name.to_lowercase();
                        n_lower.starts_with(&name_lower[..name_lower.len().min(3).max(1)])
                            || n_lower.contains(&name_lower)
                            || name_lower.contains(&n_lower)
                    })
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>();
                if !available.is_empty() {
                    format!(". Similar tools: {}", available.join(", "))
                } else {
                    format!(". Available tools: {}", all_names.join(", "))
                }
            };
            ToolResult::err(format!("Unknown tool: '{}'{}", name, hint))
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        (result, duration_ms)
    }

    /// Execute a connection tool action
    async fn execute_connection(
        &self,
        conn_name: &str,
        action_name: &str,
        args: &serde_json::Value,
    ) -> ToolResult {
        let conn = match self.connections.get(conn_name) {
            Some(c) => c,
            None => return ToolResult::err(format!("Unknown connection: {}", conn_name)),
        };

        let _action = conn.manifest.actions.as_ref()
            .and_then(|actions| actions.iter().find(|a| a.name == action_name));

        let connection_config = match &conn.manifest.connection {
            Some(c) => c,
            None => return ToolResult::err(format!("Connection '{}' missing connection config", conn_name)),
        };

        // Get credentials from keyring
        let credentials = match crate::config::get_api_key(&connection_config.credentials_key) {
            Ok(Some(key)) => key,
            Ok(None) => return ToolResult::err(format!(
                "No credentials configured for '{}'. Add them in Settings > API Keys with key '{}'.",
                conn_name, connection_config.credentials_key
            )),
            Err(e) => return ToolResult::err(format!("Failed to get credentials: {}", e)),
        };

        // For now, execute via the tool's script (sidecar support comes later)
        // The script handles the API call with credentials passed via env
        let mut manifest_clone = conn.manifest.clone();
        // Merge the action parameters into the manifest for execution
        manifest_clone.name = format!("{}.{}", conn_name, action_name);

        // Execute the connection tool script with credentials in env
        let script_path = conn.dir.join(&conn.manifest.entry_point);
        if script_path.exists() {
            // Pass connection details via enhanced args
            let mut enhanced_args = args.clone();
            if let Some(obj) = enhanced_args.as_object_mut() {
                obj.insert("__action".to_string(), serde_json::Value::String(action_name.to_string()));
                obj.insert("__credentials".to_string(), serde_json::Value::String(credentials));
                if let Some(ref base_url) = connection_config.base_url {
                    obj.insert("__base_url".to_string(), serde_json::Value::String(base_url.clone()));
                }
            }

            executor::execute_custom(
                &conn.manifest,
                &conn.dir,
                &enhanced_args,
                &self.sandbox_dir,
                &self.packages_dir,
                None, // Connection tools don't use package config
                None, // Connection tools don't use package workspace
            )
            .await
        } else {
            ToolResult::err(format!(
                "Connection tool script not found: {}. The connection '{}' may need to be reinstalled.",
                script_path.display(), conn_name
            ))
        }
    }

    /// Check if a tool name exists (native, custom, or connection)
    pub fn has_tool(&self, name: &str) -> bool {
        if self.native_registry.has_tool(name) {
            return true;
        }
        if self.custom_tools.contains_key(name) {
            return true;
        }
        // Check connection actions
        if name.contains('.') {
            let parts: Vec<&str> = name.splitn(2, '.').collect();
            if parts.len() == 2 {
                if let Some(conn) = self.connections.get(parts[0]) {
                    if let Some(ref actions) = conn.manifest.actions {
                        return actions.iter().any(|a| a.name == parts[1]);
                    }
                }
            }
        }
        false
    }

    /// Get the tools directory path
    pub fn tools_dir(&self) -> &Path {
        &self.tools_dir
    }

    /// Get the packages directory path
    pub fn packages_dir(&self) -> &Path {
        &self.packages_dir
    }

    /// Load package configs from the database and cache them by tool name.
    /// Call this after saving config from the UI.
    pub fn load_package_configs(&mut self, db: &crate::storage::Database) {
        self.package_configs.clear();
        if let Ok(conn) = db.connect() {
            for pkg in &self.marketplace_packages {
                let pkg_id = &pkg.manifest.name;

                // Load resources
                let mut resources: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
                if let Ok(mut stmt) = conn.prepare(
                    "SELECT resource_type, resource_id FROM package_resources WHERE package_id = ?1"
                ) {
                    if let Ok(rows) = stmt.query_map(rusqlite::params![pkg_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    }) {
                        for row in rows.flatten() {
                            resources.entry(row.0).or_default().push(serde_json::json!(row.1));
                        }
                    }
                }

                // Load features
                let mut features: HashMap<String, bool> = HashMap::new();
                if let Ok(mut stmt) = conn.prepare(
                    "SELECT feature_id, enabled FROM package_features WHERE package_id = ?1"
                ) {
                    if let Ok(rows) = stmt.query_map(rusqlite::params![pkg_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i32>(1)?))
                    }) {
                        for row in rows.flatten() {
                            features.insert(row.0, row.1 != 0);
                        }
                    }
                }

                if !resources.is_empty() || !features.is_empty() {
                    let config_json = serde_json::json!({
                        "package_id": pkg_id,
                        "resources": resources,
                        "features": features,
                    });
                    let config_str = serde_json::to_string(&config_json).unwrap_or_default();

                    // Map each tool in this package to the config
                    for tool_name in &pkg.manifest.tools {
                        let tool_dir = pkg.dir.join(tool_name);
                        let manifest_path = tool_dir.join("manifest.json");
                        if let Ok(content) = std::fs::read_to_string(&manifest_path) {
                            if let Ok(tm) = serde_json::from_str::<crate::tools::manifest::ToolManifest>(&content) {
                                self.package_configs.insert(tm.name.clone(), config_str.clone());
                            }
                        }
                    }
                }
            }
            tracing::info!("Loaded package configs for {} tools", self.package_configs.len());
        }
    }

    /// Create a new custom tool from agent-provided definition
    pub async fn create_custom_tool(
        &mut self,
        name: &str,
        display_name: &str,
        description: &str,
        runtime: &str,
        script_content: &str,
        parameters: HashMap<String, crate::tools::manifest::ParamDef>,
        instructions: Option<String>,
    ) -> anyhow::Result<()> {
        // Validate name
        if !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
            anyhow::bail!("Tool name must be alphanumeric with underscores/hyphens only");
        }

        let runtime_type: crate::tools::manifest::RuntimeType = match runtime {
            "python" => crate::tools::manifest::RuntimeType::Python,
            "node" | "javascript" => crate::tools::manifest::RuntimeType::Node,
            "powershell" => crate::tools::manifest::RuntimeType::PowerShell,
            "shell" | "bash" | "sh" => crate::tools::manifest::RuntimeType::Shell,
            _ => anyhow::bail!("Unsupported runtime: {}", runtime),
        };

        let (_, ext) = runtime_type.command_and_ext();
        let entry_point = format!("tool{}", ext);

        let manifest = ToolManifest {
            name: name.to_string(),
            display_name: display_name.to_string(),
            description: description.to_string(),
            version: "1.0.0".to_string(),
            tool_type: ToolType::Custom,
            runtime: runtime_type,
            entry_point: entry_point.clone(),
            parameters,
            install_commands: Vec::new(),
            timeout_seconds: 30,
            permission_tier: crate::tools::manifest::PermissionTier::Moderate,
            source: crate::tools::manifest::ToolSource::AgentCreated,
            marketplace_id: None,
            instructions,
            connection: None,
            actions: None,
        };

        manifest.validate().map_err(|e| anyhow::anyhow!(e))?;

        // Create tool directory
        let tool_dir = self.tools_dir.join("custom").join(name);
        tokio::fs::create_dir_all(&tool_dir).await?;

        // Write manifest
        let manifest_json = serde_json::to_string_pretty(&manifest)?;
        tokio::fs::write(tool_dir.join("manifest.json"), &manifest_json).await?;

        // Write script
        tokio::fs::write(tool_dir.join(&entry_point), script_content).await?;

        tracing::info!("Created custom tool '{}' at {}", name, tool_dir.display());

        // Register it immediately
        self.custom_tools.insert(
            name.to_string(),
            LoadedCustomTool {
                manifest,
                dir: tool_dir,
            },
        );

        Ok(())
    }

    /// List all installed marketplace packages
    pub fn list_marketplace_packages(&self) -> Vec<&MarketplacePackage> {
        self.marketplace_packages.iter().collect()
    }
}
