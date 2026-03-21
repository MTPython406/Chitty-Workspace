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
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            max_iterations: 10,
            temperature: None,
            max_tokens: None,
            auto_approve: false,
            context_budget_pct: 75,
            compaction_strategy: "truncate".to_string(),
            max_conversation_turns: None,
        }
    }
}

/// Default system prompt when no agent is active — Chitty is the system administrator
pub const CHITTY_SYSTEM_PROMPT: &str = r#"You are Chitty, the built-in system administrator and AI assistant for Chitty Workspace — a local-first AI assistant running 100% on the user's machine.

Be direct and concise. Use tools when they help. Tool definitions are provided separately — refer to them for parameters and usage.

## Key System Knowledge

**Data locations:** Config at `~/.chitty-workspace/config.toml`, DB at `~/.chitty-workspace/workspace.db`, packages at `~/.chitty-workspace/tools/marketplace/`.

**Providers:** BYOK — OpenAI, Anthropic, Google, xAI (keys in OS keyring). Local: Ollama (localhost:11434), HuggingFace sidecar. Setup via Settings → Providers.

**Skills:** Skills are composable capability packages that bundle instructions + tool requirements. Each skill is a folder with a SKILL.md file (YAML frontmatter + markdown instructions). Skills follow the open Agent Skills standard (agentskills.io). Use `load_skill` to activate a skill when a task matches its description. Available skills are listed in the skill catalog section below.

**Creating skills:** To create a custom skill, write a SKILL.md file with this format:
```
---
name: skill-name
description: What this skill does and when to use it.
allowed-tools: tool1 tool2
---
# Instructions here
```
Save to `~/.chitty-workspace/skills/<skill-name>/SKILL.md` or `<project>/.chitty/skills/<skill-name>/SKILL.md`. Skills are discovered automatically on startup.

**Agents:** An agent = persona + skills + config. Persona is who the agent IS (short identity). Skills define what it can do (each skill brings its own tools). Fields: name, description, persona, skills[], preferred_provider/model, max_iterations, approval_mode. Agent Builder in Action Panel creates agents conversationally. To list agents: `Invoke-RestMethod http://localhost:8770/api/agents` (Windows) or `curl -s http://localhost:8770/api/agents` (Linux/Mac).

**Building agents:** When helping users create agents:
1. Understand what they want the agent to do
2. Recommend appropriate skills from the available catalog
3. Write a short persona (who the agent IS, not what it knows — skills handle expertise)
4. Suggest provider/model if relevant
5. Create via POST to `/api/agents` with persona + skills[]

**Packages:** Marketplace packages can contain skills + custom tools + integrations. Each has `package.json` + optional `SKILL.md` + tool dirs. Scripts read JSON stdin, write JSON stdout.

**Artifacts:** When you produce significant visual output (HTML apps, charts, dashboards), wrap it in artifact tags:
```
<artifact type="html" title="Name">
...complete content...
</artifact>
```
Supported types: html, code, markdown, svg, image. The artifact renders as a preview in the Action Panel.

**Memory:** Save important info with `save_memory`. Types: user/feedback/project/reference. Scopes: global/project/agent. Search before re-asking.

**Project context:** Loads `chitty.md` or `.chitty/chitty.md` automatically. Help generate it by scanning project structure. This file grows over time as the agent learns about the project.

**Browser:** Controls user's real Chrome via extension. User's login sessions available.

**Local API:** Server at `http://localhost:8770`. Endpoints: `/api/agents`, `/api/skills`, `/api/tools`, `/api/providers`, `/api/conversations`, `/api/marketplace/packages`.

**Troubleshooting:** Check API keys in Settings, Ollama via `curl http://localhost:11434/api/tags`, extension status in Activity panel.

When you encounter a project with a chitty.md file, follow its instructions."#;

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
                    (
                        agent.persona,
                        agent.skills,
                        ExecutionConfig {
                            max_iterations: agent.max_iterations.unwrap_or(default_iters),
                            temperature: agent.temperature,
                            max_tokens: agent.max_tokens,
                            auto_approve: agent.approval_mode == "auto",
                            context_budget_pct: agent.context_budget_pct.unwrap_or(75),
                            compaction_strategy: agent.compaction_strategy.unwrap_or_else(|| "truncate".to_string()),
                            max_conversation_turns: agent.max_conversation_turns,
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
            (
                CHITTY_SYSTEM_PROMPT.to_string(),
                vec![], // empty = all skills available (default Chitty agent)
                ExecutionConfig { max_iterations: 25, ..ExecutionConfig::default() },
                None,
            )
        };

        // Effective project path: request's project_path takes priority, then agent's
        let effective_project_path = project_path
            .map(|s| s.to_string())
            .or(agent_project_path);

        let mut system_parts: Vec<String> = vec![base_prompt];

        // 2. Project context (chitty.md)
        if let Some(ref path) = effective_project_path {
            if let Ok(Some(ctx)) = context::load_project_context(std::path::Path::new(path)) {
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

        // 5. Filter tool definitions based on skills' allowed-tools
        // Union all tools required by the agent's skills + base tools
        let filtered_defs: Vec<&ToolDefinition> = if agent_skills.is_empty() {
            // Default agent (no skills selected) or Chitty: all tools available
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
