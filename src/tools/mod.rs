//! Tool system — Native tools + registry
//!
//! Tools are executable functions the agent can call.
//! Each tool carries its own **Agent Instructions** that are auto-injected
//! into the system prompt at context assembly time (DataVisions pattern).
//!
//! Native tools: file_reader, file_writer, terminal, code_search, save_memory

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCategory {
    /// Built-in native tools (file, terminal, code)
    Native,
    /// User-created or AI-generated custom tools
    Custom,
    /// Integration-provided tools
    Integration,
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
            serde_json::to_string_pretty(&self.output).unwrap_or_default()
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
    pub fn new() -> Self {
        let mut registry = Self {
            tools: HashMap::new(),
            order: Vec::new(),
        };

        registry.register(Box::new(FileReaderTool));
        registry.register(Box::new(FileWriterTool));
        registry.register(Box::new(TerminalTool));
        registry.register(Box::new(CodeSearchTool));
        registry.register(Box::new(SaveMemoryTool));

        registry
    }

    fn register(&mut self, tool: Box<dyn NativeTool>) {
        let name = tool.definition().name.clone();
        self.order.push(name.clone());
        self.tools.insert(name, tool);
    }

    /// List all tool definitions
    pub fn list_definitions(&self) -> Vec<ToolDefinition> {
        self.order
            .iter()
            .filter_map(|name| self.tools.get(name).map(|t| t.definition()))
            .collect()
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

                let max_chars = 100_000;
                if numbered.len() > max_chars {
                    let truncated = &numbered[..max_chars];
                    ToolResult::ok(format!(
                        "{}\n\n... [truncated, file is {} bytes total]",
                        truncated,
                        content.len()
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
                 - Use for builds, tests, git operations, package managers, system info, etc.\n\
                 - Commands run in the project working directory by default.\n\
                 - **Prefer short-lived commands.** Long-running processes (servers, watchers) will timeout after 30 seconds.\n\
                 - Show the user relevant output. Summarize long output.\n\
                 - Be careful with destructive commands (rm, format, etc.) — confirm with the user first.\n\
                 - On Windows, use `cmd /c` or PowerShell syntax as appropriate."
                    .to_string(),
            ),
            category: ToolCategory::Native,
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

        // Use cmd on Windows, sh on Unix
        let (shell, flag) = if cfg!(target_os = "windows") {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tokio::process::Command::new(shell)
                .arg(flag)
                .arg(command)
                .current_dir(&working_dir)
                .output(),
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

                // Truncate very long output
                let max_chars = 50_000;
                if result_text.len() > max_chars {
                    result_text = format!(
                        "{}\n\n... [output truncated, {} bytes total]",
                        &result_text[..max_chars],
                        result_text.len()
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
