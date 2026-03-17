//! LLM Provider abstraction layer
//!
//! Supports BYOK cloud providers (OpenAI, Anthropic, Google, xAI)
//! and local runtimes (Ollama, HuggingFace sidecar).

pub mod adaptors;
pub mod cloud;
pub mod ollama;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

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

impl std::str::FromStr for ProviderId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "openai" => Ok(Self::Openai),
            "anthropic" => Ok(Self::Anthropic),
            "google" => Ok(Self::Google),
            "xai" => Ok(Self::Xai),
            "ollama" => Ok(Self::Ollama),
            "huggingface" => Ok(Self::Huggingface),
            _ => Err(anyhow::anyhow!("Unknown provider: {}", s)),
        }
    }
}

/// A configured provider with API key
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: ProviderId,
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
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

/// Chat message (provider-agnostic format)
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

/// Streaming chunk from provider (and tool execution loop)
///
/// Mirrors DataVisions StreamEvent — slim version with the events
/// needed for tool calling, execution feedback, and iteration tracking.
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// Text content from the LLM
    Text(String),
    /// LLM wants to call a tool (start of tool call)
    ToolCallStart { id: String, name: String },
    /// Partial arguments for an in-progress tool call
    ToolCallDelta { id: String, arguments: String },
    /// Tool call arguments complete
    ToolCallEnd { id: String },
    /// Tool execution result (after local execution)
    ToolResult {
        id: String,
        name: String,
        content: String,
        success: bool,
        duration_ms: u64,
    },
    /// Agent thinking/processing status
    Thinking(String),
    /// Iteration tracking for tool call loop
    IterationStart {
        iteration: u32,
        max_iterations: u32,
    },
    /// Conversation metadata (sent once at start)
    Meta {
        conversation_id: String,
        message_id: Option<String>,
    },
    /// Stream complete
    Done,
    /// Error occurred
    Error(String),
}

/// Trait all providers implement
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// Provider identifier
    fn id(&self) -> ProviderId;

    /// List available models
    async fn list_models(&self) -> anyhow::Result<Vec<Model>>;

    /// Non-streaming chat completion
    async fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: Option<&[serde_json::Value]>,
    ) -> anyhow::Result<ChatMessage>;

    /// Streaming chat completion — sends chunks to the provided channel
    async fn chat_stream(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: Option<&[serde_json::Value]>,
        tx: mpsc::Sender<StreamChunk>,
    ) -> anyhow::Result<()>;
}
