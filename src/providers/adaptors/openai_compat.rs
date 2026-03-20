//! Shared OpenAI-compatible SSE streaming parser
//!
//! Parses Server-Sent Events from any OpenAI-compatible `/v1/chat/completions`
//! endpoint and emits StreamChunk events via mpsc channel.
//!
//! Used by: XaiProvider, OpenAI (via Xai), OllamaProvider, LocalProvider

use crate::providers::{StreamChunk, ToolCall, ChatMessage};

use anyhow::Result;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;

use super::TokenUsage;

// ─── SSE stream types ────────────────────────────────────

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: Option<StreamDelta>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
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

// ─── Token usage parsing ─────────────────────────────────

/// Parse token usage from an OpenAI-compatible response chunk.
pub fn parse_usage(data: &serde_json::Value) -> Option<TokenUsage> {
    data.get("usage").map(|u| {
        // xAI/OpenAI report cached tokens in prompt_tokens_details.cached_tokens
        let cached = u
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;

        TokenUsage {
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
            cached_tokens: if cached > 0 { Some(cached) } else { None },
        }
    })
}

// ─── Message format builder ──────────────────────────────

/// Build OpenAI-compatible messages array from ChatMessage slice.
pub fn build_messages(messages: &[ChatMessage]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|msg| {
            if msg.role == "tool" {
                serde_json::json!({
                    "role": "tool",
                    "content": msg.content,
                    "tool_call_id": msg.tool_call_id.as_deref().unwrap_or(""),
                })
            } else if msg.role == "assistant" {
                if let Some(ref tool_calls) = msg.tool_calls {
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
                    if msg.content.is_empty() {
                        obj["content"] = serde_json::Value::Null;
                    } else {
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
        .collect()
}

/// Build a complete OpenAI-compatible request body.
pub fn build_request_body(
    model: &str,
    messages: &[ChatMessage],
    tools: Option<&[serde_json::Value]>,
    stream: bool,
    include_stream_usage: bool,
) -> serde_json::Value {
    let api_messages = build_messages(messages);

    let mut body = serde_json::json!({
        "model": model,
        "messages": api_messages,
        "stream": stream,
    });

    if stream && include_stream_usage {
        body["stream_options"] = serde_json::json!({"include_usage": true});
    }

    if let Some(tools) = tools {
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tools.to_vec());
        }
    }

    body
}

// ─── Non-streaming response parser ──────────────────────

/// Parse a non-streaming OpenAI-compatible chat completion response.
pub fn parse_chat_response(resp: &serde_json::Value) -> ChatMessage {
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
    if let Some(tcs) = choice
        .and_then(|m| m.get("tool_calls"))
        .and_then(|t| t.as_array())
    {
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

    ChatMessage {
        role: "assistant".to_string(),
        content,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        tool_call_id: None,
    }
}

// ─── SSE streaming parser ────────────────────────────────

/// Parse an OpenAI-compatible SSE stream and emit StreamChunk events.
///
/// Handles: text deltas, reasoning_content (thinking), tool_calls,
/// token usage, [DONE] marker, and error responses.
pub async fn stream_openai_sse(
    response: reqwest::Response,
    tx: mpsc::Sender<StreamChunk>,
) -> Result<()> {
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
        if let Some(usage) = parse_usage(&data) {
            let cached = usage.cached_tokens.unwrap_or(0);
            let _ = tx
                .send(StreamChunk::TokenUsage {
                    input_tokens: usage.prompt_tokens,
                    output_tokens: usage.completion_tokens,
                    cache_read_tokens: cached,
                    cache_write_tokens: 0,
                })
                .await;
        }

        // Process choices
        if let Some(choices) = data.get("choices").and_then(|c| c.as_array()) {
            for choice in choices {
                let finish_reason = choice
                    .get("finish_reason")
                    .and_then(|f| f.as_str())
                    .map(|s| s.to_string());

                if let Some(delta) = choice.get("delta") {
                    // Reasoning content (thinking) — send as Thinking event
                    if let Some(reasoning) =
                        delta.get("reasoning_content").and_then(|c| c.as_str())
                    {
                        if !reasoning.is_empty() {
                            let _ = tx
                                .send(StreamChunk::Thinking(reasoning.to_string()))
                                .await;
                        }
                    }

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

                // Finish reason "tool_calls" or "stop" — end all active tool calls
                if let Some(ref reason) = finish_reason {
                    if reason == "tool_calls" || reason == "stop" {
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
