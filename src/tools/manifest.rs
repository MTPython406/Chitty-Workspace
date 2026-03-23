//! Tool manifest types — describes custom tools and connection tools
//!
//! Each tool on disk has a `manifest.json` that describes:
//! - What the tool does (name, description, parameters)
//! - How to run it (runtime, entry_point, timeout)
//! - Security tier (safe, moderate, elevated)
//! - Source (agent-created, user-created, marketplace)
//!
//! Connection tools extend this with API config and sidecar settings.

use std::collections::HashMap;
use serde::{Deserialize, Serialize};

/// The manifest.json for a custom or connection tool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolManifest {
    /// Unique tool name (snake_case, alphanumeric + underscore)
    pub name: String,
    /// Human-readable display name
    pub display_name: String,
    /// Short description for LLM function calling
    pub description: String,
    /// Semver version
    #[serde(default = "default_version")]
    pub version: String,
    /// Tool type
    #[serde(alias = "type", alias = "tool_type")]
    pub tool_type: ToolType,
    /// Script runtime
    pub runtime: RuntimeType,
    /// Script file name (e.g., "tool.py", "tool.js")
    pub entry_point: String,
    /// Parameter definitions (JSON Schema style)
    #[serde(default)]
    pub parameters: HashMap<String, ParamDef>,
    /// Commands to install dependencies
    #[serde(default)]
    pub install_commands: Vec<String>,
    /// Max execution time in seconds
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u32,
    /// Security permission tier
    #[serde(default)]
    pub permission_tier: PermissionTier,
    /// Where this tool came from
    #[serde(default)]
    pub source: ToolSource,
    /// Marketplace ID (if downloaded from marketplace)
    #[serde(default)]
    pub marketplace_id: Option<String>,
    /// Agent instructions — injected into the system prompt
    #[serde(default)]
    pub instructions: Option<String>,
    /// Connection-specific config (only for type=connection)
    #[serde(default)]
    pub connection: Option<ConnectionConfig>,
    /// Actions (only for type=connection — each becomes a tool)
    #[serde(default)]
    pub actions: Option<Vec<ActionDef>>,
}

fn default_version() -> String { "1.0.0".to_string() }
fn default_timeout() -> u32 { 30 }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolType {
    Custom,
    Connection,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeType {
    Python,
    Node,
    #[serde(alias = "powershell")]
    PowerShell,
    Shell,
    Binary,
}

impl RuntimeType {
    /// Get the command and file extension for this runtime
    pub fn command_and_ext(&self) -> (&str, &str) {
        match self {
            RuntimeType::Python => {
                if cfg!(target_os = "windows") {
                    ("python", ".py")
                } else {
                    ("python3", ".py")
                }
            }
            RuntimeType::Node => ("node", ".js"),
            RuntimeType::PowerShell => {
                if cfg!(target_os = "windows") {
                    ("powershell", ".ps1")
                } else {
                    ("pwsh", ".ps1")
                }
            }
            RuntimeType::Shell => {
                if cfg!(target_os = "windows") {
                    ("cmd", ".bat")
                } else {
                    ("sh", ".sh")
                }
            }
            RuntimeType::Binary => ("", ""),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionTier {
    /// Read-only operations — auto-approved
    Safe,
    /// File writes, script execution — one-click confirm
    Moderate,
    /// Install packages, network access, system commands — explicit approval
    Elevated,
}

impl Default for PermissionTier {
    fn default() -> Self { PermissionTier::Moderate }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolSource {
    /// Created by the AI agent via create_tool
    AgentCreated,
    /// Created manually by the user
    UserCreated,
    /// Downloaded from marketplace
    Marketplace,
}

impl Default for ToolSource {
    fn default() -> Self { ToolSource::UserCreated }
}

/// Parameter definition for a tool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamDef {
    /// JSON Schema type (string, number, boolean, array, object)
    #[serde(rename = "type")]
    pub param_type: String,
    /// Human-readable description
    #[serde(default)]
    pub description: String,
    /// Whether this parameter is required
    #[serde(default)]
    pub required: bool,
    /// Default value
    #[serde(default)]
    pub default: Option<serde_json::Value>,
}

/// Connection-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionConfig {
    /// Authentication type
    pub auth_type: AuthType,
    /// Keyring key name for credentials
    pub credentials_key: String,
    /// Base URL for API calls
    #[serde(default)]
    pub base_url: Option<String>,
    /// Sidecar process configuration
    #[serde(default)]
    pub sidecar: Option<SidecarConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    ApiKey,
    OAuth2,
    ServiceAccount,
    BearerToken,
    None,
}

/// Sidecar process configuration for long-running connections
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarConfig {
    /// Whether the sidecar is enabled
    #[serde(default)]
    pub enabled: bool,
    /// Entry point script/binary
    pub entry_point: String,
    /// Port (0 = auto-assign)
    #[serde(default)]
    pub port: u16,
    /// Health check endpoint
    #[serde(default = "default_health_check")]
    pub health_check: String,
}

fn default_health_check() -> String { "/health".to_string() }

/// An action within a connection tool (each becomes a separate callable tool)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionDef {
    /// Action name (becomes tool name as connection.action)
    pub name: String,
    /// Description for LLM
    pub description: String,
    /// Parameters for this action
    #[serde(default)]
    pub parameters: HashMap<String, ParamDef>,
    /// HTTP method and path (e.g., "POST /chat.postMessage")
    #[serde(default)]
    pub endpoint: Option<String>,
}

/// Marketplace package manifest (package.json)
///
/// Standard integration framework for community-contributed packages.
/// A package bundles tools, authentication, resource configuration,
/// and agent setup into a single installable unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageManifest {
    /// Unique package identifier (kebab-case, e.g., "google-cloud")
    pub name: String,
    /// Human-readable name (e.g., "Google Cloud Platform")
    pub display_name: String,
    /// Package author or organization
    pub vendor: String,
    /// Short description shown in marketplace cards
    pub description: String,
    /// Semver version string
    pub version: String,

    // ── Display ──────────────────────────────────────────────────
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub status: String,
    /// Long-form markdown description for the detail page
    #[serde(default)]
    pub long_description: Option<String>,
    /// Category tags for marketplace filtering (e.g., ["cloud", "data"])
    #[serde(default)]
    pub categories: Vec<String>,
    /// Link to documentation / source repo
    #[serde(default)]
    pub docs_url: Option<String>,
    /// Link to source repository
    #[serde(default)]
    pub repo_url: Option<String>,

    // ── Tools ────────────────────────────────────────────────────
    /// List of tool directory names included in this package
    #[serde(default)]
    pub tools: Vec<String>,

    // ── Authentication ───────────────────────────────────────────
    /// How this package authenticates (cli, oauth, api_key, none)
    #[serde(default)]
    pub auth: Option<PackageAuth>,

    // ── Setup ────────────────────────────────────────────────────
    /// Step-by-step setup wizard for initial installation
    #[serde(default)]
    pub setup_steps: Vec<SetupStep>,

    // ── Configurable Resources ───────────────────────────────────
    /// Resource types the user can configure (datasets, buckets, etc.)
    /// These define what the user can allow/restrict for the agent.
    #[serde(default)]
    pub configurable_resources: Vec<ConfigurableResource>,

    // ── Feature Flags ────────────────────────────────────────────
    /// Optional features the user can enable/disable
    #[serde(default)]
    pub feature_flags: Vec<FeatureFlag>,

    // ── Agent Configuration ──────────────────────────────────────
    /// Agent persona, prompts, and execution settings.
    /// Auto-creates a package agent on install.
    #[serde(default = "default_agent_config")]
    pub agent_config: AgentConfig,

    // ── Sub-Agent Template ──────────────────────────────────────
    /// Template for auto-generating sub-agents from configured resources.
    /// Defines form fields, scoped tools, and persona template.
    /// When present, the Package Editor UI shows a "Add Dataset Agent" form.
    #[serde(default)]
    pub sub_agent_template: Option<serde_json::Value>,

    // ── Persistent Connections ───────────────────────────────────
    /// Background processes that receive external events (WebSockets, listeners, etc.)
    /// Each connection is a long-running script managed by the platform.
    /// Events from connections are routed to configured agents.
    #[serde(default)]
    pub connections: Vec<PackageConnection>,
}

/// How a package authenticates with external services
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageAuth {
    /// Auth method: "cli" (gcloud/az), "oauth", "api_key", "service_account", "none"
    #[serde(rename = "type")]
    pub auth_type: String,
    /// Human-readable instructions for authentication
    #[serde(default)]
    pub instructions: Option<String>,
    /// For OAuth: the provider name (e.g., "google")
    #[serde(default)]
    pub oauth_provider: Option<String>,
    /// For OAuth: required scopes
    #[serde(default)]
    pub oauth_scopes: Vec<String>,
    /// For API key: the keyring key name
    #[serde(default)]
    pub credentials_key: Option<String>,
}

/// A configurable resource type that users can scope for the agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigurableResource {
    /// Resource type identifier (e.g., "datasets", "buckets", "repos")
    pub id: String,
    /// Human-readable label (e.g., "BigQuery Datasets")
    pub label: String,
    /// Description shown in config UI
    #[serde(default)]
    pub description: Option<String>,
    /// How to discover existing resources (shell command that returns JSON array)
    #[serde(default)]
    pub discover_command: Option<String>,
    /// Placeholder text for manual entry
    #[serde(default)]
    pub placeholder: Option<String>,
    /// Whether multiple resources can be allowed (default true)
    #[serde(default = "default_true")]
    pub multi: bool,
    /// Which tool actions require this resource
    #[serde(default)]
    pub required_by_actions: Vec<String>,
}

/// An optional feature the user can toggle on/off
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureFlag {
    /// Feature identifier (e.g., "allow_create_dataset")
    pub id: String,
    /// Human-readable label
    pub label: String,
    /// Description shown in config UI
    #[serde(default)]
    pub description: Option<String>,
    /// Default state (on or off)
    #[serde(default)]
    pub default_enabled: bool,
    /// Which tool actions this gates
    #[serde(default)]
    pub gates_actions: Vec<String>,
    /// Warning text shown when enabling (e.g., "This allows creating billable resources")
    #[serde(default)]
    pub enable_warning: Option<String>,
}

/// Agent configuration — defines the agent auto-created when this package is installed.
/// Each package = one agent. The agent gets the package's tools, skills, and persona.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Agent persona / system instructions (who the agent IS)
    #[serde(default)]
    pub default_instructions: Option<String>,
    /// Suggested first prompts for new users ("Try asking...")
    #[serde(default)]
    pub suggested_prompts: Vec<String>,
    /// Recommended provider/model for best results
    #[serde(default)]
    pub recommended_model: Option<String>,
    /// Capabilities summary shown on detail page
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Max tool call iterations (default 10)
    #[serde(default)]
    pub max_iterations: Option<u32>,
    /// Approval mode: "prompt" (default, ask user) or "auto" (trust agent)
    #[serde(default)]
    pub approval_mode: Option<String>,
    /// Temperature for LLM (0.0 - 2.0)
    #[serde(default)]
    pub temperature: Option<f64>,
}

fn default_agent_config() -> AgentConfig {
    AgentConfig {
        default_instructions: None,
        suggested_prompts: vec![],
        recommended_model: None,
        capabilities: vec![],
        max_iterations: None,
        approval_mode: None,
        temperature: None,
    }
}

/// A single step in a marketplace package setup wizard
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupStep {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub check_command: Option<String>,
    #[serde(default)]
    pub install_command: Option<String>,
    #[serde(default)]
    pub install_command_windows: Option<String>,
    #[serde(default)]
    pub install_command_mac: Option<String>,
    #[serde(default)]
    pub install_command_linux: Option<String>,
    #[serde(default)]
    pub install_command_template: Option<String>,
    #[serde(default)]
    pub help_text: Option<String>,
    #[serde(default)]
    pub help_url: Option<String>,
    #[serde(default)]
    pub prompt_user: bool,
    #[serde(default)]
    pub prompt_label: Option<String>,
    #[serde(default)]
    pub prompt_placeholder: Option<String>,
    #[serde(default)]
    pub prompt_help: Option<String>,
    /// If set, the user-entered value is stored in the OS keyring under this key
    /// instead of running a shell command. Used for OAuth credentials, API tokens, etc.
    #[serde(default)]
    pub credentials_key: Option<String>,
    #[serde(default = "default_true")]
    pub required: bool,
}

/// A persistent background connection declared by a marketplace package.
/// The platform manages the lifecycle (start/stop/restart/health) of the
/// connection script, and routes incoming events to configured agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageConnection {
    /// Unique connection identifier within this package (e.g., "socket_mode")
    pub id: String,
    /// Human-readable label (e.g., "Real-time Events (Socket Mode)")
    pub label: String,
    /// Description shown in the marketplace UI
    #[serde(default)]
    pub description: Option<String>,
    /// Script runtime (python, node, shell, etc.)
    pub runtime: RuntimeType,
    /// Script path relative to package directory (e.g., "socket-mode/connect.py")
    pub script: String,
    /// Feature flag ID that must be enabled for this connection to run
    #[serde(default)]
    pub requires_feature: Option<String>,
    /// Keyring credential keys that must exist before starting
    #[serde(default)]
    pub requires_credentials: Vec<String>,
    /// How often to expect heartbeats (seconds). Connection is unhealthy if missed.
    #[serde(default = "default_health_interval")]
    pub health_interval_secs: u32,
    /// Whether to auto-restart on failure
    #[serde(default = "default_true")]
    pub restart_on_failure: bool,
    /// Maximum restart attempts before giving up
    #[serde(default = "default_max_restarts")]
    pub max_restarts: u32,
    /// Delay between restart attempts (seconds)
    #[serde(default = "default_restart_delay")]
    pub restart_delay_secs: u32,
    /// Event types this connection can emit — each can be routed to an agent
    #[serde(default)]
    pub events: Vec<ConnectionEvent>,
}

/// An event type that a connection can emit.
/// Users configure which agent handles each event via the Marketplace UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionEvent {
    /// Event identifier (e.g., "mention", "dm", "slash_command")
    pub id: String,
    /// Human-readable label (e.g., "@Mention Workflow")
    pub label: String,
    /// Description shown in config UI
    #[serde(default)]
    pub description: Option<String>,
    /// Whether the user can assign an agent to this event
    #[serde(default = "default_true")]
    pub agent_configurable: bool,
}

fn default_health_interval() -> u32 { 30 }
fn default_max_restarts() -> u32 { 5 }
fn default_restart_delay() -> u32 { 10 }

fn default_true() -> bool { true }

impl ToolManifest {
    /// Convert parameters to JSON Schema format for OpenAI function calling
    pub fn to_json_schema(&self) -> serde_json::Value {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();

        for (name, param) in &self.parameters {
            let mut prop = serde_json::Map::new();
            prop.insert("type".to_string(), serde_json::Value::String(param.param_type.clone()));
            if !param.description.is_empty() {
                prop.insert("description".to_string(), serde_json::Value::String(param.description.clone()));
            }
            if let Some(ref default) = param.default {
                prop.insert("default".to_string(), default.clone());
            }
            properties.insert(name.clone(), serde_json::Value::Object(prop));

            if param.required {
                required.push(serde_json::Value::String(name.clone()));
            }
        }

        serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
        })
    }

    /// Validate the manifest
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() {
            return Err("Tool name cannot be empty".to_string());
        }
        // Only alphanumeric, underscore, hyphen
        if !self.name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
            return Err("Tool name must be alphanumeric with underscores/hyphens only".to_string());
        }
        if self.entry_point.contains("..") || self.entry_point.contains('/') || self.entry_point.contains('\\') {
            return Err("Entry point must be a simple filename (no path traversal)".to_string());
        }
        Ok(())
    }
}
