//! LLM Provider abstraction layer
//!
//! Supports BYOK cloud providers (OpenAI, Anthropic, Google, xAI)
//! and local runtimes (Ollama, HuggingFace sidecar).

pub mod ollama;
pub mod cloud;

use serde::{Deserialize, Serialize};

/// Unified provider identifier
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderId {
    Openai,
    Anthropic,
    Google,
    Xai,
    Ollama,
    Huggingface,
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Openai => write!(f, "openai"),
            Self::Anthropic => write!(f, "anthropic"),
            Self::Google => write!(f, "google"),
            Self::Xai => write!(f, "xai"),
            Self::Ollama => write!(f, "ollama"),
            Self::Huggingface => write!(f, "huggingface"),
        }
    }
}

/// A configured provider with API key
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: ProviderId,
    pub display_name: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub enabled: bool,
}

/// A model available from a provider
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub provider: ProviderId,
    pub display_name: String,
    pub context_window: Option<u32>,
    pub supports_tools: bool,
    pub supports_streaming: bool,
}

/// Chat message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Tool call from the model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Streaming chunk from provider
#[derive(Debug, Clone)]
pub enum StreamChunk {
    Text(String),
    ToolCallStart { id: String, name: String },
    ToolCallDelta { id: String, arguments: String },
    ToolCallEnd { id: String },
    Done,
    Error(String),
}

/// Trait all providers implement
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    async fn list_models(&self) -> anyhow::Result<Vec<Model>>;

    async fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: Option<&[serde_json::Value]>,
    ) -> anyhow::Result<ChatMessage>;

    // TODO: streaming chat
}
