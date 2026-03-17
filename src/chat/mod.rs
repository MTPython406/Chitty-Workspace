//! Chat engine
//!
//! Manages conversations, message history, tool call loops,
//! and persistence to local SQLite.

use serde::{Deserialize, Serialize};

/// A chat conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub title: Option<String>,
    pub skill_id: Option<String>,
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
    pub role: String,
    pub content: String,
    pub tool_calls: Option<serde_json::Value>,
    pub tool_call_id: Option<String>,
    pub created_at: String,
}

// TODO: Implement
// - Chat loop (user message -> LLM -> tool calls -> tool results -> LLM -> response)
// - Conversation CRUD in SQLite
// - Message history management
// - Streaming response handling
// - Context window management (truncation/summarization)
