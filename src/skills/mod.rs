//! Skills system
//!
//! A Skill = System Prompt + Instructions + Tool Set
//! Skills are savable, loadable, and shareable across chats, providers, and models.

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
    /// System prompt / instructions for the agent
    pub instructions: String,
    /// Tool names this skill uses
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
}

/// Skills Builder - AI-assisted skill creation
///
/// Uses the user's own BYOK key to generate tool definitions
/// and instructions for new skills.
pub struct SkillsBuilder;

// TODO: Implement
// - Skill CRUD (save/load/delete from SQLite)
// - Skill import/export (JSON files)
// - Skills Builder (AI generates tools + instructions)
// - Skill activation per chat session
// - Skill marketplace/sharing format
