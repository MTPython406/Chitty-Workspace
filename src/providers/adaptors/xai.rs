//! xAI Adaptor — Grok models via OpenAI-compatible REST API
//!
//! Base URL: https://api.x.ai/v1
//! Supports: chat completions (streaming + tools), model listing, vision
//!
//! This adaptor uses the OpenAI-compatible format that xAI provides.
//! It can also be reused for other OpenAI-compatible providers with minor tweaks.

use super::{DiscoveredModel, ProviderCapabilities};
use super::openai_compat;
use crate::providers::{ChatMessage, Model, Provider, ProviderId, StreamChunk};

use anyhow::{Context, Result};
use tokio::sync::mpsc;

/// xAI provider (Grok models)
pub struct XaiProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl XaiProvider {
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            api_key,
            base_url: base_url.unwrap_or_else(|| "https://api.x.ai/v1".to_string()),
        }
    }

    /// Fetch available models from the xAI /v1/models endpoint.
    pub async fn discover_models(&self) -> Result<Vec<DiscoveredModel>> {
        let response = self
            .client
            .get(format!("{}/models", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .context("Failed to fetch models from xAI")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("xAI models API error ({}): {}", status, body);
        }

        let resp: serde_json::Value = response.json().await?;

        let mut models = Vec::new();
        if let Some(data) = resp.get("data").and_then(|d| d.as_array()) {
            for item in data {
                let id = item
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                let owned_by = item
                    .get("owned_by")
                    .and_then(|o| o.as_str())
                    .map(|s| s.to_string());

                // Generate a readable display name from the model ID
                let display_name = id
                    .replace('-', " ")
                    .split_whitespace()
                    .map(|w| {
                        let mut c = w.chars();
                        match c.next() {
                            None => String::new(),
                            Some(f) => f.to_uppercase().to_string() + c.as_str(),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ");

                // Determine capabilities based on model ID patterns
                let is_chat = id.contains("grok");
                let is_vision = id.contains("vision");
                let is_embedding = id.contains("embedding");
                let is_image_gen = id.contains("image") || id.contains("aurora");

                models.push(DiscoveredModel {
                    id,
                    display_name,
                    owned_by,
                    context_window: Some(131_072), // xAI models typically 128k
                    capabilities: ProviderCapabilities {
                        chat: is_chat && !is_embedding && !is_image_gen,
                        streaming: is_chat && !is_embedding && !is_image_gen,
                        tool_calling: is_chat && !is_vision && !is_embedding && !is_image_gen,
                        vision: is_vision,
                        image_generation: is_image_gen,
                        audio_input: false,
                        audio_output: false,
                        video_input: false,
                        embeddings: is_embedding,
                        batch_api: false,
                        prompt_caching: false,
                        file_upload: false,
                    },
                });
            }
        }

        Ok(models)
    }
}

#[async_trait::async_trait]
impl Provider for XaiProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Xai
    }

    async fn list_models(&self) -> Result<Vec<Model>> {
        // Return only chat-capable models from discovery
        let discovered = self.discover_models().await?;
        Ok(discovered
            .into_iter()
            .filter(|m| m.capabilities.chat)
            .map(|m| Model {
                id: m.id,
                provider: ProviderId::Xai,
                display_name: m.display_name,
                context_window: m.context_window,
                supports_tools: m.capabilities.tool_calling,
                supports_streaming: m.capabilities.streaming,
            })
            .collect())
    }

    async fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: Option<&[serde_json::Value]>,
    ) -> Result<ChatMessage> {
        let body = openai_compat::build_request_body(model, messages, tools, false, false);

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send request to xAI")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            anyhow::bail!("xAI API error ({}): {}", status, error_body);
        }

        let resp: serde_json::Value = response.json().await?;
        Ok(openai_compat::parse_chat_response(&resp))
    }

    async fn chat_stream(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: Option<&[serde_json::Value]>,
        tx: mpsc::Sender<StreamChunk>,
    ) -> Result<()> {
        let body = openai_compat::build_request_body(model, messages, tools, true, true);

        // Log request details for debugging
        let msg_count = messages.len();
        let tool_count = tools.map_or(0, |t| t.len());
        let body_size = serde_json::to_string(&body).map(|s| s.len()).unwrap_or(0);
        tracing::info!(
            "xAI chat_stream: model={}, messages={}, tools={}, body_size={}",
            model,
            msg_count,
            tool_count,
            body_size
        );

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send request to xAI")?;

        tracing::info!("xAI response status: {}", response.status());

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            tracing::error!("xAI API error ({}): {}", status, error_body);
            let _ = tx
                .send(StreamChunk::Error(format!(
                    "xAI API error ({}): {}",
                    status, error_body
                )))
                .await;
            return Ok(());
        }

        // Delegate to shared OpenAI-compatible SSE parser
        openai_compat::stream_openai_sse(response, tx).await
    }
}
