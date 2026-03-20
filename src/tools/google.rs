//! Real Google API tools — Gmail, Calendar, Drive, Contacts
//!
//! These tools use OAuth tokens (stored in OS keyring) to make real API calls.
//! If not connected, they return a clear error directing user to Settings → Integrations.

use super::{NativeTool, ToolCategory, ToolContext, ToolDefinition, ToolResult};
use async_trait::async_trait;

// ---------------------------------------------------------------------------
// Gmail Read Tool
// ---------------------------------------------------------------------------

pub struct GmailReadTool;

#[async_trait]
impl NativeTool for GmailReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "gmail_read".to_string(),
            display_name: "Gmail Read".to_string(),
            description: "Read and search emails from the user's Gmail account. \
                Can list recent emails, search by query, or read a specific email by ID.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "search", "read"],
                        "description": "list = recent inbox, search = search by query, read = read specific email"
                    },
                    "query": {
                        "type": "string",
                        "description": "Gmail search query (for 'search' action). Examples: 'from:boss@company.com', 'is:unread', 'subject:invoice'"
                    },
                    "message_id": {
                        "type": "string",
                        "description": "Email message ID (for 'read' action)"
                    },
                    "max_results": {
                        "type": "number",
                        "description": "Maximum emails to return (default: 10, max: 50)"
                    }
                },
                "required": ["action"]
            }),
            instructions: Some(
                "Read emails from the user's Gmail account via the Gmail API.\n\
                 Requires Google integration (Settings → Integrations → Connect Google).\n\
                 \n\
                 Actions:\n\
                 - `list` — Get recent inbox emails (default 10)\n\
                 - `search` — Search with Gmail query syntax (from:, subject:, is:unread, etc.)\n\
                 - `read` — Get full email content by message_id\n\
                 \n\
                 The response includes: subject, from, date, snippet, and labels."
                    .to_string(),
            ),
            category: ToolCategory::Integration,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let token = match crate::oauth::get_access_token("google").await {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!(
                "Google not connected: {}. Go to Settings → Integrations → Connect Google.", e
            )),
        };

        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("list");
        let client = reqwest::Client::new();

        match action {
            "list" | "search" => {
                let max = args.get("max_results").and_then(|v| v.as_u64()).unwrap_or(10).min(50);
                let query = if action == "search" {
                    args.get("query").and_then(|v| v.as_str()).unwrap_or("in:inbox")
                } else {
                    "in:inbox"
                };

                let url = format!(
                    "https://gmail.googleapis.com/gmail/v1/users/me/messages?q={}&maxResults={}",
                    urlencoding::encode(query), max
                );

                let resp = client.get(&url)
                    .bearer_auth(&token)
                    .send().await;

                match resp {
                    Ok(r) if r.status().is_success() => {
                        let body: serde_json::Value = r.json().await.unwrap_or_default();
                        let messages = body["messages"].as_array();

                        if let Some(msgs) = messages {
                            // Fetch metadata for each message
                            let mut results = Vec::new();
                            for msg in msgs.iter().take(max as usize) {
                                let mid = msg["id"].as_str().unwrap_or("");
                                let detail_url = format!(
                                    "https://gmail.googleapis.com/gmail/v1/users/me/messages/{}?format=metadata&metadataHeaders=Subject&metadataHeaders=From&metadataHeaders=Date",
                                    mid
                                );
                                if let Ok(dr) = client.get(&detail_url).bearer_auth(&token).send().await {
                                    if let Ok(detail) = dr.json::<serde_json::Value>().await {
                                        let headers = detail["payload"]["headers"].as_array();
                                        let mut subject = String::new();
                                        let mut from = String::new();
                                        let mut date = String::new();
                                        if let Some(hdrs) = headers {
                                            for h in hdrs {
                                                match h["name"].as_str().unwrap_or("") {
                                                    "Subject" => subject = h["value"].as_str().unwrap_or("").to_string(),
                                                    "From" => from = h["value"].as_str().unwrap_or("").to_string(),
                                                    "Date" => date = h["value"].as_str().unwrap_or("").to_string(),
                                                    _ => {}
                                                }
                                            }
                                        }
                                        results.push(serde_json::json!({
                                            "id": mid,
                                            "subject": subject,
                                            "from": from,
                                            "date": date,
                                            "snippet": detail["snippet"].as_str().unwrap_or(""),
                                            "labels": detail["labelIds"],
                                        }));
                                    }
                                }
                            }
                            ToolResult::ok(serde_json::to_string_pretty(&serde_json::json!({
                                "emails": results,
                                "count": results.len(),
                                "query": query,
                            })).unwrap_or_default())
                        } else {
                            ToolResult::ok("No emails found matching the query.")
                        }
                    }
                    Ok(r) => {
                        let status = r.status();
                        let body = r.text().await.unwrap_or_default();
                        ToolResult::err(format!("Gmail API error ({}): {}", status, body))
                    }
                    Err(e) => ToolResult::err(format!("Gmail request failed: {}", e)),
                }
            }
            "read" => {
                let mid = match args.get("message_id").and_then(|v| v.as_str()) {
                    Some(id) => id,
                    None => return ToolResult::err("Missing message_id for 'read' action"),
                };

                let url = format!(
                    "https://gmail.googleapis.com/gmail/v1/users/me/messages/{}?format=full",
                    mid
                );

                match client.get(&url).bearer_auth(&token).send().await {
                    Ok(r) if r.status().is_success() => {
                        let body: serde_json::Value = r.json().await.unwrap_or_default();
                        // Extract text body from parts
                        let snippet = body["snippet"].as_str().unwrap_or("");
                        let headers = body["payload"]["headers"].as_array();
                        let mut subject = String::new();
                        let mut from = String::new();
                        let mut to = String::new();
                        let mut date = String::new();
                        if let Some(hdrs) = headers {
                            for h in hdrs {
                                match h["name"].as_str().unwrap_or("") {
                                    "Subject" => subject = h["value"].as_str().unwrap_or("").to_string(),
                                    "From" => from = h["value"].as_str().unwrap_or("").to_string(),
                                    "To" => to = h["value"].as_str().unwrap_or("").to_string(),
                                    "Date" => date = h["value"].as_str().unwrap_or("").to_string(),
                                    _ => {}
                                }
                            }
                        }
                        // Try to get plain text body
                        let text_body = extract_text_body(&body["payload"]);

                        ToolResult::ok(serde_json::to_string_pretty(&serde_json::json!({
                            "id": mid,
                            "subject": subject,
                            "from": from,
                            "to": to,
                            "date": date,
                            "snippet": snippet,
                            "body": text_body,
                        })).unwrap_or_default())
                    }
                    Ok(r) => ToolResult::err(format!("Gmail API error: {}", r.status())),
                    Err(e) => ToolResult::err(format!("Gmail request failed: {}", e)),
                }
            }
            _ => ToolResult::err(format!("Unknown action: {}. Use list, search, or read.", action)),
        }
    }
}

/// Extract plain text body from Gmail message payload
fn extract_text_body(payload: &serde_json::Value) -> String {
    // Check if this part has text/plain
    if let Some(mime) = payload["mimeType"].as_str() {
        if mime == "text/plain" {
            if let Some(data) = payload["body"]["data"].as_str() {
                if let Ok(decoded) = base64::Engine::decode(
                    &base64::engine::general_purpose::URL_SAFE_NO_PAD, data
                ) {
                    return String::from_utf8_lossy(&decoded).to_string();
                }
            }
        }
    }
    // Check parts recursively
    if let Some(parts) = payload["parts"].as_array() {
        for part in parts {
            let text = extract_text_body(part);
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Gmail Send Tool
// ---------------------------------------------------------------------------

pub struct GmailSendTool;

#[async_trait]
impl NativeTool for GmailSendTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "gmail_send".to_string(),
            display_name: "Gmail Send".to_string(),
            description: "Send an email from the user's Gmail account.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "to": { "type": "string", "description": "Recipient email address" },
                    "subject": { "type": "string", "description": "Email subject line" },
                    "body": { "type": "string", "description": "Email body (plain text)" },
                    "reply_to_id": { "type": "string", "description": "Message ID to reply to (optional)" }
                },
                "required": ["to", "subject", "body"]
            }),
            instructions: Some(
                "Send an email from the user's Gmail. Requires Google integration.\n\
                 IMPORTANT: Always confirm with the user before sending. Show them the to/subject/body first."
                    .to_string(),
            ),
            category: ToolCategory::Integration,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let token = match crate::oauth::get_access_token("google").await {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!(
                "Google not connected: {}. Go to Settings → Integrations → Connect Google.", e
            )),
        };

        let to = match args.get("to").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => return ToolResult::err("Missing 'to' email address"),
        };
        let subject = args.get("subject").and_then(|v| v.as_str()).unwrap_or("(no subject)");
        let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");

        // Build RFC 2822 email
        let raw_email = format!(
            "To: {}\r\nSubject: {}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{}",
            to, subject, body
        );
        let encoded = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            raw_email.as_bytes(),
        );

        let client = reqwest::Client::new();
        let resp = client
            .post("https://gmail.googleapis.com/gmail/v1/users/me/messages/send")
            .bearer_auth(&token)
            .json(&serde_json::json!({ "raw": encoded }))
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                ToolResult::ok(format!("Email sent successfully! Message ID: {}", data["id"].as_str().unwrap_or("unknown")))
            }
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                ToolResult::err(format!("Gmail send failed ({}): {}", status, body))
            }
            Err(e) => ToolResult::err(format!("Gmail send request failed: {}", e)),
        }
    }
}

// ---------------------------------------------------------------------------
// Calendar List Tool
// ---------------------------------------------------------------------------

pub struct CalendarListTool;

#[async_trait]
impl NativeTool for CalendarListTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "calendar_list".to_string(),
            display_name: "Google Calendar".to_string(),
            description: "List upcoming events from the user's Google Calendar.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "max_results": { "type": "number", "description": "Max events to return (default: 10)" },
                    "days_ahead": { "type": "number", "description": "How many days ahead to look (default: 7)" }
                }
            }),
            instructions: Some(
                "List upcoming calendar events. Requires Google integration.\n\
                 Returns event title, start/end time, location, and attendees."
                    .to_string(),
            ),
            category: ToolCategory::Integration,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let token = match crate::oauth::get_access_token("google").await {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!(
                "Google not connected: {}. Go to Settings → Integrations → Connect Google.", e
            )),
        };

        let max = args.get("max_results").and_then(|v| v.as_u64()).unwrap_or(10).min(50);
        let days = args.get("days_ahead").and_then(|v| v.as_u64()).unwrap_or(7);
        let now = chrono::Utc::now();
        let time_min = now.to_rfc3339();
        let time_max = (now + chrono::Duration::days(days as i64)).to_rfc3339();

        let url = format!(
            "https://www.googleapis.com/calendar/v3/calendars/primary/events\
             ?timeMin={}&timeMax={}&maxResults={}&singleEvents=true&orderBy=startTime",
            urlencoding::encode(&time_min),
            urlencoding::encode(&time_max),
            max
        );

        let client = reqwest::Client::new();
        match client.get(&url).bearer_auth(&token).send().await {
            Ok(r) if r.status().is_success() => {
                let body: serde_json::Value = r.json().await.unwrap_or_default();
                let events = body["items"].as_array();

                if let Some(evts) = events {
                    let results: Vec<serde_json::Value> = evts.iter().map(|e| {
                        serde_json::json!({
                            "title": e["summary"].as_str().unwrap_or("(no title)"),
                            "start": e["start"]["dateTime"].as_str().or(e["start"]["date"].as_str()).unwrap_or(""),
                            "end": e["end"]["dateTime"].as_str().or(e["end"]["date"].as_str()).unwrap_or(""),
                            "location": e["location"].as_str().unwrap_or(""),
                            "description": e["description"].as_str().unwrap_or(""),
                            "attendees": e["attendees"].as_array().map(|a| a.iter().map(|att| att["email"].as_str().unwrap_or("")).collect::<Vec<_>>()).unwrap_or_default(),
                        })
                    }).collect();

                    ToolResult::ok(serde_json::to_string_pretty(&serde_json::json!({
                        "events": results,
                        "count": results.len(),
                        "period": format!("next {} days", days),
                    })).unwrap_or_default())
                } else {
                    ToolResult::ok("No upcoming events found.")
                }
            }
            Ok(r) => ToolResult::err(format!("Calendar API error: {}", r.status())),
            Err(e) => ToolResult::err(format!("Calendar request failed: {}", e)),
        }
    }
}

// ---------------------------------------------------------------------------
// Drive Search Tool
// ---------------------------------------------------------------------------

pub struct DriveSearchTool;

#[async_trait]
impl NativeTool for DriveSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "drive_search".to_string(),
            display_name: "Google Drive Search".to_string(),
            description: "Search files in the user's Google Drive by name or content.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query (file name or content)" },
                    "max_results": { "type": "number", "description": "Max files to return (default: 10)" }
                },
                "required": ["query"]
            }),
            instructions: Some(
                "Search Google Drive files. Requires Google integration.\n\
                 Returns file name, type, last modified date, and sharing status."
                    .to_string(),
            ),
            category: ToolCategory::Integration,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let token = match crate::oauth::get_access_token("google").await {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!(
                "Google not connected: {}. Go to Settings → Integrations → Connect Google.", e
            )),
        };

        let query = match args.get("query").and_then(|v| v.as_str()) {
            Some(q) => q,
            None => return ToolResult::err("Missing 'query' parameter"),
        };
        let max = args.get("max_results").and_then(|v| v.as_u64()).unwrap_or(10).min(50);

        let drive_query = format!("name contains '{}' or fullText contains '{}'", query, query);
        let url = format!(
            "https://www.googleapis.com/drive/v3/files?q={}&pageSize={}&fields=files(id,name,mimeType,modifiedTime,size,webViewLink)",
            urlencoding::encode(&drive_query), max
        );

        let client = reqwest::Client::new();
        match client.get(&url).bearer_auth(&token).send().await {
            Ok(r) if r.status().is_success() => {
                let body: serde_json::Value = r.json().await.unwrap_or_default();
                ToolResult::ok(serde_json::to_string_pretty(&body).unwrap_or_default())
            }
            Ok(r) => ToolResult::err(format!("Drive API error: {}", r.status())),
            Err(e) => ToolResult::err(format!("Drive request failed: {}", e)),
        }
    }
}
