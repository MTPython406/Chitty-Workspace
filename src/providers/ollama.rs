//! Ollama local model provider
//!
//! Connects to Ollama running on localhost:11434.
//! Uses Ollama's OpenAI-compatible /v1/chat/completions for chat,
//! plus native Ollama API for model management (/api/tags, /api/pull, /api/ps).

use super::adaptors::openai_compat;
use super::{ChatMessage, Model, Provider, ProviderId, StreamChunk};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

// ─── Types ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OllamaStatus {
    pub running: bool,
    pub version: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OllamaModel {
    pub name: String,
    pub model: String,
    pub modified_at: Option<String>,
    pub size: Option<u64>,
    pub digest: Option<String>,
    pub details: Option<OllamaModelDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OllamaModelDetails {
    pub parent_model: Option<String>,
    pub format: Option<String>,
    pub family: Option<String>,
    pub families: Option<Vec<String>>,
    pub parameter_size: Option<String>,
    pub quantization_level: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningModel {
    pub name: String,
    pub model: String,
    pub size: Option<u64>,
    pub size_vram: Option<u64>,
    pub digest: Option<String>,
    pub expires_at: Option<String>,
    pub details: Option<OllamaModelDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullProgress {
    pub status: String,
    pub digest: Option<String>,
    pub total: Option<u64>,
    pub completed: Option<u64>,
}

// ─── Provider ─────────────────────────────────────────────

pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
}

impl OllamaProvider {
    pub fn new(base_url: String) -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            base_url,
        }
    }

    // ─── Management methods (not part of Provider trait) ──

    /// Check if Ollama is running.
    pub async fn check_status(&self) -> OllamaStatus {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_default();

        match client.get(&self.base_url).send().await {
            Ok(resp) => {
                let body = resp.text().await.unwrap_or_default();
                OllamaStatus {
                    running: true,
                    version: Some(body.trim().to_string()),
                    error: None,
                }
            }
            Err(e) => OllamaStatus {
                running: false,
                version: None,
                error: Some(format!(
                    "Cannot connect to Ollama at {}: {}",
                    self.base_url, e
                )),
            },
        }
    }

    /// List all installed models via GET /api/tags.
    pub async fn list_ollama_models(&self) -> Result<Vec<OllamaModel>> {
        let url = format!("{}/api/tags", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to connect to Ollama /api/tags")?;

        let body: Value = resp
            .json()
            .await
            .context("Failed to parse Ollama /api/tags response")?;

        let models: Vec<OllamaModel> = serde_json::from_value(
            body.get("models").cloned().unwrap_or(Value::Array(vec![])),
        )
        .unwrap_or_default();

        info!("Ollama: found {} installed models", models.len());
        Ok(models)
    }

    /// List running/loaded models via GET /api/ps.
    pub async fn running_models(&self) -> Result<Vec<RunningModel>> {
        let url = format!("{}/api/ps", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to connect to Ollama /api/ps")?;

        let body: Value = resp
            .json()
            .await
            .context("Failed to parse Ollama /api/ps response")?;

        let models: Vec<RunningModel> = serde_json::from_value(
            body.get("models").cloned().unwrap_or(Value::Array(vec![])),
        )
        .unwrap_or_default();

        debug!("Ollama: {} models currently loaded", models.len());
        Ok(models)
    }

    /// Pull/download a model via POST /api/pull (non-streaming for simplicity).
    pub async fn pull_model(&self, model_name: &str) -> Result<Value> {
        let url = format!("{}/api/pull", self.base_url);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3600))
            .build()?;

        info!("Ollama pull: starting download of '{}'", model_name);

        let resp = client
            .post(&url)
            .json(&json!({
                "name": model_name,
                "stream": false,
            }))
            .send()
            .await
            .context("Failed to send pull request to Ollama")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let error_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Ollama pull failed ({}): {}", status, error_body);
        }

        let result: Value = resp
            .json()
            .await
            .context("Failed to parse Ollama pull response")?;
        info!("Ollama pull complete: '{}'", model_name);
        Ok(result)
    }

    /// Pull model with streaming progress — returns lines of JSON progress.
    pub async fn pull_model_streaming(
        &self,
        model_name: &str,
    ) -> Result<reqwest::Response> {
        let url = format!("{}/api/pull", self.base_url);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3600))
            .build()?;

        let resp = client
            .post(&url)
            .json(&json!({
                "name": model_name,
                "stream": true,
            }))
            .send()
            .await
            .context("Failed to send pull request to Ollama")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let error_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Ollama pull failed ({}): {}", status, error_body);
        }

        Ok(resp)
    }

    /// Delete a model via DELETE /api/delete.
    pub async fn delete_model(&self, model_name: &str) -> Result<()> {
        let url = format!("{}/api/delete", self.base_url);
        let resp = self
            .client
            .delete(&url)
            .json(&json!({ "name": model_name }))
            .send()
            .await
            .context("Failed to send delete request to Ollama")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let error_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Ollama delete failed ({}): {}", status, error_body);
        }

        info!("Ollama model deleted: '{}'", model_name);
        Ok(())
    }

    /// Unload a model from VRAM via POST /api/generate with keep_alive=0.
    pub async fn unload_model(&self, model_name: &str) -> Result<()> {
        let url = format!("{}/api/generate", self.base_url);
        info!("Ollama unload: '{}'", model_name);

        let resp = self
            .client
            .post(&url)
            .json(&json!({
                "model": model_name,
                "keep_alive": 0,
            }))
            .send()
            .await
            .context("Failed to send unload request to Ollama")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let error_body = resp.text().await.unwrap_or_default();
            warn!("Ollama unload warning ({}): {}", status, error_body);
        }

        info!("Ollama unload complete: '{}'", model_name);
        Ok(())
    }

    /// Determine if a model family likely supports tool calling.
    pub fn model_supports_tools_static(model_name: &str, details: &Option<OllamaModelDetails>) -> bool {
        let name_lower = model_name.to_lowercase();
        // Known tool-capable families
        let tool_families = ["llama3", "mistral", "qwen", "command-r", "gemma2"];

        if let Some(d) = details {
            if let Some(ref family) = d.family {
                let fam_lower = family.to_lowercase();
                return tool_families.iter().any(|f| fam_lower.contains(f));
            }
        }

        // Fallback: check model name
        tool_families.iter().any(|f| name_lower.contains(f))
    }
}

#[async_trait::async_trait]
impl Provider for OllamaProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Ollama
    }

    async fn list_models(&self) -> Result<Vec<Model>> {
        let ollama_models = self.list_ollama_models().await?;
        Ok(ollama_models
            .into_iter()
            .map(|m| {
                let supports_tools =
                    Self::model_supports_tools_static(&m.name, &m.details);
                Model {
                    id: m.name.clone(),
                    provider: ProviderId::Ollama,
                    display_name: m.name,
                    context_window: None, // Ollama doesn't expose this in /api/tags
                    supports_tools,
                    supports_streaming: true,
                }
            })
            .collect())
    }

    async fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: Option<&[serde_json::Value]>,
    ) -> Result<ChatMessage> {
        // Use Ollama's OpenAI-compatible endpoint
        let body = openai_compat::build_request_body(model, messages, tools, false, false);

        let response = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send request to Ollama")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            anyhow::bail!("Ollama API error ({}): {}", status, error_body);
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
        // Ollama doesn't support stream_options.include_usage
        let body = openai_compat::build_request_body(model, messages, tools, true, false);

        let msg_count = messages.len();
        let tool_count = tools.map_or(0, |t| t.len());
        tracing::info!(
            "Ollama chat_stream: model={}, messages={}, tools={}",
            model,
            msg_count,
            tool_count
        );

        let response = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send request to Ollama")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            tracing::error!("Ollama API error ({}): {}", status, error_body);
            let _ = tx
                .send(StreamChunk::Error(format!(
                    "Ollama API error ({}): {}",
                    status, error_body
                )))
                .await;
            return Ok(());
        }

        // Delegate to shared OpenAI-compatible SSE parser
        openai_compat::stream_openai_sse(response, tx).await
    }
}
