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
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            max_iterations: 10,
            temperature: None,
            max_tokens: None,
        }
    }
}

/// Default system prompt when no agent is active
pub const DEFAULT_SYSTEM_PROMPT: &str = r#"You are Chitty, a helpful AI assistant running locally on the user's machine.

You have access to tools that can read/write files, run terminal commands, search code, and more.
Use tools when they help accomplish the user's request. Be direct and concise.

When you learn something important about the user or their preferences, use the save_memory tool
to remember it for future conversations.

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

    /// List all conversations, newest first.
    pub fn list_conversations(conn: &Connection) -> Result<Vec<Conversation>> {
        let mut stmt = conn.prepare(
            "SELECT id, title, agent_id, project_path, provider, model, created_at, updated_at FROM conversations ORDER BY updated_at DESC",
        )?;

        let rows = stmt.query_map([], |row| {
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
    /// 1. Base prompt (from agent or default)
    /// 2. Project context (chitty.md)
    /// 3. Relevant memories
    /// 4. Tool agent instructions (auto-injected from tools — DataVisions pattern)
    ///
    /// Returns the assembled context, execution config, and the effective project path
    /// (which may come from the agent if none was provided in the request).
    ///
    /// `all_tool_defs` should be the full list of available tools from ToolRuntime.
    /// The agent's tool list filters which ones to include.
    pub fn assemble_context(
        conn: &Connection,
        conversation_id: &str,
        agent_id: Option<&str>,
        project_path: Option<&str>,
        all_tool_defs: &[ToolDefinition],
    ) -> Result<(AssembledContext, ExecutionConfig, Option<String>)> {
        // 1. Load agent → get base prompt, tool names, execution config, agent project_path
        let (base_prompt, tool_names, exec_config, agent_project_path) = if let Some(sid) = agent_id {
            match AgentsManager::load(conn, sid) {
                Ok(Some(agent)) => {
                    let tool_list: Option<Vec<String>> = if agent.tools.is_empty() {
                        None // empty = all tools
                    } else {
                        Some(agent.tools)
                    };
                    // If agent has browser tool, allow more iterations (browser actions
                    // consume iterations quickly: open, screenshot, click, type, etc.)
                    let has_browser = tool_list.as_ref().map_or(true, |names| names.iter().any(|t| t == "browser"));
                    let default_iters = if has_browser { 25 } else { 10 };
                    (
                        agent.instructions,
                        tool_list,
                        ExecutionConfig {
                            max_iterations: agent.max_iterations.unwrap_or(default_iters),
                            temperature: agent.temperature,
                            max_tokens: agent.max_tokens,
                        },
                        agent.project_path,
                    )
                }
                _ => (
                    DEFAULT_SYSTEM_PROMPT.to_string(),
                    None,
                    ExecutionConfig::default(),
                    None,
                ),
            }
        } else {
            (
                DEFAULT_SYSTEM_PROMPT.to_string(),
                None,
                ExecutionConfig::default(),
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

        // 4. Filter tool definitions based on agent's tool list
        let filtered_defs: Vec<&ToolDefinition> = match &tool_names {
            Some(names) => all_tool_defs
                .iter()
                .filter(|d| names.contains(&d.name))
                .collect(),
            None => all_tool_defs.iter().collect(),
        };

        // 5. Tool Agent Instructions (auto-injected from tools themselves)
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

        // 6. Tool definitions in OpenAI function calling format
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

        // 7. Load conversation messages and convert to ChatMessage format
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
