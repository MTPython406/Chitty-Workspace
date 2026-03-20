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
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
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

        // ── Prompt Caching ──────────────────────────────────────────
        // Add cache_control breakpoints to save users up to 90% on input tokens.
        // Cached content: system prompt (stable), tool definitions (stable),
        // conversation history prefix (grows but prefix is stable across iterations).
        //
        // Breakpoint strategy (max 4 allowed):
        //   1. System prompt — cached for entire conversation
        //   2. Last tool definition — tools rarely change within a session
        //   3. Second-to-last user message — caches conversation history prefix

        if !system_text.is_empty() {
            // System prompt as content block array with cache_control
            body["system"] = serde_json::json!([{
                "type": "text",
                "text": system_text,
                "cache_control": {"type": "ephemeral"}
            }]);
        }

        if let Some(tools) = tools {
            if !tools.is_empty() {
                // Convert from OpenAI-style tool format to Anthropic format
                let mut anthropic_tools: Vec<serde_json::Value> = tools
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

                // Add cache_control to the LAST tool (caches all tools as a block)
                if let Some(last_tool) = anthropic_tools.last_mut() {
                    last_tool["cache_control"] = serde_json::json!({"type": "ephemeral"});
                }

                body["tools"] = serde_json::Value::Array(anthropic_tools);
            }
        }

        // Add cache_control to conversation history — cache the prefix up to the
        // second-to-last user message so subsequent iterations read from cache.
        // Only useful when there are multiple messages (multi-turn or tool iterations).
        if let Some(msgs) = body["messages"].as_array_mut() {
            if msgs.len() >= 4 {
                // Find the second-to-last user/tool_result message and add cache_control
                let mut user_msg_indices: Vec<usize> = Vec::new();
                for (i, m) in msgs.iter().enumerate() {
                    if m.get("role").and_then(|r| r.as_str()) == Some("user") {
                        user_msg_indices.push(i);
                    }
                }
                // Cache up to the second-to-last user message
                if user_msg_indices.len() >= 2 {
                    let cache_idx = user_msg_indices[user_msg_indices.len() - 2];
                    let msg = &mut msgs[cache_idx];
                    // If content is a string, convert to content block array
                    if msg.get("content").map_or(false, |c| c.is_string()) {
                        let text = msg["content"].as_str().unwrap_or("").to_string();
                        msg["content"] = serde_json::json!([{
                            "type": "text",
                            "text": text,
                            "cache_control": {"type": "ephemeral"}
                        }]);
                    } else if let Some(blocks) = msg["content"].as_array_mut() {
                        // Add cache_control to the last content block
                        if let Some(last_block) = blocks.last_mut() {
                            last_block["cache_control"] = serde_json::json!({"type": "ephemeral"});
                        }
                    }
                }
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
                id: "claude-sonnet-4-6".to_string(),
                provider: ProviderId::Anthropic,
                display_name: "Claude Sonnet 4.6".to_string(),
                context_window: Some(1_000_000),
                supports_tools: true,
                supports_streaming: true,
            },
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

        // Track index → real tool call ID (Anthropic streams tool calls by index)
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
                            // Map event index to real tool call ID
                            let index = data.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                            tool_call_ids.insert(index, id.clone());
                            let _ = tx.send(StreamChunk::ToolCallStart { id: id.clone(), name }).await;

                            // Some API versions include full input in content_block_start
                            // Send it as a delta so arguments aren't lost if no input_json_delta follows
                            if let Some(input) = block.get("input") {
                                if input.is_object() && !input.as_object().unwrap().is_empty() {
                                    let _ = tx.send(StreamChunk::ToolCallDelta {
                                        id,
                                        arguments: input.to_string(),
                                    }).await;
                                }
                            }
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
                                    // Look up the real tool call ID from the index
                                    let index = data.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                                    let id = tool_call_ids.get(&index).cloned()
                                        .unwrap_or_else(|| format!("block_{}", index));
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
                    let index = data.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                    // Only send ToolCallEnd if this index was a tool_use block
                    if let Some(id) = tool_call_ids.get(&index) {
                        let _ = tx
                            .send(StreamChunk::ToolCallEnd {
                                id: id.clone(),
                            })
                            .await;
                    }
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
                "message_start" => {
                    // Anthropic sends input token counts in message_start.message.usage
                    // With caching: cache_read_input_tokens, cache_creation_input_tokens, input_tokens
                    if let Some(usage) = data.get("message").and_then(|m| m.get("usage")) {
                        let input = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        let cache_read = usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        let cache_write = usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        // Total input = input (uncached) + cache_read + cache_write
                        let total_input = input + cache_read + cache_write;
                        if total_input > 0 || cache_read > 0 {
                            let _ = tx.send(StreamChunk::TokenUsage {
                                input_tokens: total_input,
                                output_tokens: 0,
                                cache_read_tokens: cache_read,
                                cache_write_tokens: cache_write,
                            }).await;
                        }
                    }
                }
                "message_delta" => {
                    // Anthropic sends output token count in message_delta.usage
                    if let Some(usage) = data.get("usage") {
                        let output = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        if output > 0 {
                            let _ = tx.send(StreamChunk::TokenUsage {
                                input_tokens: 0,
                                output_tokens: output,
                                cache_read_tokens: 0,
                                cache_write_tokens: 0,
                            }).await;
                        }
                    }
                }
                _ => {} // ping — ignore
            }
        }

        Ok(())
    }
}
