//! BYOK Cloud provider implementations
//!
//! Currently implements: Anthropic (Messages API with streaming + tool use)
//! Future: OpenAI, Google, xAI

use super::{ChatMessage, Model, Provider, ProviderId, StreamChunk, ToolCall};
use anyhow::{Context, Result};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use tokio::sync::mpsc;

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic provider (Claude models via Messages API)
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            base_url: base_url.unwrap_or_else(|| "https://api.anthropic.com".to_string()),
        }
    }

    /// Build the Anthropic API request body from our generic message format.
    fn build_request_body(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: Option<&[serde_json::Value]>,
        stream: bool,
    ) -> serde_json::Value {
        // Extract system message (first message if role is "system")
        let mut system_text = String::new();
        let mut api_messages: Vec<serde_json::Value> = Vec::new();

        for msg in messages {
            if msg.role == "system" {
                system_text.push_str(&msg.content);
                continue;
            }

            if msg.role == "tool" {
                // Tool results in Anthropic format: sent as user message with tool_result content
                let tool_result = serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": msg.tool_call_id.as_deref().unwrap_or(""),
                        "content": msg.content,
                    }]
                });
                api_messages.push(tool_result);
                continue;
            }

            if msg.role == "assistant" {
                if let Some(ref tool_calls) = msg.tool_calls {
                    // Assistant message with tool calls — build content blocks
                    let mut content_blocks: Vec<serde_json::Value> = Vec::new();
                    if !msg.content.is_empty() {
                        content_blocks.push(serde_json::json!({
                            "type": "text",
                            "text": msg.content,
                        }));
                    }
                    for tc in tool_calls {
                        content_blocks.push(serde_json::json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.name,
                            "input": tc.arguments,
                        }));
                    }
                    api_messages.push(serde_json::json!({
                        "role": "assistant",
                        "content": content_blocks,
                    }));
                    continue;
                }
            }

            // Standard user or assistant text message
            api_messages.push(serde_json::json!({
                "role": msg.role,
                "content": msg.content,
            }));
        }

        let mut body = serde_json::json!({
            "model": model,
            "max_tokens": 8192,
            "messages": api_messages,
            "stream": stream,
        });

        if !system_text.is_empty() {
            body["system"] = serde_json::Value::String(system_text);
        }

        if let Some(tools) = tools {
            if !tools.is_empty() {
                // Convert from OpenAI-style tool format to Anthropic format
                let anthropic_tools: Vec<serde_json::Value> = tools
                    .iter()
                    .map(|t| {
                        if let Some(func) = t.get("function") {
                            serde_json::json!({
                                "name": func.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                                "description": func.get("description").and_then(|d| d.as_str()).unwrap_or(""),
                                "input_schema": func.get("parameters").cloned().unwrap_or(serde_json::json!({"type": "object"})),
                            })
                        } else {
                            // Already in Anthropic format
                            t.clone()
                        }
                    })
                    .collect();
                body["tools"] = serde_json::Value::Array(anthropic_tools);
            }
        }

        body
    }
}

#[async_trait::async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Anthropic
    }

    async fn list_models(&self) -> Result<Vec<Model>> {
        Ok(vec![
            Model {
                id: "claude-sonnet-4-20250514".to_string(),
                provider: ProviderId::Anthropic,
                display_name: "Claude Sonnet 4".to_string(),
                context_window: Some(200_000),
                supports_tools: true,
                supports_streaming: true,
            },
            Model {
                id: "claude-haiku-4-5-20251001".to_string(),
                provider: ProviderId::Anthropic,
                display_name: "Claude Haiku 4.5".to_string(),
                context_window: Some(200_000),
                supports_tools: true,
                supports_streaming: true,
            },
            Model {
                id: "claude-opus-4-20250514".to_string(),
                provider: ProviderId::Anthropic,
                display_name: "Claude Opus 4".to_string(),
                context_window: Some(200_000),
                supports_tools: true,
                supports_streaming: true,
            },
        ])
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
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send request to Anthropic")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API error ({}): {}", status, error_body);
        }

        let resp: serde_json::Value = response.json().await?;

        // Parse response content blocks
        let mut text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        if let Some(content) = resp.get("content").and_then(|c| c.as_array()) {
            for block in content {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                            text.push_str(t);
                        }
                    }
                    Some("tool_use") => {
                        tool_calls.push(ToolCall {
                            id: block
                                .get("id")
                                .and_then(|i| i.as_str())
                                .unwrap_or("")
                                .to_string(),
                            name: block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string(),
                            arguments: block.get("input").cloned().unwrap_or_default(),
                        });
                    }
                    _ => {}
                }
            }
        }

        Ok(ChatMessage {
            role: "assistant".to_string(),
            content: text,
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
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send request to Anthropic")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            let _ = tx
                .send(StreamChunk::Error(format!(
                    "Anthropic API error ({}): {}",
                    status, error_body
                )))
                .await;
            return Ok(());
        }

        let mut stream = response.bytes_stream().eventsource();

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

            // Parse SSE event
            let data: serde_json::Value = match serde_json::from_str(&event.data) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let event_type = data
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("");

            match event_type {
                "content_block_start" => {
                    if let Some(block) = data.get("content_block") {
                        if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            let id = block
                                .get("id")
                                .and_then(|i| i.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            let _ = tx.send(StreamChunk::ToolCallStart { id, name }).await;
                        }
                    }
                }
                "content_block_delta" => {
                    if let Some(delta) = data.get("delta") {
                        match delta.get("type").and_then(|t| t.as_str()) {
                            Some("text_delta") => {
                                if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                                    let _ = tx.send(StreamChunk::Text(text.to_string())).await;
                                }
                            }
                            Some("input_json_delta") => {
                                if let Some(json) =
                                    delta.get("partial_json").and_then(|j| j.as_str())
                                {
                                    // Determine the tool call ID from the index
                                    let id = data
                                        .get("index")
                                        .and_then(|i| i.as_u64())
                                        .map(|i| format!("block_{}", i))
                                        .unwrap_or_default();
                                    let _ = tx
                                        .send(StreamChunk::ToolCallDelta {
                                            id,
                                            arguments: json.to_string(),
                                        })
                                        .await;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "content_block_stop" => {
                    // Check if this was a tool use block
                    let index = data.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                    let _ = tx
                        .send(StreamChunk::ToolCallEnd {
                            id: format!("block_{}", index),
                        })
                        .await;
                }
                "message_stop" => {
                    let _ = tx.send(StreamChunk::Done).await;
                    break;
                }
                "error" => {
                    let msg = data
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("Unknown error");
                    let _ = tx.send(StreamChunk::Error(msg.to_string())).await;
                    break;
                }
                _ => {} // message_start, message_delta, ping — ignore
            }
        }

        Ok(())
    }
}
