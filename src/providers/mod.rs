//! LLM Provider abstraction layer
//!
//! Supports BYOK cloud providers (OpenAI, Anthropic, Google, xAI)
//! and local runtimes (Ollama, HuggingFace sidecar).

pub mod adaptors;
pub mod cloud;
pub mod local_sidecar;
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
    /// Approval request — agent wants to perform a sensitive action
    ApprovalRequest {
        /// Unique ID for this approval request
        approval_id: String,
        /// The tool name requesting approval
        tool_name: String,
        /// Human-readable description of the action
        action_description: String,
        /// Structured details for the UI card
        details: serde_json::Value,
    },
    /// Token usage reported by the provider (sent at end of stream)
    TokenUsage {
        input_tokens: u32,
        output_tokens: u32,
        /// Tokens read from cache (Anthropic: cache_read_input_tokens, OpenAI/xAI: cached_tokens)
        cache_read_tokens: u32,
        /// Tokens written to cache (Anthropic: cache_creation_input_tokens)
        cache_write_tokens: u32,
    },
    /// Context usage info (sent before each LLM call for UI progress bar)
    ContextInfo {
        used_tokens: u32,
        max_tokens: u32,
        percentage: u8,
    },
    /// Multi-agent dispatch: agent started working
    AgentStart {
        agent_name: String,
        agent_icon: String,
        instruction: String,
    },
    /// Multi-agent dispatch: agent is generating text
    AgentText {
        agent_name: String,
        text: String,
    },
    /// Multi-agent dispatch: agent called a tool
    AgentToolCall {
        agent_name: String,
        tool_name: String,
        tool_args: serde_json::Value,
    },
    /// Multi-agent dispatch: agent tool result
    AgentToolResult {
        agent_name: String,
        tool_name: String,
        success: bool,
        result_preview: String,
        duration_ms: u64,
    },
    /// Multi-agent dispatch: agent completed
    AgentComplete {
        agent_name: String,
        response: String,
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
