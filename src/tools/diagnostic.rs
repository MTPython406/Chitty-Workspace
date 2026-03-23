//! Self-diagnostic tool — lets Chitty inspect its own session
//!
//! Reads from the conversation's message history in the DB to surface:
//! - Recent errors and failures
//! - Tool call success/failure rates
//! - Context usage stats
//! - Conversation flow summary

use async_trait::async_trait;

use super::{NativeTool, ToolCategory, ToolContext, ToolDefinition, ToolResult};

pub struct DiagnosticTool;

#[async_trait]
impl NativeTool for DiagnosticTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "check_session".to_string(),
            display_name: "Check Session".to_string(),
            description: "Inspect the current session's activity: recent errors, tool call stats, and conversation health. Use when things aren't working to understand what went wrong.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "focus": {
                        "type": "string",
                        "description": "What to focus on: 'errors' (recent failures), 'tools' (tool call stats), 'summary' (full session overview), 'last_error' (most recent error detail)",
                        "enum": ["errors", "tools", "summary", "last_error"]
                    }
                },
                "required": ["focus"]
            }),
            instructions: Some(
                "Use `check_session` to inspect your own session when things go wrong.\n\
                 - `errors` — See all recent tool failures with details. Use after repeated failures to change strategy.\n\
                 - `tools` — See success/failure rates per tool. Helps identify broken tools.\n\
                 - `summary` — Full session overview: message count, iterations, tool stats.\n\
                 - `last_error` — Get the full content of the most recent error.\n\n\
                 **When to use:** After 2+ consecutive tool failures, call `check_session(focus='errors')` before retrying.\n\
                 This helps you avoid repeating the same failing approach."
                    .to_string(),
            ),
            category: ToolCategory::Native,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let focus = args
            .get("focus")
            .and_then(|v| v.as_str())
            .unwrap_or("summary")
            .to_string();

        let conv_id = ctx.conversation_id.clone();
        let db = ctx.db.clone();

        let result = db
            .with_conn(move |conn| {
                match focus.as_str() {
                    "errors" => get_errors(conn, &conv_id),
                    "tools" => get_tool_stats(conn, &conv_id),
                    "last_error" => get_last_error(conn, &conv_id),
                    _ => get_summary(conn, &conv_id),
                }
            })
            .await;

        match result {
            Ok(data) => ToolResult::ok(data),
            Err(e) => ToolResult::err(format!("Diagnostic failed: {}", e)),
        }
    }
}

fn get_errors(
    conn: &rusqlite::Connection,
    conv_id: &str,
) -> anyhow::Result<serde_json::Value> {
    // Find all tool result messages that contain errors
    let mut stmt = conn.prepare(
        "SELECT content, tool_call_id, created_at FROM messages
         WHERE conversation_id = ?1 AND role = 'tool'
         AND (content LIKE '%Error:%' OR content LIKE '%error%' OR content LIKE '%denied%' OR content LIKE '%FAIL%')
         ORDER BY created_at DESC LIMIT 10",
    )?;

    let errors: Vec<serde_json::Value> = stmt
        .query_map(rusqlite::params![conv_id], |row| {
            let content: String = row.get(0)?;
            let tool_call_id: Option<String> = row.get(1)?;
            let created_at: String = row.get(2)?;
            Ok(serde_json::json!({
                "error": content.chars().take(500).collect::<String>(),
                "tool_call_id": tool_call_id,
                "time": created_at,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    // Also find which tool calls produced these errors
    let mut tool_errors: Vec<serde_json::Value> = Vec::new();
    for err in &errors {
        if let Some(tcid) = err.get("tool_call_id").and_then(|v| v.as_str()) {
            // Find the assistant message with this tool call
            let tool_name: Option<String> = conn
                .query_row(
                    "SELECT tool_calls FROM messages
                     WHERE conversation_id = ?1 AND role = 'assistant' AND tool_calls LIKE ?2
                     ORDER BY created_at DESC LIMIT 1",
                    rusqlite::params![conv_id, format!("%{}%", tcid)],
                    |row| row.get(0),
                )
                .ok();

            if let Some(tc_json) = tool_name {
                if let Ok(tcs) = serde_json::from_str::<Vec<serde_json::Value>>(&tc_json) {
                    for tc in &tcs {
                        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        if id == tcid {
                            let name = tc
                                .get("name")
                                .or_else(|| tc.get("function").and_then(|f| f.get("name")))
                                .and_then(|v| v.as_str())
                                .unwrap_or("?");
                            tool_errors.push(serde_json::json!({
                                "tool": name,
                                "error": err.get("error"),
                                "time": err.get("time"),
                            }));
                        }
                    }
                }
            }
        }
    }

    Ok(serde_json::json!({
        "error_count": errors.len(),
        "errors": tool_errors,
        "advice": if errors.len() >= 3 {
            "Multiple failures detected. Consider changing your approach — different tool, different parameters, or ask the user for help."
        } else if errors.len() >= 1 {
            "Some errors found. Check the details and adjust your approach if needed."
        } else {
            "No errors found in this session."
        }
    }))
}

fn get_tool_stats(
    conn: &rusqlite::Connection,
    conv_id: &str,
) -> anyhow::Result<serde_json::Value> {
    // Count all tool result messages, split by success/failure
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND role = 'tool'",
        rusqlite::params![conv_id],
        |row| row.get(0),
    )?;

    let failures: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND role = 'tool'
         AND (content LIKE '%Error:%' OR content LIKE '%error%' OR content LIKE '%denied%')",
        rusqlite::params![conv_id],
        |row| row.get(0),
    )?;

    let successes = total - failures;
    let failure_rate = if total > 0 {
        (failures as f64 / total as f64 * 100.0) as u32
    } else {
        0
    };

    // Get per-tool breakdown by parsing tool_calls from assistant messages
    let mut tool_counts: std::collections::HashMap<String, (u32, u32)> = std::collections::HashMap::new();

    let mut stmt = conn.prepare(
        "SELECT m.content, a.tool_calls FROM messages m
         JOIN messages a ON m.tool_call_id IS NOT NULL
            AND a.conversation_id = m.conversation_id
            AND a.role = 'assistant'
            AND a.tool_calls LIKE '%' || m.tool_call_id || '%'
         WHERE m.conversation_id = ?1 AND m.role = 'tool'",
    )?;

    let _ = stmt.query_map(rusqlite::params![conv_id], |row| {
        let content: String = row.get(0)?;
        let tc_json: String = row.get(1)?;
        let is_error = content.contains("Error:") || content.contains("denied");

        if let Ok(tcs) = serde_json::from_str::<Vec<serde_json::Value>>(&tc_json) {
            for tc in &tcs {
                let name = tc
                    .get("name")
                    .or_else(|| tc.get("function").and_then(|f| f.get("name")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let entry = tool_counts.entry(name).or_insert((0, 0));
                if is_error {
                    entry.1 += 1;
                } else {
                    entry.0 += 1;
                }
            }
        }
        Ok(())
    })?.for_each(|_| {});

    let per_tool: Vec<serde_json::Value> = tool_counts
        .iter()
        .map(|(name, (ok, fail))| {
            serde_json::json!({
                "tool": name,
                "success": ok,
                "failures": fail,
                "total": ok + fail,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "total_tool_calls": total,
        "successes": successes,
        "failures": failures,
        "failure_rate_pct": failure_rate,
        "per_tool": per_tool,
        "health": if failure_rate > 50 { "critical" } else if failure_rate > 25 { "degraded" } else { "healthy" }
    }))
}

fn get_last_error(
    conn: &rusqlite::Connection,
    conv_id: &str,
) -> anyhow::Result<serde_json::Value> {
    let result = conn.query_row(
        "SELECT content, created_at FROM messages
         WHERE conversation_id = ?1 AND role = 'tool'
         AND (content LIKE '%Error:%' OR content LIKE '%error%' OR content LIKE '%denied%')
         ORDER BY created_at DESC LIMIT 1",
        rusqlite::params![conv_id],
        |row| {
            let content: String = row.get(0)?;
            let created_at: String = row.get(1)?;
            Ok((content, created_at))
        },
    );

    match result {
        Ok((content, time)) => Ok(serde_json::json!({
            "last_error": content,
            "time": time,
        })),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(serde_json::json!({
            "last_error": null,
            "message": "No errors in this session"
        })),
        Err(e) => Err(anyhow::anyhow!("DB error: {}", e)),
    }
}

fn get_summary(
    conn: &rusqlite::Connection,
    conv_id: &str,
) -> anyhow::Result<serde_json::Value> {
    let msg_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1",
        rusqlite::params![conv_id],
        |row| row.get(0),
    )?;

    let user_msgs: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND role = 'user'",
        rusqlite::params![conv_id],
        |row| row.get(0),
    )?;

    let assistant_msgs: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND role = 'assistant'",
        rusqlite::params![conv_id],
        |row| row.get(0),
    )?;

    let tool_results: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND role = 'tool'",
        rusqlite::params![conv_id],
        |row| row.get(0),
    )?;

    let errors: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND role = 'tool'
         AND (content LIKE '%Error:%' OR content LIKE '%error%' OR content LIKE '%denied%')",
        rusqlite::params![conv_id],
        |row| row.get(0),
    )?;

    // Iterations = number of assistant messages with tool_calls
    let iterations: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND role = 'assistant' AND tool_calls IS NOT NULL",
        rusqlite::params![conv_id],
        |row| row.get(0),
    )?;

    Ok(serde_json::json!({
        "messages": msg_count,
        "user_messages": user_msgs,
        "assistant_messages": assistant_msgs,
        "tool_calls": tool_results,
        "errors": errors,
        "iterations": iterations,
        "health": if errors > tool_results / 2 { "critical" } else if errors > 3 { "degraded" } else { "healthy" }
    }))
}
