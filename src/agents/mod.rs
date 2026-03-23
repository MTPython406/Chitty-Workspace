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
use crate::tools::manifest::PackageManifest;

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
    /// Link to source marketplace package (None = user-created agent)
    /// Package agents have ID format "pkg-{package_name}"
    #[serde(default)]
    pub package_id: Option<String>,
    /// Parent agent ID for sub-agents (None = top-level agent)
    /// Sub-agents are created by packages at configuration time.
    /// Example: Google Cloud package creates "WMS Data" sub-agent scoped to a dataset.
    #[serde(default)]
    pub parent_agent_id: Option<String>,
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
    #[serde(default)]
    pub package_id: Option<String>,
    /// Parent agent ID for sub-agents (None = top-level)
    #[serde(default)]
    pub parent_agent_id: Option<String>,
}

/// Scoped tool configuration for a sub-agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentTool {
    pub id: String,
    pub agent_id: String,
    pub tool_name: String,
    pub display_name: Option<String>,
    pub locked_params: serde_json::Value,
    pub enabled: bool,
}

/// Agents CRUD manager
pub struct AgentsManager;

impl AgentsManager {
    /// Save an agent (insert or update)
    pub fn save(conn: &Connection, agent: &Agent) -> Result<()> {
        let skills_json = serde_json::to_string(&agent.skills)?;
        let tags_json = serde_json::to_string(&agent.tags)?;

        conn.execute(
            "INSERT INTO agents (id, name, description, persona, skills, project_path, preferred_provider, preferred_model, tags, version, ai_generated, max_iterations, temperature, max_tokens, approval_mode, context_budget_pct, compaction_strategy, max_conversation_turns, package_id, parent_agent_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
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
                package_id = excluded.package_id,
                parent_agent_id = excluded.parent_agent_id,
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
                agent.package_id,
                agent.parent_agent_id,
            ],
        )?;
        Ok(())
    }

    /// Load an agent by ID
    pub fn load(conn: &Connection, id: &str) -> Result<Option<Agent>> {
        let mut stmt = conn.prepare(
            "SELECT id, name, description, persona, skills, project_path, preferred_provider, preferred_model, tags, version, ai_generated, max_iterations, temperature, max_tokens, approval_mode, context_budget_pct, compaction_strategy, max_conversation_turns, package_id, parent_agent_id
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
                    package_id: row.get(18)?,
                    parent_agent_id: row.get(19)?,
                })
            })
            .ok();

        Ok(result)
    }

    /// List all agents (summaries)
    pub fn list(conn: &Connection) -> Result<Vec<AgentSummary>> {
        let mut stmt = conn.prepare(
            "SELECT id, name, description, skills, tags, max_iterations, project_path, preferred_provider, preferred_model, approval_mode, package_id, parent_agent_id FROM agents ORDER BY name",
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
                package_id: row.get(10)?,
                parent_agent_id: row.get(11)?,
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

    /// Auto-create (or update) an agent from a marketplace package manifest.
    /// Each package gets one agent with a deterministic ID: "pkg-{package_name}".
    /// Uses ON CONFLICT DO UPDATE so reinstalls update rather than duplicate.
    pub fn create_from_package(conn: &Connection, manifest: &PackageManifest) -> Result<Agent> {
        let agent_id = format!("pkg-{}", manifest.name);
        let ac = &manifest.agent_config;

        // Build persona from agent_config or generate a default
        let persona = ac.default_instructions.clone().unwrap_or_else(|| {
            format!(
                "You are the {} agent. Help the user with {} capabilities. Use your tools to accomplish tasks.",
                manifest.display_name,
                manifest.description.to_lowercase()
            )
        });

        let mut tags = vec!["package".to_string(), "auto-created".to_string()];
        for cat in &manifest.categories {
            tags.push(cat.clone());
        }

        let agent = Agent {
            id: agent_id,
            name: manifest.display_name.clone(),
            description: manifest.description.clone(),
            persona,
            skills: vec![], // Package agents use tools directly, not via skills
            project_path: None,
            preferred_provider: None,
            preferred_model: ac.recommended_model.clone(),
            tags,
            version: manifest.version.clone(),
            ai_generated: false,
            max_iterations: ac.max_iterations.or(Some(10)),
            temperature: ac.temperature,
            max_tokens: None,
            approval_mode: ac.approval_mode.clone().unwrap_or_else(|| "prompt".to_string()),
            context_budget_pct: None,
            compaction_strategy: None,
            max_conversation_turns: None,
            package_id: Some(manifest.name.clone()),
            parent_agent_id: None,
        };

        Self::save(conn, &agent)?;
        Ok(agent)
    }

    /// List child sub-agents for a parent agent
    pub fn list_children(conn: &Connection, parent_id: &str) -> Result<Vec<AgentSummary>> {
        let mut stmt = conn.prepare(
            "SELECT id, name, description, skills, tags, max_iterations, project_path, preferred_provider, preferred_model, approval_mode, package_id, parent_agent_id
             FROM agents WHERE parent_agent_id = ?1 ORDER BY name",
        )?;

        let rows = stmt.query_map(rusqlite::params![parent_id], |row| {
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
                package_id: row.get(10)?,
                parent_agent_id: row.get(11)?,
            })
        })?;

        let mut agents = Vec::new();
        for row in rows {
            agents.push(row?);
        }
        Ok(agents)
    }

    /// Create a sub-agent with scoped tools.
    /// Sub-agents are children of a parent package agent and have locked tool parameters.
    pub fn create_sub_agent(
        conn: &Connection,
        parent_id: &str,
        name: &str,
        description: &str,
        persona: &str,
        scoped_tools: &[SubAgentTool],
        preferred_provider: Option<String>,
        preferred_model: Option<String>,
    ) -> Result<Agent> {
        // Load parent to inherit package_id
        let parent = Self::load(conn, parent_id)?
            .ok_or_else(|| anyhow::anyhow!("Parent agent '{}' not found", parent_id))?;

        // Generate deterministic ID from parent + slugified name
        let slug = name.to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect::<String>();
        let slug = slug.trim_matches('-').to_string();
        let agent_id = format!("{}-{}", parent_id, slug);

        let agent = Agent {
            id: agent_id.clone(),
            name: name.to_string(),
            description: description.to_string(),
            persona: persona.to_string(),
            skills: vec![],
            project_path: parent.project_path.clone(),
            preferred_provider,
            preferred_model,
            tags: vec!["sub-agent".to_string(), "auto-created".to_string()],
            version: "1.0".to_string(),
            ai_generated: false,
            max_iterations: parent.max_iterations.or(Some(10)),
            temperature: parent.temperature,
            max_tokens: parent.max_tokens,
            approval_mode: parent.approval_mode.clone(),
            context_budget_pct: parent.context_budget_pct,
            compaction_strategy: parent.compaction_strategy.clone(),
            max_conversation_turns: parent.max_conversation_turns,
            package_id: parent.package_id.clone(),
            parent_agent_id: Some(parent_id.to_string()),
        };

        Self::save(conn, &agent)?;

        // Insert scoped tool configurations
        for tool in scoped_tools {
            let tool_id = format!("sat-{}-{}", agent_id, tool.tool_name);
            let locked_json = serde_json::to_string(&tool.locked_params)?;
            conn.execute(
                "INSERT INTO sub_agent_tools (id, agent_id, tool_name, display_name, locked_params, enabled)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(agent_id, tool_name) DO UPDATE SET
                    display_name = excluded.display_name,
                    locked_params = excluded.locked_params,
                    enabled = excluded.enabled",
                rusqlite::params![
                    tool_id,
                    agent_id,
                    tool.tool_name,
                    tool.display_name,
                    locked_json,
                    tool.enabled,
                ],
            )?;
        }

        Ok(agent)
    }

    /// Load scoped tool configurations for a sub-agent
    pub fn load_sub_agent_tools(conn: &Connection, agent_id: &str) -> Result<Vec<SubAgentTool>> {
        let mut stmt = conn.prepare(
            "SELECT id, agent_id, tool_name, display_name, locked_params, enabled
             FROM sub_agent_tools WHERE agent_id = ?1 AND enabled = 1",
        )?;

        let rows = stmt.query_map(rusqlite::params![agent_id], |row| {
            let locked_str: String = row.get(4)?;
            Ok(SubAgentTool {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                tool_name: row.get(2)?,
                display_name: row.get(3)?,
                locked_params: serde_json::from_str(&locked_str).unwrap_or_default(),
                enabled: row.get(5)?,
            })
        })?;

        let mut tools = Vec::new();
        for row in rows {
            tools.push(row?);
        }
        Ok(tools)
    }
}
