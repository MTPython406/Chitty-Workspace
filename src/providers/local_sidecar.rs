//! Local Sidecar provider (HuggingFace GGUF models)
//!
//! Routes chat to the Python inference sidecar running on localhost:8766.
//! Uses the same OpenAI-compatible /chat/completions endpoint format.

use super::adaptors::openai_compat;
use super::{ChatMessage, Model, Provider, ProviderId, StreamChunk};

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::info;

// ─── Provider ─────────────────────────────────────────────

pub struct LocalSidecarProvider {
    client: reqwest::Client,
    base_url: String,
}

impl LocalSidecarProvider {
    pub fn new(base_url: String) -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                // Model loading can be slow — give generous timeout for first inference
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            base_url,
        }
    }

    /// Check if the sidecar is running and reachable.
    pub async fn is_running(&self) -> bool {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap_or_default();

        client
            .get(format!("{}/health", self.base_url))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }
}

#[async_trait::async_trait]
impl Provider for LocalSidecarProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Local
    }

    async fn list_models(&self) -> Result<Vec<Model>> {
        let resp = self
            .client
            .get(format!("{}/models", self.base_url))
            .send()
            .await
            .context("Failed to connect to local sidecar")?;

        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let models = body
            .get("models")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default();

        Ok(models
            .into_iter()
            .filter_map(|m| {
                let name = m.get("name")?.as_str()?.to_string();
                Some(Model {
                    id: name.clone(),
                    provider: ProviderId::Local,
                    display_name: name,
                    context_window: None,
                    supports_tools: true, // llama-cpp-python supports tool calling for Qwen, Llama, etc.
                    supports_streaming: true,
                })
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

        info!("LocalSidecar chat: model={}, messages={}", model, messages.len());

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send request to local sidecar. Is the sidecar running?")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            anyhow::bail!("Local sidecar error ({}): {}", status, error_body);
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
        let body = openai_compat::build_request_body(model, messages, tools, true, false);

        info!(
            "LocalSidecar chat_stream: model={}, messages={}, tools={}",
            model,
            messages.len(),
            tools.map_or(0, |t| t.len())
        );

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send request to local sidecar. Is the sidecar running?")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            tracing::error!("Local sidecar API error ({}): {}", status, error_body);
            let _ = tx
                .send(StreamChunk::Error(format!(
                    "Local sidecar error ({}): {}",
                    status, error_body
                )))
                .await;
            return Ok(());
        }

        // Delegate to shared OpenAI-compatible SSE parser
        openai_compat::stream_openai_sse(response, tx).await
    }
}
