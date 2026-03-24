//! Tool system — Native tools, custom tools, connection tools, and runtime
//!
//! Tools are executable functions the agent can call.
//! Each tool carries its own **Agent Instructions** that are auto-injected
//! into the system prompt at context assembly time (DataVisions pattern).
//!
//! Three types of tools:
//! - **Native**: Compiled into the binary (file_reader, file_writer, terminal, etc.)
//! - **Custom**: Script-based tools created by the agent or user (~/.chitty-workspace/tools/custom/)
//! - **Connection**: API integrations with optional sidecars (~/.chitty-workspace/tools/connections/)
//!
//! The `ToolRuntime` provides unified dispatch across all three types.

pub mod manifest;
pub mod executor;
pub mod google;
pub mod media;
pub mod web;
pub mod diagnostic;
pub mod runtime;
pub mod marketplace_client;
#[cfg(feature = "cdp-browser")]
pub mod browser_engine;

pub use runtime::ToolRuntime;
pub use marketplace_client::MarketplaceClient;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::server::BrowserBridge;
use crate::storage::Database;

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// Tool definition (JSON Schema compatible for LLM function calling)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Unique tool name
    pub name: String,
    /// Human-readable display name
    pub display_name: String,
    /// Short description (sent to the LLM in function calling schema)
    pub description: String,
    /// JSON Schema for parameters
    pub parameters: serde_json::Value,
    /// Agent Instructions — injected into the system prompt automatically.
    /// Tells the LLM *when* and *how* to use this tool effectively.
    pub instructions: Option<String>,
    /// Tool category
    pub category: ToolCategory,
    /// Package vendor name (for marketplace tools)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCategory {
    /// Built-in native tools (file, terminal, code)
    Native,
    /// User-created or AI-generated custom tools
    Custom,
    /// Integration-provided tools (Google OAuth, etc.)
    Integration,
    /// Marketplace package tools
    Marketplace,
}

/// Result of executing a tool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub success: bool,
    pub output: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ToolResult {
    pub fn ok(output: impl Into<serde_json::Value>) -> Self {
        Self {
            success: true,
            output: output.into(),
            error: None,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        Self {
            success: false,
            output: serde_json::Value::Null,
            error: Some(msg),
        }
    }

    /// Get the output as a string for sending back to the LLM
    pub fn as_content_string(&self) -> String {
        if let Some(ref err) = self.error {
            format!("Error: {}", err)
        } else if let Some(s) = self.output.as_str() {
            s.to_string()
        } else {
            // Use compact JSON (not pretty) to avoid control character issues
            // when content contains large base64 strings (media tools)
            serde_json::to_string(&self.output).unwrap_or_default()
        }
    }
}

// ---------------------------------------------------------------------------
// NativeTool trait
// ---------------------------------------------------------------------------

/// Context passed to tool execution
pub struct ToolContext {
    pub working_dir: PathBuf,
    pub db: Database,
    pub conversation_id: String,
}

/// Trait for built-in native tools
#[async_trait]
pub trait NativeTool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    async fn execute(&self, args: &serde_json::Value, ctx: &ToolContext) -> ToolResult;
}

// ---------------------------------------------------------------------------
// Tool Registry
// ---------------------------------------------------------------------------

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn NativeTool>>,
    /// Ordered list of tool names (for consistent output)
    order: Vec<String>,
}

impl ToolRegistry {
    /// Create a registry with all native tools registered
    pub fn new(browser_bridge: Arc<BrowserBridge>, skill_registry: Arc<crate::skills::SkillRegistry>) -> Self {
        let mut registry = Self {
            tools: HashMap::new(),
            order: Vec::new(),
        };

        registry.register(Box::new(FileReaderTool));
        registry.register(Box::new(FileWriterTool));
        registry.register(Box::new(TerminalTool));
        registry.register(Box::new(CodeSearchTool));
        registry.register(Box::new(SaveMemoryTool));
        registry.register(Box::new(CreateToolTool));
        registry.register(Box::new(InstallPackageTool));
        registry.register(Box::new(BrowserTool { bridge: browser_bridge }));
        registry.register(Box::new(LoadSkillTool { skill_registry }));

        // Web tools (search + scraper — critical system tools)
        registry.register(Box::new(web::WebSearchTool));
        registry.register(Box::new(web::WebScraperTool));

        // Self-diagnostic tool
        registry.register(Box::new(diagnostic::DiagnosticTool));

        // Media generation tools (image, video, audio, editing)
        registry.register(Box::new(media::GenerateImageTool));
        registry.register(Box::new(media::EditImageTool));
        registry.register(Box::new(media::GenerateVideoTool));
        registry.register(Box::new(media::TextToSpeechTool));

        // Google API tools — Gmail/Calendar now provided by marketplace packages
        // Only Drive search remains as native (no marketplace package yet)
        registry.register(Box::new(google::DriveSearchTool));

        registry
    }

    fn register(&mut self, tool: Box<dyn NativeTool>) {
        let name = tool.definition().name.clone();
        self.order.push(name.clone());
        self.tools.insert(name, tool);
    }

    /// List all tool definitions (native tools + virtual tools like dispatch_agents)
    pub fn list_definitions(&self) -> Vec<ToolDefinition> {
        let mut defs: Vec<ToolDefinition> = self.order
            .iter()
            .filter_map(|name| self.tools.get(name).map(|t| t.definition()))
            .collect();

        // Add virtual tools (handled specially in server.rs, not via NativeTool trait)
        defs.push(ToolDefinition {
            name: "dispatch_agents".to_string(),
            display_name: "Dispatch Agents".to_string(),
            description: "Dispatch tasks to one or more installed package agents. Each agent runs independently with its own tools and persona. Use this when a request needs capabilities from installed packages (Slack, Gmail, Calendar, etc.). For multi-package tasks, dispatch in parallel.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "agent": { "type": "string", "description": "Package agent name or ID (e.g., 'Slack', 'Google Gmail', 'pkg-slack')" },
                                "instruction": { "type": "string", "description": "What to ask this agent to do" }
                            },
                            "required": ["agent", "instruction"]
                        },
                        "minItems": 1,
                        "maxItems": 5,
                        "description": "Tasks to dispatch. Each task runs as a separate agent conversation."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["parallel", "sequential"],
                        "description": "parallel: run all tasks concurrently (default). sequential: run in order, each seeing prior results.",
                        "default": "parallel"
                    }
                },
                "required": ["tasks"]
            }),
            instructions: Some("Use dispatch_agents to delegate tasks to installed package agents. Examples:\n- 'send a Slack message' → dispatch to Slack agent\n- 'prepare standup' → dispatch parallel to Slack + Calendar + Gmail\n- 'read my email and check calendar' → dispatch parallel to Gmail + Calendar\nAlways tell the user which agents you're dispatching to.".to_string()),
            category: ToolCategory::Native,
            vendor: None,
        });

        defs.push(ToolDefinition {
            name: "execute_package_tool".to_string(),
            display_name: "Execute Package Tool".to_string(),
            description: "Execute a specific tool from an installed package DIRECTLY — no LLM call, fast and deterministic. Use this when you know EXACTLY which tool to call and have ALL the arguments. Much faster than dispatch_agents for single tool calls.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "package": {
                        "type": "string",
                        "description": "Package name (e.g., 'slack', 'google-calendar', 'google-gmail', 'google-cloud')"
                    },
                    "tool": {
                        "type": "string",
                        "description": "Tool name within the package (e.g., 'send_message', 'calendar_list', 'gmail_read')"
                    },
                    "arguments": {
                        "type": "object",
                        "description": "Arguments to pass directly to the tool"
                    }
                },
                "required": ["package", "tool", "arguments"]
            }),
            instructions: Some("Use execute_package_tool (Tier 1) when you know the EXACT tool and arguments. Use dispatch_agents (Tier 2) only when the task needs reasoning or multiple tool calls.\n\nTier 1 examples:\n- Send Slack message: execute_package_tool(package='slack', tool='send_message', arguments={channel:'#general', message:'Hello'})\n- List calendar: execute_package_tool(package='google-calendar', tool='calendar_list', arguments={max_results:10})\n- Read Gmail: execute_package_tool(package='google-gmail', tool='gmail_read', arguments={action:'search', query:'is:unread'})\n\nTier 2 examples (use dispatch_agents instead):\n- 'Research recent Slack discussions and summarize' (needs reasoning)\n- 'Find a meeting time that works for everyone' (needs multiple tools)".to_string()),
            category: ToolCategory::Native,
            vendor: None,
        });

        defs.push(ToolDefinition {
            name: "ask_user_questions".to_string(),
            display_name: "Ask User Questions".to_string(),
            description: "Present questions to the user as interactive cards with clickable options. Batch ALL questions into one call. Each question has 2-4 options with the first being recommended. User answers sequentially, all answers returned together.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": { "type": "string" },
                                "options": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": { "type": "string" },
                                            "description": { "type": "string" }
                                        },
                                        "required": ["label", "description"]
                                    },
                                    "minItems": 2, "maxItems": 4
                                }
                            },
                            "required": ["question", "options"]
                        },
                        "minItems": 1, "maxItems": 6
                    }
                },
                "required": ["questions"]
            }),
            instructions: None,
            category: ToolCategory::Native,
            vendor: None,
        });

        defs
    }

    /// Get definitions for specific tool names only
    pub fn get_definitions(&self, names: &[String]) -> Vec<ToolDefinition> {
        names
            .iter()
            .filter_map(|name| self.tools.get(name).map(|t| t.definition()))
            .collect()
    }

    /// Convert tool definitions to OpenAI function calling format.
    /// If `names` is None, returns all tools.
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

    /// Build the agent instructions section for the system prompt.
    /// Collects `instructions` from each selected tool (or all if names is None).
    /// Mirrors DataVisions `_build_tool_instructions_section()`.
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

    /// Check if a native tool exists by name
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Execute a tool by name
    pub async fn execute(
        &self,
        name: &str,
        args: &serde_json::Value,
        ctx: &ToolContext,
    ) -> (ToolResult, u64) {
        let start = Instant::now();

        let result = match self.tools.get(name) {
            Some(tool) => tool.execute(args, ctx).await,
            None => ToolResult::err(format!("Unknown tool: {}", name)),
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        (result, duration_ms)
    }
}

// ---------------------------------------------------------------------------
// Path validation helper
// ---------------------------------------------------------------------------

/// Validate that a path doesn't escape the working directory
fn validate_path(working_dir: &Path, requested: &str) -> std::result::Result<PathBuf, String> {
    let path = if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        working_dir.join(requested)
    };

    // Canonicalize what we can (working_dir must exist)
    let canonical_wd = working_dir
        .canonicalize()
        .map_err(|e| format!("Cannot resolve working directory: {}", e))?;

    // For the requested path, resolve parent if the file doesn't exist yet
    let canonical_path = if path.exists() {
        path.canonicalize()
            .map_err(|e| format!("Cannot resolve path: {}", e))?
    } else {
        // File doesn't exist yet (e.g., file_writer creating new file)
        // Validate the parent directory exists and is within working_dir
        let parent = path
            .parent()
            .ok_or_else(|| "Invalid path: no parent directory".to_string())?;
        if !parent.exists() {
            return Err(format!("Parent directory does not exist: {}", parent.display()));
        }
        let canonical_parent = parent
            .canonicalize()
            .map_err(|e| format!("Cannot resolve parent: {}", e))?;
        if !canonical_parent.starts_with(&canonical_wd) {
            return Err("Path escapes the working directory".to_string());
        }
        // Return the intended path (parent is valid, file will be created)
        canonical_parent.join(path.file_name().unwrap_or_default())
    };

    if !canonical_path.starts_with(&canonical_wd) {
        return Err("Path escapes the working directory".to_string());
    }

    Ok(canonical_path)
}

// ===========================================================================
// Native Tool Implementations
// ===========================================================================

// ---------------------------------------------------------------------------
// file_reader
// ---------------------------------------------------------------------------

struct FileReaderTool;

#[async_trait]
impl NativeTool for FileReaderTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "file_reader".to_string(),
            display_name: "File Reader".to_string(),
            description: "Read the contents of a file. Returns the file content as a string with line numbers.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path (relative to project directory or absolute)"
                    }
                },
                "required": ["path"]
            }),
            instructions: Some(
                "Read files from the project directory. Returns content with line numbers.\n\
                 - **Always read a file before modifying it** to understand its current state.\n\
                 - Supports any text file (source code, configs, markdown, etc.).\n\
                 - For large files, the output may be truncated. Mention this to the user if it occurs.\n\
                 - Use relative paths when possible (relative to the project root)."
                    .to_string(),
            ),
            category: ToolCategory::Native,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolResult::err("Missing required parameter: path"),
        };

        let full_path = match validate_path(&ctx.working_dir, path_str) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };

        match tokio::fs::read_to_string(&full_path).await {
            Ok(content) => {
                // Add line numbers
                let numbered: String = content
                    .lines()
                    .enumerate()
                    .map(|(i, line)| format!("{:>4}│ {}", i + 1, line))
                    .collect::<Vec<_>>()
                    .join("\n");

                let max_chars = 8_000;
                if numbered.len() > max_chars {
                    // Show first portion + last portion for context
                    let head = &numbered[..max_chars * 3 / 4];
                    let tail_start = numbered.len().saturating_sub(max_chars / 4);
                    let tail = &numbered[tail_start..];
                    ToolResult::ok(format!(
                        "{}\n\n... [truncated: showing first {} + last {} of {} total chars, {} lines]\n\n{}",
                        head,
                        head.len(),
                        tail.len(),
                        numbered.len(),
                        content.lines().count(),
                        tail
                    ))
                } else {
                    ToolResult::ok(numbered)
                }
            }
            Err(e) => ToolResult::err(format!("Failed to read {}: {}", path_str, e)),
        }
    }
}

// ---------------------------------------------------------------------------
// file_writer
// ---------------------------------------------------------------------------

struct FileWriterTool;

#[async_trait]
impl NativeTool for FileWriterTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "file_writer".to_string(),
            display_name: "File Writer".to_string(),
            description: "Write content to a file. Creates the file if it doesn't exist, overwrites if it does.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path (relative to project directory or absolute)"
                    },
                    "content": {
                        "type": "string",
                        "description": "The content to write to the file"
                    }
                },
                "required": ["path", "content"]
            }),
            instructions: Some(
                "Write or create files in the project directory.\n\
                 - **Always read a file first** before overwriting to avoid data loss.\n\
                 - Creates parent directories automatically if they don't exist.\n\
                 - Tell the user what file you're creating/modifying and summarize the changes.\n\
                 - For code files, ensure the content is syntactically correct."
                    .to_string(),
            ),
            category: ToolCategory::Native,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolResult::err("Missing required parameter: path"),
        };

        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return ToolResult::err("Missing required parameter: content"),
        };

        let full_path = match validate_path(&ctx.working_dir, path_str) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(e),
        };

        // Create parent directories if needed
        if let Some(parent) = full_path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolResult::err(format!("Failed to create directories: {}", e));
            }
        }

        match tokio::fs::write(&full_path, content).await {
            Ok(()) => ToolResult::ok(format!(
                "Successfully wrote {} bytes to {}",
                content.len(),
                path_str
            )),
            Err(e) => ToolResult::err(format!("Failed to write {}: {}", path_str, e)),
        }
    }
}

// ---------------------------------------------------------------------------
// terminal
// ---------------------------------------------------------------------------

struct TerminalTool;

#[async_trait]
impl NativeTool for TerminalTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "terminal".to_string(),
            display_name: "Terminal".to_string(),
            description: "Run a shell command and return stdout and stderr.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    },
                    "working_dir": {
                        "type": "string",
                        "description": "Working directory for the command (optional, defaults to project root)"
                    }
                },
                "required": ["command"]
            }),
            instructions: Some(
                "Run shell commands on the user's machine.\n\
                 - Commands execute in: PowerShell (Windows), zsh (macOS), sh (Linux).\n\
                 - Use for builds, tests, git operations, package managers, system info, HTTP requests, etc.\n\
                 - Commands run in the project working directory by default.\n\
                 - For HTTP requests: use `Invoke-RestMethod` on Windows (PowerShell), `curl` on Linux/Mac.\n\
                 - **Prefer short-lived commands.** Long-running processes will timeout after 30 seconds.\n\
                 - Show the user relevant output. Summarize long output.\n\
                 - Be careful with destructive commands — confirm with the user first."
                    .to_string(),
            ),
            category: ToolCategory::Native,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return ToolResult::err("Missing required parameter: command"),
        };

        let working_dir = args
            .get("working_dir")
            .and_then(|v| v.as_str())
            .map(|p| {
                if Path::new(p).is_absolute() {
                    PathBuf::from(p)
                } else {
                    ctx.working_dir.join(p)
                }
            })
            .unwrap_or_else(|| ctx.working_dir.clone());

        // Use PowerShell on Windows (has curl alias, better tool support),
        // sh on Linux, zsh on macOS (default shell since Catalina)
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            {
                let mut cmd = if cfg!(target_os = "windows") {
                    let mut c = tokio::process::Command::new("powershell");
                    c.args(&["-NoProfile", "-NonInteractive", "-Command", command]);
                    c
                } else if cfg!(target_os = "macos") {
                    let mut c = tokio::process::Command::new("zsh");
                    c.args(&["-c", command]);
                    c
                } else {
                    let mut c = tokio::process::Command::new("sh");
                    c.args(&["-c", command]);
                    c
                };
                cmd.current_dir(&working_dir);
                // Extend PATH with common tool locations (gcloud, etc.)
                let mut path_env = std::env::var("PATH").unwrap_or_default();
                #[cfg(target_os = "windows")]
                {
                    let extra_paths = [
                        r"C:\Program Files (x86)\Google\Cloud SDK\google-cloud-sdk\bin",
                        r"C:\Program Files\Google\Cloud SDK\google-cloud-sdk\bin",
                    ];
                    for p in &extra_paths {
                        if std::path::Path::new(p).exists() && !path_env.contains(p) {
                            path_env = format!("{};{}", p, path_env);
                        }
                    }
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
                }
                cmd.env("PATH", &path_env);
                cmd.output()
            },
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                let mut result_text = String::new();
                if !stdout.is_empty() {
                    result_text.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !result_text.is_empty() {
                        result_text.push_str("\n--- stderr ---\n");
                    }
                    result_text.push_str(&stderr);
                }

                // Truncate long output to preserve context budget
                let max_chars = 8_000;
                if result_text.len() > max_chars {
                    let head_len = max_chars * 3 / 4;
                    let tail_len = max_chars / 4;
                    let tail_start = result_text.len().saturating_sub(tail_len);
                    result_text = format!(
                        "{}\n\n... [output truncated: showing first {} + last {} of {} total chars]\n\n{}",
                        &result_text[..head_len],
                        head_len,
                        result_text.len() - tail_start,
                        result_text.len(),
                        &result_text[tail_start..]
                    );
                }

                if output.status.success() {
                    if result_text.is_empty() {
                        ToolResult::ok("Command completed successfully (no output)")
                    } else {
                        ToolResult::ok(result_text)
                    }
                } else {
                    let code = output.status.code().unwrap_or(-1);
                    ToolResult {
                        success: false,
                        output: serde_json::Value::String(result_text),
                        error: Some(format!("Command exited with code {}", code)),
                    }
                }
            }
            Ok(Err(e)) => ToolResult::err(format!("Failed to run command: {}", e)),
            Err(_) => ToolResult::err("Command timed out after 30 seconds"),
        }
    }
}

// ---------------------------------------------------------------------------
// code_search
// ---------------------------------------------------------------------------

struct CodeSearchTool;

#[async_trait]
impl NativeTool for CodeSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "code_search".to_string(),
            display_name: "Code Search".to_string(),
            description: "Search for a pattern in code files. Returns matching lines with file paths and line numbers.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Regex pattern to search for"
                    },
                    "glob": {
                        "type": "string",
                        "description": "File glob pattern to filter (e.g., '*.rs', '*.ts'). Optional — searches all text files by default."
                    }
                },
                "required": ["query"]
            }),
            instructions: Some(
                "Search code files by regex pattern. Returns matching lines with file:line references.\n\
                 - **Use before editing** to find where things are defined or used.\n\
                 - Supports full regex syntax (e.g., `fn\\s+\\w+`, `TODO|FIXME`).\n\
                 - Use the `glob` parameter to narrow to specific file types (e.g., `*.rs`, `*.py`).\n\
                 - Results are limited to 100 matches. Refine your query if you get too many."
                    .to_string(),
            ),
            category: ToolCategory::Native,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let query = match args.get("query").and_then(|v| v.as_str()) {
            Some(q) => q,
            None => return ToolResult::err("Missing required parameter: query"),
        };

        let glob_pattern = args.get("glob").and_then(|v| v.as_str());

        // Compile regex
        let re = match regex::Regex::new(query) {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("Invalid regex pattern: {}", e)),
        };

        // Run search on blocking thread (walkdir is sync)
        let working_dir = ctx.working_dir.clone();
        let glob_owned = glob_pattern.map(|s| s.to_string());

        let result = tokio::task::spawn_blocking(move || {
            search_files(&working_dir, &re, glob_owned.as_deref())
        })
        .await;

        match result {
            Ok(matches) => {
                if matches.is_empty() {
                    ToolResult::ok("No matches found")
                } else {
                    let count = matches.len();
                    let output = matches.join("\n");
                    ToolResult::ok(format!("{} matches found:\n\n{}", count, output))
                }
            }
            Err(e) => ToolResult::err(format!("Search failed: {}", e)),
        }
    }
}

/// Perform the actual file search (runs on blocking thread)
fn search_files(dir: &Path, re: &regex::Regex, glob: Option<&str>) -> Vec<String> {
    let mut matches = Vec::new();
    let max_matches = 100;

    // Skip common non-code directories
    let skip_dirs = [
        "node_modules",
        ".git",
        "target",
        "dist",
        "build",
        "__pycache__",
        ".next",
        "vendor",
    ];

    for entry in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            if e.file_type().is_dir() {
                !skip_dirs.contains(&name.as_ref())
            } else {
                true
            }
        })
    {
        if matches.len() >= max_matches {
            break;
        }

        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();

        // Apply glob filter if provided
        if let Some(glob_pat) = glob {
            let file_name = path.file_name().unwrap_or_default().to_string_lossy();
            if !simple_glob_match(glob_pat, &file_name) {
                continue;
            }
        }

        // Skip binary files (simple heuristic: check extension)
        if is_likely_binary(path) {
            continue;
        }

        // Read and search
        if let Ok(content) = std::fs::read_to_string(path) {
            for (line_num, line) in content.lines().enumerate() {
                if matches.len() >= max_matches {
                    break;
                }
                if re.is_match(line) {
                    let rel_path = path
                        .strip_prefix(dir)
                        .unwrap_or(path)
                        .to_string_lossy();
                    matches.push(format!("{}:{}: {}", rel_path, line_num + 1, line.trim()));
                }
            }
        }
    }

    matches
}

/// Simple glob matching (supports *.ext patterns)
fn simple_glob_match(pattern: &str, filename: &str) -> bool {
    if pattern.starts_with("*.") {
        let ext = &pattern[1..]; // includes the dot
        filename.ends_with(ext)
    } else if pattern.contains('*') {
        // Very simple wildcard: split on * and check starts/ends
        let parts: Vec<&str> = pattern.split('*').collect();
        if parts.len() == 2 {
            filename.starts_with(parts[0]) && filename.ends_with(parts[1])
        } else {
            filename == pattern
        }
    } else {
        filename == pattern
    }
}

/// Check if a file is likely binary based on extension
fn is_likely_binary(path: &Path) -> bool {
    let binary_exts = [
        "png", "jpg", "jpeg", "gif", "bmp", "ico", "svg", "webp", "mp3", "mp4", "avi", "mov",
        "wav", "ogg", "flac", "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "pdf", "doc",
        "docx", "xls", "xlsx", "ppt", "pptx", "exe", "dll", "so", "dylib", "o", "obj", "lib",
        "a", "class", "pyc", "pyo", "wasm", "ttf", "otf", "woff", "woff2", "eot", "db",
        "sqlite", "sqlite3",
    ];

    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| binary_exts.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// save_memory
// ---------------------------------------------------------------------------

struct SaveMemoryTool;

#[async_trait]
impl NativeTool for SaveMemoryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "save_memory".to_string(),
            display_name: "Save Memory".to_string(),
            description: "Save important information to persistent memory for future conversations.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Short descriptive name for this memory"
                    },
                    "content": {
                        "type": "string",
                        "description": "The information to remember"
                    },
                    "memory_type": {
                        "type": "string",
                        "enum": ["user", "feedback", "project", "reference"],
                        "description": "Type of memory: user (preferences), feedback (corrections), project (project info), reference (external resources). Defaults to 'project'."
                    }
                },
                "required": ["name", "content"]
            }),
            instructions: Some(
                "Save important information to persistent memory for future conversations.\n\
                 - **Save when you learn something important** about the user, project, or their preferences.\n\
                 - Use `user` type for preferences and expertise (e.g., 'prefers Rust, senior developer').\n\
                 - Use `feedback` type for corrections (e.g., 'don't use unwrap, handle errors').\n\
                 - Use `project` type for project decisions and context.\n\
                 - Use `reference` type for pointers to external resources.\n\
                 - Don't save trivial or temporary information."
                    .to_string(),
            ),
            category: ToolCategory::Native,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let name = match args.get("name").and_then(|v| v.as_str()) {
            Some(n) => n.to_string(),
            None => return ToolResult::err("Missing required parameter: name"),
        };

        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => return ToolResult::err("Missing required parameter: content"),
        };

        let memory_type = args
            .get("memory_type")
            .and_then(|v| v.as_str())
            .unwrap_or("project");

        let memory_type_parsed: crate::chat::memory::MemoryType = match memory_type.parse() {
            Ok(t) => t,
            Err(_) => crate::chat::memory::MemoryType::Project,
        };

        let memory = crate::chat::memory::Memory {
            id: uuid::Uuid::new_v4().to_string(),
            memory_type: memory_type_parsed,
            name: name.clone(),
            description: String::new(),
            content: content.clone(),
            scope: crate::chat::memory::MemoryScope::Global,
            scope_ref: None,
            tags: Vec::new(),
            created_at: chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            updated_at: chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        };

        let db = ctx.db.clone();
        match db
            .with_conn(move |conn| crate::chat::memory::MemoryManager::save(conn, &memory))
            .await
        {
            Ok(()) => ToolResult::ok(format!("Memory saved: '{}'", name)),
            Err(e) => ToolResult::err(format!("Failed to save memory: {}", e)),
        }
    }
}

// ---------------------------------------------------------------------------
// create_tool — Agent can create new custom tools on-the-fly
// ---------------------------------------------------------------------------

struct CreateToolTool;

#[async_trait]
impl NativeTool for CreateToolTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "create_tool".to_string(),
            display_name: "Create Tool".to_string(),
            description: "Create a new reusable custom tool. The tool is a script (Python, Node.js, Shell) that receives JSON parameters on stdin and returns JSON on stdout.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Unique tool name (snake_case, e.g., 'pdf_generator', 'chart_builder')"
                    },
                    "display_name": {
                        "type": "string",
                        "description": "Human-readable display name (e.g., 'PDF Generator')"
                    },
                    "description": {
                        "type": "string",
                        "description": "What the tool does (shown to LLMs in function calling)"
                    },
                    "runtime": {
                        "type": "string",
                        "enum": ["python", "node", "powershell", "shell"],
                        "description": "Script runtime to use"
                    },
                    "script": {
                        "type": "string",
                        "description": "The script source code. Must read JSON from stdin and write JSON to stdout with format: {\"success\": true, \"output\": \"result\"}"
                    },
                    "parameters": {
                        "type": "object",
                        "description": "Parameter definitions. Each key is the param name, value is {\"type\": \"string\", \"description\": \"...\", \"required\": true/false}"
                    },
                    "instructions": {
                        "type": "string",
                        "description": "Instructions for when/how to use this tool (injected into system prompt). Optional."
                    }
                },
                "required": ["name", "display_name", "description", "runtime", "script", "parameters"]
            }),
            instructions: Some(
                "Create reusable custom tools that persist across sessions.\n\
                 - **Use when the user needs a capability that doesn't exist** (e.g., PDF generation, chart creation, API integration).\n\
                 - The script MUST read JSON from stdin and write JSON to stdout.\n\
                 - Output format: `{\"success\": true, \"output\": \"result data\"}` or `{\"success\": false, \"error\": \"error message\"}`.\n\
                 - For Python tools, use `import json, sys; args = json.load(sys.stdin)` to read params.\n\
                 - For Node tools, use `process.stdin` to read JSON input.\n\
                 - After creating the tool, it's immediately available for use.\n\
                 - If the tool needs packages, use `install_package` first, then create the tool.\n\
                 - Example Python tool template:\n\
                 ```python\n\
                 import json, sys\n\
                 args = json.load(sys.stdin)\n\
                 # Do work with args...\n\
                 result = {\"success\": True, \"output\": \"done\"}\n\
                 print(json.dumps(result))\n\
                 ```"
                    .to_string(),
            ),
            category: ToolCategory::Native,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let name = match args.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => return ToolResult::err("Missing required parameter: name"),
        };
        let display_name = match args.get("display_name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => return ToolResult::err("Missing required parameter: display_name"),
        };
        let description = match args.get("description").and_then(|v| v.as_str()) {
            Some(d) => d,
            None => return ToolResult::err("Missing required parameter: description"),
        };
        let runtime = match args.get("runtime").and_then(|v| v.as_str()) {
            Some(r) => r,
            None => return ToolResult::err("Missing required parameter: runtime"),
        };
        let script = match args.get("script").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return ToolResult::err("Missing required parameter: script"),
        };

        // Validate name
        if !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
            return ToolResult::err("Tool name must be alphanumeric with underscores/hyphens only");
        }

        // Determine runtime and extension
        let runtime_type = match runtime {
            "python" => manifest::RuntimeType::Python,
            "node" | "javascript" => manifest::RuntimeType::Node,
            "powershell" => manifest::RuntimeType::PowerShell,
            "shell" | "bash" | "sh" => manifest::RuntimeType::Shell,
            _ => return ToolResult::err(format!("Unsupported runtime: {}. Use: python, node, powershell, shell", runtime)),
        };

        let (_, ext) = runtime_type.command_and_ext();
        let entry_point = format!("tool{}", ext);

        // Parse parameters
        let mut param_defs = std::collections::HashMap::new();
        if let Some(params) = args.get("parameters").and_then(|v| v.as_object()) {
            for (key, val) in params {
                let param_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("string").to_string();
                let desc = val.get("description").and_then(|d| d.as_str()).unwrap_or("").to_string();
                let required = val.get("required").and_then(|r| r.as_bool()).unwrap_or(false);
                param_defs.insert(key.clone(), manifest::ParamDef {
                    param_type,
                    description: desc,
                    required,
                    default: val.get("default").cloned(),
                });
            }
        }

        let instructions = args.get("instructions").and_then(|v| v.as_str()).map(String::from);

        // Create the manifest
        let tool_manifest = manifest::ToolManifest {
            name: name.to_string(),
            display_name: display_name.to_string(),
            description: description.to_string(),
            version: "1.0.0".to_string(),
            tool_type: manifest::ToolType::Custom,
            runtime: runtime_type,
            entry_point: entry_point.clone(),
            parameters: param_defs,
            install_commands: Vec::new(),
            timeout_seconds: 30,
            permission_tier: manifest::PermissionTier::Moderate,
            source: manifest::ToolSource::AgentCreated,
            marketplace_id: None,
            instructions,
            connection: None,
            actions: None,
        };

        // Write to disk
        let data_dir = crate::storage::default_data_dir();
        let tool_dir = data_dir.join("tools").join("custom").join(name);

        if let Err(e) = tokio::fs::create_dir_all(&tool_dir).await {
            return ToolResult::err(format!("Failed to create tool directory: {}", e));
        }

        // Write manifest
        let manifest_json = match serde_json::to_string_pretty(&tool_manifest) {
            Ok(j) => j,
            Err(e) => return ToolResult::err(format!("Failed to serialize manifest: {}", e)),
        };
        if let Err(e) = tokio::fs::write(tool_dir.join("manifest.json"), &manifest_json).await {
            return ToolResult::err(format!("Failed to write manifest: {}", e));
        }

        // Write script
        if let Err(e) = tokio::fs::write(tool_dir.join(&entry_point), script).await {
            return ToolResult::err(format!("Failed to write script: {}", e));
        }

        tracing::info!("Created custom tool '{}' at {}", name, tool_dir.display());

        ToolResult::ok(format!(
            "Tool '{}' created successfully at {}. It will be available after the ToolRuntime reloads. \
             The tool reads JSON from stdin and writes JSON to stdout.",
            name,
            tool_dir.display()
        ))
    }
}

// ---------------------------------------------------------------------------
// install_package — Install packages for custom tool dependencies
// ---------------------------------------------------------------------------

struct InstallPackageTool;

#[async_trait]
impl NativeTool for InstallPackageTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "install_package".to_string(),
            display_name: "Install Package".to_string(),
            description: "Install Python or Node.js packages for use by custom tools. Packages are installed in an isolated directory.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "runtime": {
                        "type": "string",
                        "enum": ["python", "node"],
                        "description": "Package manager to use"
                    },
                    "packages": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of package names to install (e.g., ['markdown2', 'weasyprint'])"
                    },
                    "tool_name": {
                        "type": "string",
                        "description": "Name of the custom tool these packages are for (creates isolated install)"
                    }
                },
                "required": ["runtime", "packages", "tool_name"]
            }),
            instructions: Some(
                "Install package dependencies for custom tools.\n\
                 - **Use before create_tool** when the tool needs external packages.\n\
                 - Packages are installed in an isolated directory per tool (not globally).\n\
                 - Python packages go to `~/.chitty-workspace/packages/python/{tool_name}/`.\n\
                 - Node packages go to `~/.chitty-workspace/packages/node/{tool_name}/`.\n\
                 - The custom tool executor automatically adds these paths to the runtime search path."
                    .to_string(),
            ),
            category: ToolCategory::Native,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let runtime = match args.get("runtime").and_then(|v| v.as_str()) {
            Some(r) => r,
            None => return ToolResult::err("Missing required parameter: runtime"),
        };
        let packages: Vec<String> = match args.get("packages").and_then(|v| v.as_array()) {
            Some(arr) => arr.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            None => return ToolResult::err("Missing required parameter: packages"),
        };
        let tool_name = match args.get("tool_name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => return ToolResult::err("Missing required parameter: tool_name"),
        };

        if packages.is_empty() {
            return ToolResult::err("No packages specified");
        }

        // Validate package names (basic safety check)
        for pkg in &packages {
            if pkg.contains("..") || pkg.contains('/') || pkg.contains('\\') || pkg.contains(';') {
                return ToolResult::err(format!("Invalid package name: {}", pkg));
            }
        }

        let data_dir = crate::storage::default_data_dir();
        let packages_dir = data_dir.join("packages");

        let (cmd, cmd_args, target_dir) = match runtime {
            "python" => {
                let target = packages_dir.join("python").join(tool_name);
                if let Err(e) = tokio::fs::create_dir_all(&target).await {
                    return ToolResult::err(format!("Failed to create packages directory: {}", e));
                }
                let python = if cfg!(target_os = "windows") { "python" } else { "python3" };
                let mut install_args = vec![
                    "-m".to_string(), "pip".to_string(), "install".to_string(),
                    "--target".to_string(), target.to_string_lossy().to_string(),
                    "--quiet".to_string(),
                ];
                install_args.extend(packages.clone());
                (python.to_string(), install_args, target)
            }
            "node" => {
                let target = packages_dir.join("node").join(tool_name);
                if let Err(e) = tokio::fs::create_dir_all(&target).await {
                    return ToolResult::err(format!("Failed to create packages directory: {}", e));
                }
                let mut install_args = vec![
                    "install".to_string(),
                    "--prefix".to_string(), target.to_string_lossy().to_string(),
                ];
                install_args.extend(packages.clone());
                ("npm".to_string(), install_args, target)
            }
            _ => return ToolResult::err(format!("Unsupported runtime for package install: {}", runtime)),
        };

        tracing::info!("Installing packages for '{}': {} {:?}", tool_name, cmd, packages);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            tokio::process::Command::new(&cmd)
                .args(&cmd_args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                if output.status.success() {
                    tracing::info!("Packages installed for '{}' at {}", tool_name, target_dir.display());
                    ToolResult::ok(format!(
                        "Installed {} package(s) for '{}': {}\nTarget: {}",
                        packages.len(),
                        tool_name,
                        packages.join(", "),
                        target_dir.display()
                    ))
                } else {
                    let error_text = if stderr.is_empty() { stdout.to_string() } else { stderr.to_string() };
                    ToolResult::err(format!(
                        "Package installation failed:\n{}",
                        &error_text[..error_text.len().min(2000)]
                    ))
                }
            }
            Ok(Err(e)) => ToolResult::err(format!("Failed to run {}: {} (is it installed?)", cmd, e)),
            Err(_) => ToolResult::err("Package installation timed out after 120 seconds"),
        }
    }
}

// ---------------------------------------------------------------------------
// Browser tool — controls the user's browser via the Chitty Browser Extension
// ---------------------------------------------------------------------------

struct BrowserTool {
    bridge: Arc<BrowserBridge>,
}

#[async_trait]
impl NativeTool for BrowserTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "browser".to_string(),
            display_name: "Browser".to_string(),
            description: "Control the user's browser via the Chitty Browser Extension. \
                Navigate to any website, click elements, type text, take screenshots, \
                and read page content. Works on LinkedIn, X.com, Gmail, and any site. \
                The user can see everything happening in their browser.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["open", "screenshot", "click", "type", "read_text", "execute_js", "wait_for", "page_info", "close"],
                        "description": "The browser action to perform"
                    },
                    "url": {
                        "type": "string",
                        "description": "URL to navigate to (required for 'open' action)"
                    },
                    "selector": {
                        "type": "string",
                        "description": "CSS selector for targeting elements (for click/type/read_text/wait_for)"
                    },
                    "text": {
                        "type": "string",
                        "description": "Text to type into the targeted element (for 'type' action)"
                    },
                    "script": {
                        "type": "string",
                        "description": "JavaScript code to execute in the page context (for 'execute_js' action)"
                    },
                    "timeout": {
                        "type": "number",
                        "description": "Timeout in milliseconds for wait_for action (default: 10000)"
                    }
                },
                "required": ["action"]
            }),
            instructions: Some(
                "Control the user's actual Chrome browser via the Chitty Browser Extension.\n\
                 The user sees everything — pages open in real Chrome tabs they can interact with.\n\
                 \n\
                 **IMPORTANT: Always try the action first.** Don't ask about setup or check connection \
                 status. Just call the browser tool. If it fails, THEN tell the user to check the extension.\n\
                 \n\
                 **Actions:**\n\
                 - `open` — Navigate to any URL. Opens a tab in Chrome.\n\
                 - `screenshot` — Capture the visible page as a screenshot (shown in chat).\n\
                 - `click` — Click an element by CSS `selector`.\n\
                 - `type` — Type `text` into a field targeted by `selector`. Works with contenteditable.\n\
                 - `read_text` — Extract text content. Pass `selector` for specific element, or omit for full page.\n\
                 - `execute_js` — Run JavaScript in the page context.\n\
                 - `wait_for` — Wait for element matching `selector` to appear (default 10s timeout).\n\
                 - `page_info` — Get current URL, title, and text snippet.\n\
                 - `close` — Close the current tab.\n\
                 \n\
                 **Full access:** Works on ANY site — Gmail, LinkedIn, X.com, banks, etc.\n\
                 The user's login sessions are available because it's their own browser.\n\
                 If a site requires login, just open it — the user will see the login page in Chrome \
                 and can log in themselves. Then continue with the task.\n\
                 \n\
                 **For Gmail:** Just `open` https://mail.google.com — the user is likely already logged in.\n\
                 Then use `read_text` to read emails, `click` to open messages, etc.\n\
                 \n\
                 **For LinkedIn:** `open` https://www.linkedin.com then navigate normally.\n\
                 \n\
                 **Never tell users to set up OAuth, API credentials, or go to Google Cloud Console.** \
                 The browser tool gives you direct access to any website the user is logged into."
                    .to_string(),
            ),
            category: ToolCategory::Native,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let action = match args.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => return ToolResult::err("Missing required parameter: action"),
        };

        // The extension connects via HTTP polling (/api/browser/poll).
        // Commands are queued and the extension picks them up on its next poll cycle.
        // If the extension isn't running, the command will timeout.

        let cmd = crate::server::BrowserCommand {
            id: uuid::Uuid::new_v4().to_string(),
            action: action.to_string(),
            params: args.clone(),
        };

        let timeout = match action {
            "open" => std::time::Duration::from_secs(20),
            "wait_for" => {
                let ms = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(10000);
                std::time::Duration::from_millis(ms + 2000) // extra buffer
            }
            "execute_js" => std::time::Duration::from_secs(30),
            _ => std::time::Duration::from_secs(10),
        };

        match self.bridge.send_command(cmd, timeout).await {
            Ok(resp) if resp.success => ToolResult::ok(resp.data),
            Ok(resp) => ToolResult::err(resp.error.unwrap_or_else(|| "Browser action failed".into())),
            Err(e) => ToolResult::err(format!("Browser command failed: {}", e)),
        }
    }
}

// ---------------------------------------------------------------------------
// load_skill — Load a skill's full instructions into context
// ---------------------------------------------------------------------------

struct LoadSkillTool {
    skill_registry: Arc<crate::skills::SkillRegistry>,
}

#[async_trait]
impl NativeTool for LoadSkillTool {
    fn definition(&self) -> ToolDefinition {
        // Build enum of valid skill names for the parameter constraint
        let skill_names = self.skill_registry.names();
        let enum_value = if skill_names.is_empty() {
            serde_json::json!([])
        } else {
            serde_json::json!(skill_names)
        };

        ToolDefinition {
            name: "load_skill".to_string(),
            display_name: "Load Skill".to_string(),
            description: "Load a skill's full instructions into context. Call this when a task matches a skill's description to get specialized guidance.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "skill_name": {
                        "type": "string",
                        "description": "Name of the skill to load",
                        "enum": enum_value
                    }
                },
                "required": ["skill_name"]
            }),
            instructions: None, // Instructions come from the skill catalog in the system prompt
            category: ToolCategory::Native,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let skill_name = match args.get("skill_name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => return ToolResult::err("Missing required parameter: skill_name"),
        };

        match self.skill_registry.load_skill_content(skill_name) {
            Some(content) => ToolResult::ok(content),
            None => ToolResult::err(format!(
                "Skill '{}' not found. Available skills: {}",
                skill_name,
                self.skill_registry.names().join(", ")
            )),
        }
    }
}
