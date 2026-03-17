//! xAI Adaptor — Grok models via OpenAI-compatible REST API
//!
//! Base URL: https://api.x.ai/v1
//! Supports: chat completions (streaming + tools), model listing, vision
//!
//! This adaptor uses the OpenAI-compatible format that xAI provides.
//! It can also be reused for other OpenAI-compatible providers with minor tweaks.

use super::{DiscoveredModel, ProviderCapabilities, TokenUsage};
use crate::providers::{ChatMessage, Model, Provider, ProviderId, StreamChunk, ToolCall};

use anyhow::{Context, Result};
use futures::StreamExt;
use serde::Deserialize;
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
            client: reqwest::Client::new(),
            api_key,
            base_url: base_url.unwrap_or_else(|| "https://api.x.ai/v1".to_string()),
        }
    }

    /// Build the request body in OpenAI-compatible chat completions format.
    fn build_request_body(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: Option<&[serde_json::Value]>,
        stream: bool,
    ) -> serde_json::Value {
        let api_messages: Vec<serde_json::Value> = messages
            .iter()
            .map(|msg| {
                if msg.role == "tool" {
                    // Tool result message
                    serde_json::json!({
                        "role": "tool",
                        "content": msg.content,
                        "tool_call_id": msg.tool_call_id.as_deref().unwrap_or(""),
                    })
                } else if msg.role == "assistant" {
                    if let Some(ref tool_calls) = msg.tool_calls {
                        // Assistant with tool calls
                        let tc: Vec<serde_json::Value> = tool_calls
                            .iter()
                            .map(|tc| {
                                serde_json::json!({
                                    "id": tc.id,
                                    "type": "function",
                                    "function": {
                                        "name": tc.name,
                                        "arguments": if tc.arguments.is_string() {
                                            tc.arguments.as_str().unwrap_or("{}").to_string()
                                        } else {
                                            serde_json::to_string(&tc.arguments).unwrap_or_default()
                                        },
                                    }
                                })
                            })
                            .collect();
                        let mut obj = serde_json::json!({
                            "role": "assistant",
                            "tool_calls": tc,
                        });
                        if !msg.content.is_empty() {
                            obj["content"] = serde_json::Value::String(msg.content.clone());
                        }
                        obj
                    } else {
                        serde_json::json!({
                            "role": "assistant",
                            "content": msg.content,
                        })
                    }
                } else {
                    // user or system
                    serde_json::json!({
                        "role": msg.role,
                        "content": msg.content,
                    })
                }
            })
            .collect();

        let mut body = serde_json::json!({
            "model": model,
            "messages": api_messages,
            "stream": stream,
        });

        if stream {
            // Include usage in stream for token tracking
            body["stream_options"] = serde_json::json!({"include_usage": true});
        }

        if let Some(tools) = tools {
            if !tools.is_empty() {
                body["tools"] = serde_json::Value::Array(tools.to_vec());
            }
        }

        body
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

    /// Parse token usage from a response or stream final chunk
    fn parse_usage(data: &serde_json::Value) -> Option<TokenUsage> {
        data.get("usage").map(|u| TokenUsage {
            prompt_tokens: u
                .get("prompt_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
            completion_tokens: u
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
            total_tokens: u
                .get("total_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
            cached_tokens: None,
        })
    }
}

/// SSE stream event from OpenAI-compatible API
#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: Option<StreamDelta>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    content: Option<String>,
    tool_calls: Option<Vec<StreamToolCall>>,
}

#[derive(Debug, Deserialize)]
struct StreamToolCall {
    index: Option<u32>,
    id: Option<String>,
    function: Option<StreamFunction>,
}

#[derive(Debug, Deserialize)]
struct StreamFunction {
    name: Option<String>,
    arguments: Option<String>,
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
        let body = self.build_request_body(model, messages, tools, false);

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

        // Parse first choice
        let choice = resp
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"));

        let content = choice
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

        let mut tool_calls: Vec<ToolCall> = Vec::new();
        if let Some(tcs) = choice.and_then(|m| m.get("tool_calls")).and_then(|t| t.as_array()) {
            for tc in tcs {
                if let Some(func) = tc.get("function") {
                    let args_str = func
                        .get("arguments")
                        .and_then(|a| a.as_str())
                        .unwrap_or("{}");
                    let arguments: serde_json::Value =
                        serde_json::from_str(args_str).unwrap_or_default();

                    tool_calls.push(ToolCall {
                        id: tc
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or("")
                            .to_string(),
                        name: func
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string(),
                        arguments,
                    });
                }
            }
        }

        Ok(ChatMessage {
            role: "assistant".to_string(),
            content,
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            tool_call_id: None,
        })
    }

    async fn chat_stream(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: Option<&[serde_json::Value]>,
        tx: mpsc::Sender<StreamChunk>,
    ) -> Result<()> {
        let body = self.build_request_body(model, messages, tools, true);

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
            let _ = tx
                .send(StreamChunk::Error(format!(
                    "xAI API error ({}): {}",
                    status, error_body
                )))
                .await;
            return Ok(());
        }

        // Parse SSE stream using eventsource-stream
        use eventsource_stream::Eventsource;
        let mut stream = response.bytes_stream().eventsource();

        // Track active tool calls by index for proper ID association
        let mut tool_call_ids: std::collections::HashMap<u32, String> =
            std::collections::HashMap::new();

        while let Some(event) = stream.next().await {
            let event = match event {
                Ok(e) => e,
                Err(e) => {
                    let _ = tx
                        .send(StreamChunk::Error(format!("Stream error: {}", e)))
                        .await;
                    break;
                }
            };

            // [DONE] marker signals end of stream
            if event.data.trim() == "[DONE]" {
                let _ = tx.send(StreamChunk::Done).await;
                break;
            }

            let data: serde_json::Value = match serde_json::from_str(&event.data) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // Check for usage in the final chunk (stream_options.include_usage)
            if let Some(_usage) = Self::parse_usage(&data) {
                // Token usage from stream — we'll track this via the server
                // The usage chunk often comes with empty choices
            }

            // Process choices
            if let Some(choices) = data.get("choices").and_then(|c| c.as_array()) {
                for choice in choices {
                    let finish_reason = choice
                        .get("finish_reason")
                        .and_then(|f| f.as_str())
                        .map(|s| s.to_string());

                    if let Some(delta) = choice.get("delta") {
                        // Text content
                        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                            if !content.is_empty() {
                                let _ = tx.send(StreamChunk::Text(content.to_string())).await;
                            }
                        }

                        // Tool calls
                        if let Some(tool_calls) =
                            delta.get("tool_calls").and_then(|t| t.as_array())
                        {
                            for tc in tool_calls {
                                let index = tc
                                    .get("index")
                                    .and_then(|i| i.as_u64())
                                    .unwrap_or(0)
                                    as u32;

                                // New tool call (has id and function.name)
                                if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                                    let name = tc
                                        .get("function")
                                        .and_then(|f| f.get("name"))
                                        .and_then(|n| n.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    tool_call_ids.insert(index, id.to_string());
                                    let _ = tx
                                        .send(StreamChunk::ToolCallStart {
                                            id: id.to_string(),
                                            name,
                                        })
                                        .await;
                                }

                                // Tool call argument delta
                                if let Some(args) = tc
                                    .get("function")
                                    .and_then(|f| f.get("arguments"))
                                    .and_then(|a| a.as_str())
                                {
                                    if !args.is_empty() {
                                        let id = tool_call_ids
                                            .get(&index)
                                            .cloned()
                                            .unwrap_or_else(|| format!("tc_{}", index));
                                        let _ = tx
                                            .send(StreamChunk::ToolCallDelta {
                                                id,
                                                arguments: args.to_string(),
                                            })
                                            .await;
                                    }
                                }
                            }
                        }
                    }

                    // Finish reason "tool_calls" means tool calls are complete
                    if let Some(ref reason) = finish_reason {
                        if reason == "tool_calls" || reason == "stop" {
                            // Send ToolCallEnd for all active tool calls
                            for (_idx, id) in tool_call_ids.drain() {
                                let _ = tx.send(StreamChunk::ToolCallEnd { id }).await;
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
