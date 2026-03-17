//! Chitty Adaptors — Provider integration framework
//!
//! Each adaptor is a feature-rich integration for a specific LLM provider API format.
//! Adaptors handle message format conversion, streaming SSE parsing, tool calling,
//! model discovery, and token tracking.
//!
//! Current adaptors:
//! - `xai` — xAI (Grok models, OpenAI-compatible REST API)
//! - (future) `openai` — OpenAI native
//! - (future) `anthropic` — Anthropic Messages API (currently in cloud.rs)
//! - (future) `google` — Google Gemini API

pub mod xai;

use serde::{Deserialize, Serialize};

/// Capabilities a provider supports — drives UI feature toggles
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub chat: bool,
    pub streaming: bool,
    pub tool_calling: bool,
    pub vision: bool,
    pub image_generation: bool,
    pub audio_input: bool,
    pub audio_output: bool,
    pub video_input: bool,
    pub embeddings: bool,
    pub batch_api: bool,
    pub prompt_caching: bool,
    pub file_upload: bool,
}

/// Model info returned from provider's model listing API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredModel {
    pub id: String,
    pub display_name: String,
    pub owned_by: Option<String>,
    pub context_window: Option<u32>,
    pub capabilities: ProviderCapabilities,
}

/// Token usage from a single LLM call
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    /// Cache-related tokens (for providers that support prompt caching)
    pub cached_tokens: Option<u32>,
}
