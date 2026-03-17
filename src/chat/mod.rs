//! Chat engine
//!
//! Manages conversations, message history, tool call loops,
//! and persistence to local SQLite.
//!
//! Context assembly order for each conversation:
//! 1. Base system prompt (from skill or default)
//! 2. Project context (from chitty.md if in a project directory)
//! 3. Relevant memories (global + project-scoped + skill-scoped)
//! 4. Tool definitions (from skill + native + custom)
//! 5. Conversation history (messages, trimmed to fit context window)
//! 6. User message

pub mod context;
pub mod memory;

use serde::{Deserialize, Serialize};

/// A chat conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub title: Option<String>,
    pub skill_id: Option<String>,
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
    /// System prompt (skill instructions + project context + memories)
    pub system_prompt: String,
    /// Conversation messages (trimmed to fit)
    pub messages: Vec<Message>,
    /// Tool definitions available for this call
    pub tools: Vec<serde_json::Value>,
}

/// Default system prompt when no skill is active
pub const DEFAULT_SYSTEM_PROMPT: &str = r#"You are Chitty, a helpful AI assistant running locally on the user's machine.

You have access to tools that can read/write files, run terminal commands, search code, and more.
Use tools when they help accomplish the user's request. Be direct and concise.

When you learn something important about the user or their preferences, use the save_memory tool
to remember it for future conversations.

When you encounter a project with a chitty.md file, follow its instructions."#;
