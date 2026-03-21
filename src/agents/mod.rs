//! Agents system
//!
//! An Agent = Persona + Skills + Execution Config
//!
//! Agents are simple: the user picks which skills the agent has access to
//! and writes a short persona describing the agent's identity/role.
//! Skills bundle domain expertise + tool requirements together.
//! Tool usage instructions come FROM the tools themselves (agent instructions),
//! so the user never has to describe how to use tools.

use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

fn default_approval_mode() -> String {
    "prompt".to_string()
}

/// An agent definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    /// Unique identifier
    pub id: String,
    /// Display name
    pub name: String,
    /// Short description of what this agent does
    pub description: String,
    /// Agent persona — who the agent IS (short identity/role text)
    /// Stored as "persona" in DB (V8+), reads "instructions" for backward compat
    #[serde(alias = "instructions")]
    pub persona: String,
    /// Skills this agent uses (empty = all available skills)
    /// Stored as "skills" in DB (V8+), reads "tools" for backward compat
    #[serde(alias = "tools")]
    pub skills: Vec<String>,
    /// Optional project directory scope (None = global)
    pub project_path: Option<String>,
    /// Provider/model preference (None = use default)
    pub preferred_provider: Option<String>,
    pub preferred_model: Option<String>,
    /// Tags for organization
    pub tags: Vec<String>,
    /// Version for agent updates
    pub version: String,
    /// Whether this agent was AI-generated via Agent Builder
    pub ai_generated: bool,
    // Execution config (mirrors DataVisions agent node properties)
    /// Max tool call iterations (default 10, coding agents: 25)
    pub max_iterations: Option<u32>,
    /// Temperature override (None = use model default)
    pub temperature: Option<f64>,
    /// Max output tokens override (None = use model default)
    pub max_tokens: Option<u32>,
    /// Approval mode: "prompt" (default) = ask user, "auto" = auto-approve all actions
    #[serde(default = "default_approval_mode")]
    pub approval_mode: String,
    // Context management
    /// Context budget: percentage of model's context window before compacting (default 75, range 25-95)
    #[serde(default)]
    pub context_budget_pct: Option<u32>,
    /// Compaction strategy: "truncate" (fast, default) or "summarize" (uses LLM for summary)
    #[serde(default)]
    pub compaction_strategy: Option<String>,
    /// Max conversation turns before forcing compaction (None = unlimited)
    #[serde(default)]
    pub max_conversation_turns: Option<u32>,
}

/// Summary for listing agents (lightweight)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(alias = "tools")]
    pub skills: Vec<String>,
    pub tags: Vec<String>,
    pub max_iterations: Option<u32>,
    pub project_path: Option<String>,
    pub preferred_provider: Option<String>,
    pub preferred_model: Option<String>,
    #[serde(default = "default_approval_mode")]
    pub approval_mode: String,
}

/// Agents CRUD manager
pub struct AgentsManager;

impl AgentsManager {
    /// Save an agent (insert or update)
    pub fn save(conn: &Connection, agent: &Agent) -> Result<()> {
        let skills_json = serde_json::to_string(&agent.skills)?;
        let tags_json = serde_json::to_string(&agent.tags)?;

        conn.execute(
            "INSERT INTO agents (id, name, description, persona, skills, project_path, preferred_provider, preferred_model, tags, version, ai_generated, max_iterations, temperature, max_tokens, approval_mode, context_budget_pct, compaction_strategy, max_conversation_turns)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                description = excluded.description,
                persona = excluded.persona,
                skills = excluded.skills,
                project_path = excluded.project_path,
                preferred_provider = excluded.preferred_provider,
                preferred_model = excluded.preferred_model,
                tags = excluded.tags,
                version = excluded.version,
                ai_generated = excluded.ai_generated,
                max_iterations = excluded.max_iterations,
                temperature = excluded.temperature,
                max_tokens = excluded.max_tokens,
                approval_mode = excluded.approval_mode,
                context_budget_pct = excluded.context_budget_pct,
                compaction_strategy = excluded.compaction_strategy,
                max_conversation_turns = excluded.max_conversation_turns,
                updated_at = datetime('now')",
            rusqlite::params![
                agent.id,
                agent.name,
                agent.description,
                agent.persona,
                skills_json,
                agent.project_path,
                agent.preferred_provider,
                agent.preferred_model,
                tags_json,
                agent.version,
                agent.ai_generated,
                agent.max_iterations.map(|v| v as i32),
                agent.temperature,
                agent.max_tokens.map(|v| v as i32),
                agent.approval_mode,
                agent.context_budget_pct.map(|v| v as i32),
                agent.compaction_strategy,
                agent.max_conversation_turns.map(|v| v as i32),
            ],
        )?;
        Ok(())
    }

    /// Load an agent by ID
    pub fn load(conn: &Connection, id: &str) -> Result<Option<Agent>> {
        let mut stmt = conn.prepare(
            "SELECT id, name, description, persona, skills, project_path, preferred_provider, preferred_model, tags, version, ai_generated, max_iterations, temperature, max_tokens, approval_mode, context_budget_pct, compaction_strategy, max_conversation_turns
             FROM agents WHERE id = ?1",
        )?;

        let result = stmt
            .query_row(rusqlite::params![id], |row| {
                let skills_str: String = row.get(4)?;
                let tags_str: String = row.get(8)?;
                Ok(Agent {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    persona: row.get(3)?,
                    skills: serde_json::from_str(&skills_str).unwrap_or_default(),
                    project_path: row.get(5)?,
                    preferred_provider: row.get(6)?,
                    preferred_model: row.get(7)?,
                    tags: serde_json::from_str(&tags_str).unwrap_or_default(),
                    version: row.get(9)?,
                    ai_generated: row.get(10)?,
                    max_iterations: row.get::<_, Option<i32>>(11)?.map(|v| v as u32),
                    temperature: row.get(12)?,
                    max_tokens: row.get::<_, Option<i32>>(13)?.map(|v| v as u32),
                    approval_mode: row.get::<_, Option<String>>(14)?.unwrap_or_else(|| "prompt".to_string()),
                    context_budget_pct: row.get::<_, Option<i32>>(15)?.map(|v| v as u32),
                    compaction_strategy: row.get(16)?,
                    max_conversation_turns: row.get::<_, Option<i32>>(17)?.map(|v| v as u32),
                })
            })
            .ok();

        Ok(result)
    }

    /// List all agents (summaries)
    pub fn list(conn: &Connection) -> Result<Vec<AgentSummary>> {
        let mut stmt = conn.prepare(
            "SELECT id, name, description, skills, tags, max_iterations, project_path, preferred_provider, preferred_model, approval_mode FROM agents ORDER BY name",
        )?;

        let rows = stmt.query_map([], |row| {
            let skills_str: String = row.get(3)?;
            let tags_str: String = row.get(4)?;
            Ok(AgentSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                skills: serde_json::from_str(&skills_str).unwrap_or_default(),
                tags: serde_json::from_str(&tags_str).unwrap_or_default(),
                max_iterations: row.get::<_, Option<i32>>(5)?.map(|v| v as u32),
                project_path: row.get(6)?,
                preferred_provider: row.get(7)?,
                preferred_model: row.get(8)?,
                approval_mode: row.get::<_, Option<String>>(9)?.unwrap_or_else(|| "prompt".to_string()),
            })
        })?;

        let mut agents = Vec::new();
        for row in rows {
            agents.push(row?);
        }
        Ok(agents)
    }

    /// Delete an agent by ID
    pub fn delete(conn: &Connection, id: &str) -> Result<bool> {
        let count = conn.execute("DELETE FROM agents WHERE id = ?1", rusqlite::params![id])?;
        Ok(count > 0)
    }
}
