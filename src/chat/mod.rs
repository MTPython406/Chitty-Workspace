//! Chat engine
//!
//! Manages conversations, message history, tool call loops,
//! and persistence to local SQLite.
//!
//! Context assembly order for each conversation:
//! 1. Base system prompt (from agent or default)
//! 2. Project context (from chitty.md if in a project directory)
//! 3. Relevant memories (global + project-scoped + agent-scoped)
//! 4. Tool agent instructions (auto-injected from tools)
//! 5. Tool definitions (OpenAI function calling format)
//! 6. Conversation history (messages, trimmed to fit context window)

pub mod context;
pub mod memory;

use anyhow::{Context as _, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::providers::ChatMessage;
use crate::agents::AgentsManager;
use crate::tools::ToolDefinition;

/// Load Chitty orchestrator config from its marketplace package on disk.
/// Returns (persona, allowed_tools, exec_config) or None if files not found.
fn load_chitty_package() -> Option<(String, Vec<String>, ExecutionConfig)> {
    let chitty_dir = crate::storage::default_data_dir()
        .join("tools")
        .join("marketplace")
        .join("chitty");

    // Load SKILL.md → persona (body) + allowed-tools (frontmatter)
    let skill_path = chitty_dir.join("SKILL.md");
    let skill_content = std::fs::read_to_string(&skill_path).ok()?;

    // Parse YAML frontmatter
    let (persona, allowed_tools) = parse_skill_md(&skill_content)?;

    // Load package.json → execution config
    let pkg_path = chitty_dir.join("package.json");
    let exec_config = if let Ok(pkg_content) = std::fs::read_to_string(&pkg_path) {
        parse_chitty_exec_config(&pkg_content)
    } else {
        ExecutionConfig { max_iterations: 25, ..ExecutionConfig::default() }
    };

    tracing::info!(
        "Loaded Chitty package: {} tools, {} char persona",
        allowed_tools.len(),
        persona.len()
    );

    Some((persona, allowed_tools, exec_config))
}

/// Parse a SKILL.md file into (body_text, allowed_tools_list)
fn parse_skill_md(content: &str) -> Option<(String, Vec<String>)> {
    // Split frontmatter from body
    let trimmed = content.trim();
    if !trimmed.starts_with("---") {
        return Some((content.to_string(), vec![]));
    }

    let after_first = &trimmed[3..];
    let end = after_first.find("---")?;
    let frontmatter = &after_first[..end];
    let body = after_first[end + 3..].trim().to_string();

    // Extract allowed-tools from frontmatter
    let mut allowed_tools = Vec::new();
    for line in frontmatter.lines() {
        let line = line.trim();
        if line.starts_with("allowed-tools:") {
            let tools_str = line.strip_prefix("allowed-tools:")?.trim();
            allowed_tools = tools_str.split_whitespace().map(|s| s.to_string()).collect();
        }
    }

    if body.is_empty() {
        return None;
    }

    Some((body, allowed_tools))
}

/// Parse execution config from Chitty's package.json
fn parse_chitty_exec_config(content: &str) -> ExecutionConfig {
    let default = ExecutionConfig { max_iterations: 25, ..ExecutionConfig::default() };

    let json: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => return default,
    };

    let ac = match json.get("agent_config") {
        Some(v) => v,
        None => return default,
    };

    ExecutionConfig {
        max_iterations: ac.get("max_iterations").and_then(|v| v.as_u64()).unwrap_or(25) as u32,
        temperature: ac.get("temperature").and_then(|v| v.as_f64()),
        max_tokens: ac.get("max_tokens").and_then(|v| v.as_u64()).map(|v| v as u32),
        auto_approve: ac.get("approval_mode").and_then(|v| v.as_str()) == Some("auto"),
        context_budget_pct: ac.get("context_budget_pct").and_then(|v| v.as_u64()).unwrap_or(75) as u32,
        compaction_strategy: ac.get("compaction_strategy").and_then(|v| v.as_str()).unwrap_or("truncate").to_string(),
        max_conversation_turns: ac.get("max_conversation_turns").and_then(|v| v.as_u64()).map(|v| v as u32),
        context_length: ac.get("context_length").and_then(|v| v.as_u64()).map(|v| v as u32),
        sub_agent_tools: Vec::new(),
    }
}

/// A chat conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub title: Option<String>,
    pub agent_id: Option<String>,
    pub project_path: Option<String>,
    pub provider: String,
    pub model: String,
    pub created_at: String,
    pub updated_at: String,
}

/// A message in a conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    pub parent_message_id: Option<String>,
    pub role: String,
    pub content: String,
    pub tool_calls: Option<serde_json::Value>,
    pub tool_call_id: Option<String>,
    pub token_count: Option<i32>,
    pub created_at: String,
}

/// Assembled context for an LLM call
#[derive(Debug)]
pub struct AssembledContext {
    /// System prompt (agent instructions + project context + memories + tool instructions)
    pub system_prompt: String,
    /// Conversation messages (trimmed to fit)
    pub messages: Vec<ChatMessage>,
    /// Tool definitions in OpenAI function calling format
    pub tools: Vec<serde_json::Value>,
}

/// Execution configuration (from agent or defaults)
/// Mirrors DataVisions agent node properties (slim version)
#[derive(Debug, Clone)]
pub struct ExecutionConfig {
    pub max_iterations: u32,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    /// If true, skip approval prompts — agent auto-approves all sensitive actions
    pub auto_approve: bool,
    /// Context budget: percentage of model's context window to use before compacting (default 75)
    pub context_budget_pct: u32,
    /// Compaction strategy: "truncate" (fast, mechanical) or "summarize" (uses LLM)
    pub compaction_strategy: String,
    /// Max conversation turns before forcing compaction (None = unlimited)
    pub max_conversation_turns: Option<u32>,
    /// Context window size override (None = use model default)
    pub context_length: Option<u32>,
    /// Sub-agent scoped tools with locked_params (auto-merged into tool call arguments)
    pub sub_agent_tools: Vec<crate::agents::SubAgentTool>,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            max_iterations: 10,
            temperature: None,
            max_tokens: None,
            auto_approve: false,
            context_budget_pct: 60,
            compaction_strategy: "truncate".to_string(),
            max_conversation_turns: None,
            context_length: None,
            sub_agent_tools: Vec::new(),
        }
    }
}

/// Default system prompt when no agent is active — Chitty is the orchestrator
pub const CHITTY_SYSTEM_PROMPT: &str = r#"You are Chitty, the orchestrator for Chitty Workspace — a local-first AI assistant running 100% on the user's machine.

Be direct, concise, and **action-oriented**. Use your tools to DO things — don't explain how the user could do them manually.

## Your Role

You are the **orchestrator**. You have system tools for file operations, terminal commands, browser control, and memory. For everything else, you **dispatch to package agents**.

**IMPORTANT — Be proactive:**
- When the user asks about code, bugs, reviews, or errors → read the files and fix them immediately
- When the user says "my code" or "the project" → use the project directory, don't ask which files
- When something needs fixing → fix it with tools, then report what you did
- Only ask clarifying questions when truly ambiguous (multiple valid interpretations)

**When to handle directly** (your system tools):
- File reading, writing, code search, **code fixing and refactoring**
- Terminal commands (build, test, run)
- Browser control
- Memory (save/recall)
- Skill loading
- Installing new packages
- Creating custom tools

**When to use package tools** (email, calendar, Slack, cloud, etc.):

**PREFER Tier 1 — `execute_package_tool`** for direct, fast tool calls:
- Use when you know the exact tool name and arguments
- Examples: `execute_package_tool(package="google-gmail", tool="gmail_read", arguments={action:"list", max_results:5})`
- `execute_package_tool(package="slack", tool="slack_list_channels", arguments={})`
- `execute_package_tool(package="google-calendar", tool="calendar_list", arguments={max_results:10})`
- No LLM overhead — instant execution

**Use Tier 2 — `dispatch_agents`** only for complex multi-step tasks:
- When the task needs the agent to reason about what tools to call
- When multiple tool calls in sequence are needed with decisions between them
- Dispatch **parallel** when tasks are independent (e.g., "prepare standup" → Slack + Calendar + Gmail simultaneously)
- Example: "Research recent Slack discussions and summarize the key decisions"

## Package Discovery

If the user asks for something no installed package handles, suggest relevant packages from the marketplace. Use `install_package` (with user approval) to add new capabilities. Each installed package auto-creates an agent with its own tools.

## When Package Tools Fail (Auth/Setup Errors)

**CRITICAL:** When `execute_package_tool` fails with authentication, credential, or OAuth errors, DO NOT:
- Tell the user to manually get OAuth tokens
- Fall back to opening websites in the terminal
- Give technical instructions about Google Cloud projects or API credentials

Instead, guide the user through the **in-app setup flow**:
1. Tell the user: "The [package name] package needs to be set up first. Let me walk you through it."
2. Direct them to the **Marketplace tab** in the Action Panel (right side)
3. Have them click the package (e.g., Google Gmail) to see its setup steps
4. The setup includes an OAuth login button — clicking it opens the standard Google/Slack login in their browser
5. Once they authorize, the package is connected and tools will work

**For Google packages** (Gmail, Calendar, Cloud): The setup requires a one-time OAuth login. The Marketplace tab shows a "Connect" button that triggers the OAuth flow automatically. No manual token copying needed.

## Browser Extension

The Chitty Browser Extension lets you control the user's Chrome/Edge browser — open pages, click elements, read content, take screenshots, and run JavaScript.

**When the browser tool fails or the extension is not connected:**
1. Call `GET /api/browser/extension-info` to check status and get the extension path
2. Walk the user through installation step by step:
   - Open Chrome or Edge and navigate to `chrome://extensions`
   - Enable **Developer mode** (toggle in the top-right corner)
   - Click **Load unpacked**
   - Select the `extension` folder from the Chitty Workspace installation directory
   - The Chitty icon will appear in the browser toolbar
   - Click it to verify it shows a green "Connected" badge
3. The extension ships with Chitty Workspace — no separate download needed
4. It connects automatically to Chitty on localhost:8770

**NEVER** use `terminal` to open websites as a fallback for the browser tool. If the extension isn't set up, help the user install it.

## Building Custom Agents

When users want to create a new agent, use `ask_user_questions` to understand their needs, then create the agent via POST to `/api/agents`. An agent = persona + package tools + settings.

## System Knowledge

**Data:** Config at `~/.chitty-workspace/config.toml`, DB at `~/.chitty-workspace/workspace.db`, packages at `~/.chitty-workspace/tools/marketplace/`.

**Providers:** BYOK — OpenAI, Anthropic, Google, xAI. Local: Chitty Model Manager (GGUF, SafeTensors, EXL2). Keys in OS keyring.

**Skills:** Composable capability packages (SKILL.md files). Use `load_skill` to activate.

**Artifacts:** Wrap rich output in `<artifact type="html" title="Name">...</artifact>` tags.

**Memory:** Save important info with `save_memory`. Types: user/feedback/project/reference.

**Project context:** Loads `chitty.md` automatically. Follow its instructions.

**Browser:** Controls user's Chrome/Edge via the Chitty Browser Extension. The extension ships with the app and must be loaded as an unpacked extension in Chrome/Edge. Use `GET /api/browser/extension-info` to check status and get install instructions.

When you encounter a project with a chitty.md file, follow its instructions."#;

/// System tools that Chitty (orchestrator) always has access to.
/// Package tools are accessed via dispatch_agents, not directly.
pub const ORCHESTRATOR_TOOLS: &[&str] = &[
    "file_reader", "file_writer", "file_editor", "terminal", "code_search", "code_outline",
    "save_memory", "create_tool", "install_package", "browser",
    "load_skill", "dispatch_agents", "execute_package_tool", "ask_user_questions",
    "open_agent_panel", "web_search", "web_scraper", "check_session",
    "generate_image", "edit_image", "generate_video", "text_to_speech",
];

/// Chat engine — stateless functions that operate on a database connection.
pub struct ChatEngine;

impl ChatEngine {
    /// Create a new conversation.
    pub fn create_conversation(
        conn: &Connection,
        provider: &str,
        model: &str,
        title: Option<&str>,
    ) -> Result<Conversation> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();

        conn.execute(
            "INSERT INTO conversations (id, title, provider, model, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, title, provider, model, now, now],
        ).context("Failed to create conversation")?;

        Ok(Conversation {
            id,
            title: title.map(|s| s.to_string()),
            agent_id: None,
            project_path: None,
            provider: provider.to_string(),
            model: model.to_string(),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// Save a message to the database.
    pub fn save_message(
        conn: &Connection,
        conversation_id: &str,
        role: &str,
        content: &str,
        tool_calls: Option<&serde_json::Value>,
        tool_call_id: Option<&str>,
    ) -> Result<Message> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let tool_calls_json = tool_calls.map(|tc| serde_json::to_string(tc).unwrap_or_default());

        conn.execute(
            "INSERT INTO messages (id, conversation_id, role, content, tool_calls, tool_call_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![id, conversation_id, role, content, tool_calls_json, tool_call_id, now],
        ).context("Failed to save message")?;

        // Update conversation timestamp
        conn.execute(
            "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
            rusqlite::params![now, conversation_id],
        )?;

        Ok(Message {
            id,
            conversation_id: conversation_id.to_string(),
            parent_message_id: None,
            role: role.to_string(),
            content: content.to_string(),
            tool_calls: tool_calls.cloned(),
            tool_call_id: tool_call_id.map(|s| s.to_string()),
            token_count: None,
            created_at: now,
        })
    }

    /// Update conversation title.
    pub fn update_title(conn: &Connection, conversation_id: &str, title: &str) -> Result<()> {
        conn.execute(
            "UPDATE conversations SET title = ?1 WHERE id = ?2",
            rusqlite::params![title, conversation_id],
        )?;
        Ok(())
    }

    /// List conversations, newest first. Optionally filter by agent_id.
    /// Pass `Some("")` to get conversations with no agent, `Some(id)` for a
    /// specific agent, or `None` for all conversations.
    pub fn list_conversations(conn: &Connection, agent_id: Option<&str>) -> Result<Vec<Conversation>> {
        let (sql, params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) = match agent_id {
            Some("") => (
                "SELECT id, title, agent_id, project_path, provider, model, created_at, updated_at \
                 FROM conversations WHERE agent_id IS NULL OR agent_id = '' \
                 ORDER BY updated_at DESC".to_string(),
                vec![],
            ),
            Some(id) => (
                "SELECT id, title, agent_id, project_path, provider, model, created_at, updated_at \
                 FROM conversations WHERE agent_id = ?1 \
                 ORDER BY updated_at DESC".to_string(),
                vec![Box::new(id.to_string())],
            ),
            None => (
                "SELECT id, title, agent_id, project_path, provider, model, created_at, updated_at \
                 FROM conversations ORDER BY updated_at DESC".to_string(),
                vec![],
            ),
        };

        let mut stmt = conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok(Conversation {
                id: row.get(0)?,
                title: row.get(1)?,
                agent_id: row.get(2)?,
                project_path: row.get(3)?,
                provider: row.get(4)?,
                model: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })?;

        let mut conversations = Vec::new();
        for row in rows {
            conversations.push(row?);
        }
        Ok(conversations)
    }

    /// Get all messages in a conversation.
    pub fn get_messages(conn: &Connection, conversation_id: &str) -> Result<Vec<Message>> {
        let mut stmt = conn.prepare(
            "SELECT id, conversation_id, parent_message_id, role, content, tool_calls, tool_call_id, token_count, created_at FROM messages WHERE conversation_id = ?1 ORDER BY created_at ASC",
        )?;

        let rows = stmt.query_map(rusqlite::params![conversation_id], |row| {
            let tool_calls_str: Option<String> = row.get(5)?;
            let tool_calls = tool_calls_str.and_then(|s| serde_json::from_str(&s).ok());

            Ok(Message {
                id: row.get(0)?,
                conversation_id: row.get(1)?,
                parent_message_id: row.get(2)?,
                role: row.get(3)?,
                content: row.get(4)?,
                tool_calls,
                tool_call_id: row.get(6)?,
                token_count: row.get(7)?,
                created_at: row.get(8)?,
            })
        })?;

        let mut messages = Vec::new();
        for row in rows {
            messages.push(row?);
        }
        Ok(messages)
    }

    /// Delete a conversation and all its messages (CASCADE).
    pub fn delete_conversation(conn: &Connection, conversation_id: &str) -> Result<()> {
        conn.execute(
            "DELETE FROM conversations WHERE id = ?1",
            rusqlite::params![conversation_id],
        )?;
        Ok(())
    }

    /// Assemble context for an LLM call.
    ///
    /// Builds system prompt with:
    /// 1. Base prompt (from agent persona or default)
    /// 2. Project context (chitty.md)
    /// 3. Relevant memories
    /// 4. Skill catalog (available skills — names + descriptions)
    /// 5. Tool agent instructions (auto-injected from tools — DataVisions pattern)
    /// 6. Tool definitions (filtered by skills' allowed-tools)
    ///
    /// Returns the assembled context, execution config, and the effective project path
    /// (which may come from the agent if none was provided in the request).
    ///
    /// `all_tool_defs` should be the full list of available tools from ToolRuntime.
    /// `skill_registry` provides skill discovery and catalog generation.
    pub fn assemble_context(
        conn: &Connection,
        conversation_id: &str,
        agent_id: Option<&str>,
        project_path: Option<&str>,
        all_tool_defs: &[ToolDefinition],
        skill_registry: &crate::skills::SkillRegistry,
    ) -> Result<(AssembledContext, ExecutionConfig, Option<String>)> {
        // 1. Load agent → get base prompt, skill names, execution config, agent project_path
        let (base_prompt, agent_skills, exec_config, agent_project_path) = if let Some(sid) = agent_id {
            match AgentsManager::load(conn, sid) {
                Ok(Some(agent)) => {
                    // Check if any of the agent's skills require the browser tool
                    // (for setting default iterations)
                    let required_tools = skill_registry.union_tools(&agent.skills);
                    let has_browser = agent.skills.is_empty() || required_tools.contains("browser");
                    let default_iters = if has_browser { 25 } else { 10 };

                    // Load sub-agent scoped tools if this is a sub-agent (has parent)
                    let sub_agent_tools = if agent.parent_agent_id.is_some() {
                        AgentsManager::load_sub_agent_tools(conn, sid).unwrap_or_default()
                    } else {
                        Vec::new()
                    };

                    // Sub-agents inherit auto_approve from parent
                    let auto_approve = agent.approval_mode == "auto"
                        || agent.parent_agent_id.as_ref().map_or(false, |pid| {
                            AgentsManager::load(conn, pid).ok().flatten()
                                .map_or(false, |p| p.approval_mode == "auto")
                        });

                    (
                        agent.persona,
                        agent.skills,
                        ExecutionConfig {
                            max_iterations: agent.max_iterations.unwrap_or(default_iters),
                            temperature: agent.temperature,
                            max_tokens: agent.max_tokens,
                            auto_approve,
                            context_budget_pct: agent.context_budget_pct.unwrap_or(75),
                            compaction_strategy: agent.compaction_strategy.unwrap_or_else(|| "truncate".to_string()),
                            max_conversation_turns: agent.max_conversation_turns,
                            context_length: agent.context_length,
                            sub_agent_tools,
                        },
                        agent.project_path,
                    )
                }
                _ => (
                    CHITTY_SYSTEM_PROMPT.to_string(),
                    vec![],
                    ExecutionConfig { max_iterations: 25, ..ExecutionConfig::default() },
                    None,
                ),
            }
        } else {
            // Load Chitty config from marketplace package (editable on disk)
            // Falls back to hardcoded constants if package files are missing
            if let Some((persona, _tools, exec_cfg)) = load_chitty_package() {
                (persona, vec![], exec_cfg, None)
            } else {
                tracing::warn!("Chitty package not found on disk, using hardcoded defaults");
                (
                    CHITTY_SYSTEM_PROMPT.to_string(),
                    vec![],
                    ExecutionConfig { max_iterations: 25, ..ExecutionConfig::default() },
                    None,
                )
            }
        };

        // Effective project path: request's project_path takes priority, then agent's
        let effective_project_path = project_path
            .map(|s| s.to_string())
            .or(agent_project_path);

        let mut system_parts: Vec<String> = vec![base_prompt];

        // 2. Project path + context (chitty.md — auto-generated if missing)
        if let Some(ref path) = effective_project_path {
            system_parts.push(format!(
                "\n\n## Current Project\nProject directory: {}\n\
                 All file tool paths are relative to this directory. Use relative paths (e.g. \".\" or \"src/\").\n\
                 When the user asks about \"my code\" or \"the project\", use file_reader to scan and read files immediately — do not ask which files.",
                path
            ));

            let project_path = std::path::Path::new(path);
            let project_ctx = match context::load_project_context(project_path) {
                Ok(Some(ctx)) => Some(ctx),
                _ => {
                    // Auto-generate chitty.md on first use
                    match context::auto_generate(project_path) {
                        Ok(ctx) => {
                            tracing::info!("Auto-generated chitty.md for {}", path);
                            Some(ctx)
                        }
                        Err(e) => {
                            tracing::warn!("Failed to auto-generate chitty.md: {}", e);
                            None
                        }
                    }
                }
            };

            if let Some(ctx) = project_ctx {
                system_parts.push(format!("\n\n## Project Context\n{}", ctx.content));
            }
        }

        // 3. Memories
        if let Ok(memories) = memory::MemoryManager::load_relevant(
            conn,
            effective_project_path.as_deref(),
            agent_id,
        ) {
            if !memories.is_empty() {
                let mem_text = memory::MemoryManager::format_as_context(&memories);
                system_parts.push(format!("\n\n{}", mem_text));
            }
        }

        // 4. Skill catalog (available skills — metadata only, ~50-100 tokens per skill)
        let skill_catalog = skill_registry.build_catalog_xml(&agent_skills);
        if !skill_catalog.is_empty() {
            system_parts.push(format!("\n\n{}", skill_catalog));
        }

        // 4b. Package agent catalog with tool names (for orchestrator)
        // Includes tool names so Chitty can choose between Tier 1 (execute_package_tool) and Tier 2 (dispatch_agents)
        if agent_id.is_none() {
            let pkg_agents = AgentsManager::list(conn)?;
            let pkg_list: Vec<String> = pkg_agents
                .iter()
                .filter(|a| a.package_id.is_some())
                .map(|a| {
                    // Find tools belonging to this package
                    let pkg_id = a.package_id.as_deref().unwrap_or("");
                    let pkg_name = pkg_id.strip_prefix("pkg-").unwrap_or(pkg_id);
                    let tool_names: Vec<&str> = all_tool_defs.iter()
                        .filter(|d| {
                            d.vendor.as_deref().map(|v| v.eq_ignore_ascii_case(pkg_name)).unwrap_or(false)
                                || d.name.starts_with(&format!("{}_", pkg_name.replace('-', "_")))
                        })
                        .map(|d| d.name.as_str())
                        .collect();
                    let tools_str = if tool_names.is_empty() {
                        String::new()
                    } else {
                        format!(" | Tools: [{}]", tool_names.join(", "))
                    };
                    format!("- **{}** ({}): {}{}", a.name, pkg_name, a.description, tools_str)
                })
                .collect();
            if !pkg_list.is_empty() {
                system_parts.push(format!(
                    "\n\n## Available Package Agents\n\
                    Use `execute_package_tool` (Tier 1) when you know the exact tool + args.\n\
                    Use `dispatch_agents` (Tier 2) when the task needs reasoning or multiple tool calls.\n\n{}",
                    pkg_list.join("\n")
                ));
            }
        }

        // 5. Filter tool definitions based on agent type
        let is_orchestrator = agent_id.is_none(); // Default Chitty = orchestrator
        let is_package_agent = agent_id.map(|id| id.starts_with("pkg-")).unwrap_or(false);

        let filtered_defs: Vec<&ToolDefinition> = if is_orchestrator {
            // Chitty orchestrator: tools from package SKILL.md allowed-tools, fallback to hardcoded
            let allowed = load_chitty_package()
                .map(|(_, tools, _)| tools)
                .unwrap_or_default();
            if allowed.is_empty() {
                // Fallback to hardcoded constant
                all_tool_defs
                    .iter()
                    .filter(|d| ORCHESTRATOR_TOOLS.contains(&d.name.as_str()))
                    .collect()
            } else {
                all_tool_defs
                    .iter()
                    .filter(|d| allowed.iter().any(|t| t == &d.name))
                    .collect()
            }
        } else if is_package_agent || agent_skills.is_empty() {
            // Package agent or agent with no skill filter: all tools available
            all_tool_defs.iter().collect()
        } else {
            let mut required_tools = skill_registry.union_tools(&agent_skills);
            // Always include base tools regardless of skill selection
            for base in &["load_skill", "save_memory", "file_reader"] {
                required_tools.insert(base.to_string());
            }
            all_tool_defs
                .iter()
                .filter(|d| required_tools.contains(&d.name))
                .collect()
        };

        // 6. Tool Agent Instructions (auto-injected from tools themselves)
        // This is the DataVisions pattern — tools self-describe their usage
        let instruction_parts: Vec<String> = filtered_defs
            .iter()
            .filter_map(|d| {
                d.instructions
                    .as_ref()
                    .map(|inst| format!("### {}\n{}", d.display_name, inst))
            })
            .collect();
        if !instruction_parts.is_empty() {
            system_parts.push(format!(
                "\n\n## Tool Instructions\n\n{}",
                instruction_parts.join("\n\n")
            ));
        }

        let system_prompt = system_parts.join("");

        // 7. Tool definitions in OpenAI function calling format
        let tools: Vec<serde_json::Value> = filtered_defs
            .iter()
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
            .collect();

        // 8. Load conversation messages and convert to ChatMessage format
        let db_messages = Self::get_messages(conn, conversation_id)?;
        let messages: Vec<ChatMessage> = db_messages
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| {
                let tool_calls = m.tool_calls.as_ref().and_then(|tc| {
                    serde_json::from_value::<Vec<crate::providers::ToolCall>>(tc.clone()).ok()
                });
                ChatMessage {
                    role: m.role.clone(),
                    content: m.content.clone(),
                    tool_calls,
                    tool_call_id: m.tool_call_id.clone(),
                }
            })
            .collect();

        Ok((
            AssembledContext {
                system_prompt,
                messages,
                tools,
            },
            exec_config,
            effective_project_path,
        ))
    }
}
