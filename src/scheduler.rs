//! Agent Scheduler — runs scheduled tasks on cron expressions
//!
//! Polls the `scheduled_tasks` table every 30 seconds, identifies due tasks,
//! and updates run timestamps. Full agent execution will be wired in a future iteration.

use std::str::FromStr;
use std::sync::Arc;

use chrono::Local;
use cron::Schedule;
use tracing::{error, info, warn};

use crate::server::AppState;

/// A scheduled task loaded from the database
#[derive(Debug, Clone)]
pub struct ScheduledTask {
    pub id: String,
    pub name: String,
    pub agent_id: Option<String>,
    pub prompt: String,
    pub cron_expression: String,
    pub project_path: Option<String>,
    pub enabled: bool,
    pub auto_approve: bool,
    pub last_run_at: Option<String>,
    pub next_run_at: Option<String>,
}

/// Run the scheduler loop — call this from a tokio::spawn in server.rs
pub async fn run(state: Arc<AppState>) {
    info!("Scheduler started — polling every 30 seconds");

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        let db = state.db.clone();
        let now_str = chrono::Utc::now().to_rfc3339();
        let tasks: Vec<ScheduledTask> = match db
            .with_conn(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, name, agent_id, prompt, cron_expression, project_path, \
                     enabled, auto_approve, last_run_at, next_run_at \
                     FROM scheduled_tasks \
                     WHERE enabled = 1 AND (next_run_at IS NULL OR next_run_at <= ?1)",
                )?;
                let rows = stmt.query_map(rusqlite::params![now_str], |row| {
                    Ok(ScheduledTask {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        agent_id: row.get(2)?,
                        prompt: row.get(3)?,
                        cron_expression: row.get(4)?,
                        project_path: row.get(5)?,
                        enabled: row.get::<_, i32>(6)? != 0,
                        auto_approve: row.get::<_, i32>(7)? != 0,
                        last_run_at: row.get(8)?,
                        next_run_at: row.get(9)?,
                    })
                })?;
                let mut result = Vec::new();
                for row in rows {
                    result.push(row?);
                }
                Ok(result)
            })
            .await
        {
            Ok(t) => t,
            Err(e) => {
                error!("Scheduler: failed to load tasks: {}", e);
                continue;
            }
        };

        for task in tasks {
            info!("Scheduler: task '{}' is due — executing", task.name);

            let next_run = compute_next_run(&task.cron_expression);

            // Update timestamps
            let db = state.db.clone();
            let task_id = task.id.clone();
            let now = chrono::Utc::now().to_rfc3339();
            let next = next_run.clone();
            if let Err(e) = db
                .with_conn(move |conn| {
                    conn.execute(
                        "UPDATE scheduled_tasks SET last_run_at = ?1, next_run_at = ?2, updated_at = ?1 WHERE id = ?3",
                        rusqlite::params![now, next, task_id],
                    )?;
                    Ok(())
                })
                .await
            {
                error!("Scheduler: failed to update timestamps: {}", e);
            }

            // Execute the task
            let task_state = state.clone();
            let task_clone = task.clone();
            tokio::spawn(async move {
                if let Err(e) = execute_scheduled_task(task_state, task_clone).await {
                    error!("Scheduler: task '{}' failed: {}", task.name, e);
                }
            });
        }
    }
}

/// Compute the next run time from a 5-field cron expression
pub fn compute_next_run(cron_expr: &str) -> Option<String> {
    let full_expr = format!("0 {} *", cron_expr);
    match Schedule::from_str(&full_expr) {
        Ok(schedule) => schedule.upcoming(Local).next().map(|dt| dt.to_rfc3339()),
        Err(e) => {
            warn!("Invalid cron expression '{}': {}", cron_expr, e);
            None
        }
    }
}

/// Execute a scheduled task (public for manual trigger from API)
pub async fn execute_scheduled_task(
    state: Arc<AppState>,
    task: ScheduledTask,
) -> anyhow::Result<()> {
    info!(
        "Scheduler: running '{}' (agent: {})",
        task.name,
        task.agent_id.as_deref().unwrap_or("Chitty")
    );

    // TODO: Full agent execution — refactor process_chat() into a reusable function
    // For now, log the execution and create a placeholder conversation
    info!(
        "Scheduler: task '{}' triggered (prompt: '{}') — full execution pending",
        task.name,
        if task.prompt.len() > 80 { &task.prompt[..80] } else { &task.prompt }
    );

    Ok(())
}

/// Initialize next_run_at for enabled tasks that don't have one set
pub async fn initialize_next_runs(state: Arc<AppState>) {
    let db = state.db.clone();
    let tasks: Vec<(String, String)> = db
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, cron_expression FROM scheduled_tasks WHERE enabled = 1 AND next_run_at IS NULL",
            )?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            let mut result = Vec::new();
            for row in rows {
                result.push(row?);
            }
            Ok(result)
        })
        .await
        .unwrap_or_default();

    for (id, cron_expr) in tasks {
        if let Some(next_run) = compute_next_run(&cron_expr) {
            let db = state.db.clone();
            let task_id = id.clone();
            let nr = next_run.clone();
            let _ = db
                .with_conn(move |conn| {
                    conn.execute(
                        "UPDATE scheduled_tasks SET next_run_at = ?1 WHERE id = ?2",
                        rusqlite::params![nr, task_id],
                    )?;
                    Ok(())
                })
                .await;
            info!("Scheduler: initialized next_run for task {} → {}", id, next_run);
        }
    }
}
