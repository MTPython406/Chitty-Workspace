//! Memory system
//!
//! Persistent knowledge the agent retains across sessions.
//! Memories are scoped (global, per-project, per-agent) and typed
//! (user, feedback, project, reference).
//!
//! The agent can save, recall, update, and delete memories.
//! Relevant memories are loaded at conversation start based on
//! the active project and agent.

use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// Memory types — mirrors Claude Code's memory categories
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    /// User preferences, role, expertise
    User,
    /// Corrections and guidance from the user
    Feedback,
    /// Project-specific context (goals, decisions, deadlines)
    Project,
    /// Pointers to external resources
    Reference,
}

impl std::fmt::Display for MemoryType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::User => write!(f, "user"),
            Self::Feedback => write!(f, "feedback"),
            Self::Project => write!(f, "project"),
            Self::Reference => write!(f, "reference"),
        }
    }
}

impl std::str::FromStr for MemoryType {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "user" => Ok(Self::User),
            "feedback" => Ok(Self::Feedback),
            "project" => Ok(Self::Project),
            "reference" => Ok(Self::Reference),
            _ => anyhow::bail!("Unknown memory type: {}", s),
        }
    }
}

/// Memory scope — where this memory applies
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MemoryScope {
    /// Applies everywhere
    Global,
    /// Applies to a specific project directory
    Project,
    /// Applies when a specific agent is active
    Agent,
}

impl std::fmt::Display for MemoryScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Global => write!(f, "global"),
            Self::Project => write!(f, "project"),
            Self::Agent => write!(f, "agent"),
        }
    }
}

/// A persistent memory entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub memory_type: MemoryType,
    pub name: String,
    pub description: String,
    pub content: String,
    pub scope: MemoryScope,
    pub scope_ref: Option<String>,
    pub tags: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Memory manager — CRUD operations on the memories table
pub struct MemoryManager;

impl MemoryManager {
    /// Save a new memory
    pub fn save(conn: &Connection, memory: &Memory) -> Result<()> {
        let tags_json = serde_json::to_string(&memory.tags)?;
        conn.execute(
            "INSERT INTO memories (id, memory_type, name, description, content, scope, scope_ref, tags)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                description = excluded.description,
                content = excluded.content,
                tags = excluded.tags,
                updated_at = datetime('now')",
            rusqlite::params![
                memory.id,
                memory.memory_type.to_string(),
                memory.name,
                memory.description,
                memory.content,
                memory.scope.to_string(),
                memory.scope_ref,
                tags_json,
            ],
        )?;
        Ok(())
    }

    /// Load all memories relevant to the current context
    /// Returns: global memories + project-scoped + agent-scoped
    pub fn load_relevant(
        conn: &Connection,
        project_path: Option<&str>,
        agent_id: Option<&str>,
    ) -> Result<Vec<Memory>> {
        let mut sql = String::from(
            "SELECT id, memory_type, name, description, content, scope, scope_ref, tags, created_at, updated_at
             FROM memories WHERE scope = 'global'",
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(project) = project_path {
            sql.push_str(" UNION ALL SELECT id, memory_type, name, description, content, scope, scope_ref, tags, created_at, updated_at FROM memories WHERE scope = 'project' AND scope_ref = ?");
            params.push(Box::new(project.to_string()));
        }

        if let Some(agent) = agent_id {
            sql.push_str(" UNION ALL SELECT id, memory_type, name, description, content, scope, scope_ref, tags, created_at, updated_at FROM memories WHERE scope = 'agent' AND scope_ref = ?");
            params.push(Box::new(agent.to_string()));
        }

        sql.push_str(" ORDER BY updated_at DESC");

        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            let tags_str: String = row.get(7)?;
            Ok(Memory {
                id: row.get(0)?,
                memory_type: row.get::<_, String>(1)?
                    .parse()
                    .unwrap_or(MemoryType::User),
                name: row.get(2)?,
                description: row.get(3)?,
                content: row.get(4)?,
                scope: match row.get::<_, String>(5)?.as_str() {
                    "project" => MemoryScope::Project,
                    "agent" => MemoryScope::Agent,
                    _ => MemoryScope::Global,
                },
                scope_ref: row.get(6)?,
                tags: serde_json::from_str(&tags_str).unwrap_or_default(),
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?;

        let mut memories = Vec::new();
        for row in rows {
            memories.push(row?);
        }
        Ok(memories)
    }

    /// Delete a memory by ID
    pub fn delete(conn: &Connection, id: &str) -> Result<bool> {
        let count = conn.execute("DELETE FROM memories WHERE id = ?", [id])?;
        Ok(count > 0)
    }

    /// Search memories by content (simple LIKE search)
    pub fn search(conn: &Connection, query: &str) -> Result<Vec<Memory>> {
        let pattern = format!("%{}%", query);
        let mut stmt = conn.prepare(
            "SELECT id, memory_type, name, description, content, scope, scope_ref, tags, created_at, updated_at
             FROM memories
             WHERE name LIKE ?1 OR description LIKE ?1 OR content LIKE ?1
             ORDER BY updated_at DESC
             LIMIT 20",
        )?;

        let rows = stmt.query_map([&pattern], |row| {
            let tags_str: String = row.get(7)?;
            Ok(Memory {
                id: row.get(0)?,
                memory_type: row.get::<_, String>(1)?
                    .parse()
                    .unwrap_or(MemoryType::User),
                name: row.get(2)?,
                description: row.get(3)?,
                content: row.get(4)?,
                scope: match row.get::<_, String>(5)?.as_str() {
                    "project" => MemoryScope::Project,
                    "agent" => MemoryScope::Agent,
                    _ => MemoryScope::Global,
                },
                scope_ref: row.get(6)?,
                tags: serde_json::from_str(&tags_str).unwrap_or_default(),
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?;

        let mut memories = Vec::new();
        for row in rows {
            memories.push(row?);
        }
        Ok(memories)
    }

    /// Format memories as context string for injection into system prompt
    pub fn format_as_context(memories: &[Memory]) -> String {
        if memories.is_empty() {
            return String::new();
        }

        let mut ctx = String::from("\n## Active Memories\n\n");
        for mem in memories {
            ctx.push_str(&format!(
                "### {} [{}] ({})\n{}\n\n",
                mem.name,
                mem.memory_type,
                mem.scope,
                mem.content
            ));
        }
        ctx
    }
}
