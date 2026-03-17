//! Skills system
//!
//! A Skill = System Prompt + Tool Selection + Execution Config
//!
//! Skills are simple: the user picks which tools the agent has access to
//! and writes a system prompt describing the agent's role/task.
//! Tool usage instructions come FROM the tools themselves (agent instructions),
//! so the user never has to describe how to use tools.

use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// A skill definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// Unique identifier
    pub id: String,
    /// Display name
    pub name: String,
    /// Short description of what this skill does
    pub description: String,
    /// System prompt / instructions for the agent (persona/task only — NOT tool usage docs)
    pub instructions: String,
    /// Tool names this skill uses (empty = all tools)
    pub tools: Vec<String>,
    /// Optional project directory scope (None = global)
    pub project_path: Option<String>,
    /// Provider/model preference (None = use default)
    pub preferred_provider: Option<String>,
    pub preferred_model: Option<String>,
    /// Tags for organization
    pub tags: Vec<String>,
    /// Version for skill updates
    pub version: String,
    /// Whether this skill was AI-generated via Skills Builder
    pub ai_generated: bool,
    // Execution config (mirrors DataVisions agent node properties)
    /// Max tool call iterations (default 10, coding skills: 25)
    pub max_iterations: Option<u32>,
    /// Temperature override (None = use model default)
    pub temperature: Option<f64>,
    /// Max output tokens override (None = use model default)
    pub max_tokens: Option<u32>,
}

/// Summary for listing skills (lightweight)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tools: Vec<String>,
    pub tags: Vec<String>,
    pub max_iterations: Option<u32>,
}

/// Skills CRUD manager
pub struct SkillsManager;

impl SkillsManager {
    /// Save a skill (insert or update)
    pub fn save(conn: &Connection, skill: &Skill) -> Result<()> {
        let tools_json = serde_json::to_string(&skill.tools)?;
        let tags_json = serde_json::to_string(&skill.tags)?;

        conn.execute(
            "INSERT INTO skills (id, name, description, instructions, tools, project_path, preferred_provider, preferred_model, tags, version, ai_generated, max_iterations, temperature, max_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                description = excluded.description,
                instructions = excluded.instructions,
                tools = excluded.tools,
                project_path = excluded.project_path,
                preferred_provider = excluded.preferred_provider,
                preferred_model = excluded.preferred_model,
                tags = excluded.tags,
                version = excluded.version,
                ai_generated = excluded.ai_generated,
                max_iterations = excluded.max_iterations,
                temperature = excluded.temperature,
                max_tokens = excluded.max_tokens,
                updated_at = datetime('now')",
            rusqlite::params![
                skill.id,
                skill.name,
                skill.description,
                skill.instructions,
                tools_json,
                skill.project_path,
                skill.preferred_provider,
                skill.preferred_model,
                tags_json,
                skill.version,
                skill.ai_generated,
                skill.max_iterations.map(|v| v as i32),
                skill.temperature,
                skill.max_tokens.map(|v| v as i32),
            ],
        )?;
        Ok(())
    }

    /// Load a skill by ID
    pub fn load(conn: &Connection, id: &str) -> Result<Option<Skill>> {
        let mut stmt = conn.prepare(
            "SELECT id, name, description, instructions, tools, project_path, preferred_provider, preferred_model, tags, version, ai_generated, max_iterations, temperature, max_tokens
             FROM skills WHERE id = ?1",
        )?;

        let result = stmt
            .query_row(rusqlite::params![id], |row| {
                let tools_str: String = row.get(4)?;
                let tags_str: String = row.get(8)?;
                Ok(Skill {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    instructions: row.get(3)?,
                    tools: serde_json::from_str(&tools_str).unwrap_or_default(),
                    project_path: row.get(5)?,
                    preferred_provider: row.get(6)?,
                    preferred_model: row.get(7)?,
                    tags: serde_json::from_str(&tags_str).unwrap_or_default(),
                    version: row.get(9)?,
                    ai_generated: row.get(10)?,
                    max_iterations: row.get::<_, Option<i32>>(11)?.map(|v| v as u32),
                    temperature: row.get(12)?,
                    max_tokens: row.get::<_, Option<i32>>(13)?.map(|v| v as u32),
                })
            })
            .ok();

        Ok(result)
    }

    /// List all skills (summaries)
    pub fn list(conn: &Connection) -> Result<Vec<SkillSummary>> {
        let mut stmt = conn.prepare(
            "SELECT id, name, description, tools, tags, max_iterations FROM skills ORDER BY name",
        )?;

        let rows = stmt.query_map([], |row| {
            let tools_str: String = row.get(3)?;
            let tags_str: String = row.get(4)?;
            Ok(SkillSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                tools: serde_json::from_str(&tools_str).unwrap_or_default(),
                tags: serde_json::from_str(&tags_str).unwrap_or_default(),
                max_iterations: row.get::<_, Option<i32>>(5)?.map(|v| v as u32),
            })
        })?;

        let mut skills = Vec::new();
        for row in rows {
            skills.push(row?);
        }
        Ok(skills)
    }

    /// Delete a skill by ID
    pub fn delete(conn: &Connection, id: &str) -> Result<bool> {
        let count = conn.execute("DELETE FROM skills WHERE id = ?1", rusqlite::params![id])?;
        Ok(count > 0)
    }
}
