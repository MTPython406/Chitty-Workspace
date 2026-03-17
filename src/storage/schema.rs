//! Database schema and migrations
//!
//! All tables for conversations, messages, memories, skills, tools, and providers.

use anyhow::Result;
use rusqlite::Connection;

/// Current schema version
const SCHEMA_VERSION: i32 = 3;

/// Run all pending migrations
pub fn run_migrations(conn: &Connection) -> Result<()> {
    // Create migrations tracking table
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER NOT NULL DEFAULT 0
        );
        INSERT OR IGNORE INTO schema_version (rowid, version) VALUES (1, 0);",
    )?;

    let current: i32 =
        conn.query_row("SELECT version FROM schema_version", [], |row| row.get(0))?;

    if current < 1 {
        migrate_v1(conn)?;
    }
    if current < 2 {
        migrate_v2(conn)?;
    }
    if current < 3 {
        migrate_v3(conn)?;
    }

    conn.execute(
        "UPDATE schema_version SET version = ? WHERE rowid = 1",
        [SCHEMA_VERSION],
    )?;

    Ok(())
}

/// V1: Initial schema
fn migrate_v1(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        -- ============================================================
        -- CONVERSATIONS
        -- ============================================================
        CREATE TABLE IF NOT EXISTS conversations (
            id              TEXT PRIMARY KEY,
            title           TEXT,
            skill_id        TEXT,
            project_path    TEXT,
            provider        TEXT NOT NULL,
            model           TEXT NOT NULL,
            created_at      TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_conversations_project
            ON conversations(project_path);
        CREATE INDEX IF NOT EXISTS idx_conversations_skill
            ON conversations(skill_id);
        CREATE INDEX IF NOT EXISTS idx_conversations_updated
            ON conversations(updated_at DESC);

        -- ============================================================
        -- MESSAGES
        -- ============================================================
        CREATE TABLE IF NOT EXISTS messages (
            id                  TEXT PRIMARY KEY,
            conversation_id     TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
            parent_message_id   TEXT REFERENCES messages(id),
            role                TEXT NOT NULL,  -- user, assistant, system, tool
            content             TEXT NOT NULL DEFAULT '',
            tool_calls          TEXT,           -- JSON array of tool calls
            tool_call_id        TEXT,           -- ID of the tool call this message responds to
            token_count         INTEGER,
            created_at          TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_messages_conversation
            ON messages(conversation_id, created_at);
        CREATE INDEX IF NOT EXISTS idx_messages_parent
            ON messages(parent_message_id);

        -- ============================================================
        -- MEMORIES
        -- Persistent knowledge the agent retains across sessions.
        -- Scoped: global, per-project, or per-skill.
        -- ============================================================
        CREATE TABLE IF NOT EXISTS memories (
            id              TEXT PRIMARY KEY,
            memory_type     TEXT NOT NULL,      -- user, feedback, project, reference
            name            TEXT NOT NULL,
            description     TEXT NOT NULL,      -- one-line summary for relevance matching
            content         TEXT NOT NULL,      -- full memory content
            scope           TEXT NOT NULL DEFAULT 'global',  -- global, project, skill
            scope_ref       TEXT,               -- project path or skill ID (NULL for global)
            tags            TEXT,               -- JSON array of tags
            created_at      TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_memories_type
            ON memories(memory_type);
        CREATE INDEX IF NOT EXISTS idx_memories_scope
            ON memories(scope, scope_ref);

        -- ============================================================
        -- SKILLS
        -- Skill = Instructions + Tool Set
        -- ============================================================
        CREATE TABLE IF NOT EXISTS skills (
            id                  TEXT PRIMARY KEY,
            name                TEXT NOT NULL,
            description         TEXT NOT NULL DEFAULT '',
            instructions        TEXT NOT NULL DEFAULT '',
            tools               TEXT NOT NULL DEFAULT '[]',    -- JSON array of tool names
            project_path        TEXT,                          -- NULL = global skill
            preferred_provider  TEXT,
            preferred_model     TEXT,
            tags                TEXT NOT NULL DEFAULT '[]',    -- JSON array
            version             TEXT NOT NULL DEFAULT '1.0',
            ai_generated        INTEGER NOT NULL DEFAULT 0,
            created_at          TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at          TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_skills_project
            ON skills(project_path);

        -- ============================================================
        -- CUSTOM TOOLS
        -- User-defined or AI-generated tools
        -- ============================================================
        CREATE TABLE IF NOT EXISTS custom_tools (
            id              TEXT PRIMARY KEY,
            name            TEXT NOT NULL UNIQUE,
            description     TEXT NOT NULL DEFAULT '',
            instructions    TEXT,                       -- detailed usage instructions for LLM
            parameters      TEXT NOT NULL DEFAULT '{}', -- JSON Schema
            script_type     TEXT,                       -- python, node, shell, http
            script_body     TEXT,                       -- script content or HTTP config
            category        TEXT NOT NULL DEFAULT 'custom',
            ai_generated    INTEGER NOT NULL DEFAULT 0,
            created_at      TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- ============================================================
        -- PROVIDER CONFIGS
        -- BYOK provider settings (keys stored in OS keyring, not here)
        -- ============================================================
        CREATE TABLE IF NOT EXISTS provider_configs (
            id              TEXT PRIMARY KEY,
            provider_id     TEXT NOT NULL UNIQUE,   -- openai, anthropic, google, xai, ollama, huggingface
            display_name    TEXT NOT NULL,
            base_url        TEXT,
            enabled         INTEGER NOT NULL DEFAULT 1,
            extra_config    TEXT DEFAULT '{}',      -- JSON for provider-specific settings
            created_at      TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- Seed default providers
        INSERT OR IGNORE INTO provider_configs (id, provider_id, display_name, base_url, enabled) VALUES
            ('prov-openai',      'openai',      'OpenAI',       'https://api.openai.com/v1',          0),
            ('prov-anthropic',   'anthropic',   'Anthropic',    'https://api.anthropic.com',          0),
            ('prov-google',      'google',      'Google AI',    'https://generativelanguage.googleapis.com', 0),
            ('prov-xai',         'xai',         'xAI',          'https://api.x.ai/v1',                0),
            ('prov-ollama',      'ollama',      'Ollama',       'http://localhost:11434',              1),
            ('prov-huggingface', 'huggingface', 'HuggingFace',  'http://localhost:8766',              0);
        ",
    )?;

    tracing::info!("Database migrated to v1");
    Ok(())
}

/// V2: User-selected models + token tracking
fn migrate_v2(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        -- ============================================================
        -- USER-SELECTED MODELS
        -- Models the user has chosen to add to their system.
        -- Fetched from provider API, user picks which ones to keep.
        -- ============================================================
        CREATE TABLE IF NOT EXISTS user_models (
            id              TEXT PRIMARY KEY,
            provider_id     TEXT NOT NULL,       -- openai, anthropic, xai, etc.
            model_id        TEXT NOT NULL,        -- e.g. grok-3-latest
            display_name    TEXT NOT NULL,
            context_window  INTEGER,
            supports_tools  INTEGER NOT NULL DEFAULT 0,
            supports_streaming INTEGER NOT NULL DEFAULT 0,
            supports_vision INTEGER NOT NULL DEFAULT 0,
            is_default      INTEGER NOT NULL DEFAULT 0,  -- default model for this provider
            enabled         INTEGER NOT NULL DEFAULT 1,
            added_at        TEXT NOT NULL DEFAULT (datetime('now')),
            UNIQUE(provider_id, model_id)
        );

        CREATE INDEX IF NOT EXISTS idx_user_models_provider
            ON user_models(provider_id);

        -- ============================================================
        -- TOKEN USAGE TRACKING
        -- Per-message token counts for cost tracking & analytics
        -- ============================================================
        CREATE TABLE IF NOT EXISTS token_usage (
            id                  TEXT PRIMARY KEY,
            conversation_id     TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
            message_id          TEXT REFERENCES messages(id) ON DELETE SET NULL,
            provider_id         TEXT NOT NULL,
            model_id            TEXT NOT NULL,
            prompt_tokens       INTEGER NOT NULL DEFAULT 0,
            completion_tokens   INTEGER NOT NULL DEFAULT 0,
            total_tokens        INTEGER NOT NULL DEFAULT 0,
            cached_tokens       INTEGER DEFAULT 0,
            created_at          TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_token_usage_conversation
            ON token_usage(conversation_id);
        CREATE INDEX IF NOT EXISTS idx_token_usage_provider_model
            ON token_usage(provider_id, model_id);
        CREATE INDEX IF NOT EXISTS idx_token_usage_created
            ON token_usage(created_at);
        ",
    )?;

    tracing::info!("Database migrated to v2 (user models + token tracking)");
    Ok(())
}

/// V3: Execution config columns on skills table
fn migrate_v3(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        ALTER TABLE skills ADD COLUMN max_iterations INTEGER;
        ALTER TABLE skills ADD COLUMN temperature REAL;
        ALTER TABLE skills ADD COLUMN max_tokens INTEGER;
        ",
    )?;

    tracing::info!("Database migrated to v3 (skill execution config)");
    Ok(())
}
