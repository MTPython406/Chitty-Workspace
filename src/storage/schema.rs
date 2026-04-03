//! Database schema and migrations
//!
//! All tables for conversations, messages, memories, agents, tools, and providers.

use anyhow::Result;
use rusqlite::Connection;

/// Current schema version
const SCHEMA_VERSION: i32 = 13;

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
    if current < 4 {
        migrate_v4(conn)?;
    }
    if current < 5 {
        migrate_v5(conn)?;
    }
    if current < 6 {
        migrate_v6(conn)?;
    }
    if current < 7 {
        migrate_v7(conn)?;
    }
    if current < 8 {
        migrate_v8(conn)?;
    }
    if current < 9 {
        migrate_v9(conn)?;
    }
    if current < 10 {
        migrate_v10(conn)?;
    }
    if current < 11 {
        migrate_v11(conn)?;
    }
    if current < 12 {
        migrate_v12(conn)?;
    }
    if current < 13 {
        migrate_v13(conn)?;
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
        -- Scoped: global, per-project, or per-agent.
        -- ============================================================
        CREATE TABLE IF NOT EXISTS memories (
            id              TEXT PRIMARY KEY,
            memory_type     TEXT NOT NULL,      -- user, feedback, project, reference
            name            TEXT NOT NULL,
            description     TEXT NOT NULL,      -- one-line summary for relevance matching
            content         TEXT NOT NULL,      -- full memory content
            scope           TEXT NOT NULL DEFAULT 'global',  -- global, project, skill
            scope_ref       TEXT,               -- project path or agent ID (NULL for global)
            tags            TEXT,               -- JSON array of tags
            created_at      TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_memories_type
            ON memories(memory_type);
        CREATE INDEX IF NOT EXISTS idx_memories_scope
            ON memories(scope, scope_ref);

        -- ============================================================
        -- SKILLS (legacy — renamed to agents in v4)
        -- ============================================================
        CREATE TABLE IF NOT EXISTS skills (
            id                  TEXT PRIMARY KEY,
            name                TEXT NOT NULL,
            description         TEXT NOT NULL DEFAULT '',
            instructions        TEXT NOT NULL DEFAULT '',
            tools               TEXT NOT NULL DEFAULT '[]',    -- JSON array of tool names
            project_path        TEXT,                          -- NULL = global
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

/// V3: Execution config columns on skills table (legacy)
fn migrate_v3(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        ALTER TABLE skills ADD COLUMN max_iterations INTEGER;
        ALTER TABLE skills ADD COLUMN temperature REAL;
        ALTER TABLE skills ADD COLUMN max_tokens INTEGER;
        ",
    )?;

    tracing::info!("Database migrated to v3 (execution config)");
    Ok(())
}

/// V4: Rename skills → agents (clean slate)
fn migrate_v4(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        -- Drop legacy skills table
        DROP TABLE IF EXISTS skills;

        -- Create agents table
        CREATE TABLE IF NOT EXISTS agents (
            id                  TEXT PRIMARY KEY,
            name                TEXT NOT NULL,
            description         TEXT NOT NULL DEFAULT '',
            instructions        TEXT NOT NULL DEFAULT '',
            tools               TEXT NOT NULL DEFAULT '[]',    -- JSON array of tool names
            project_path        TEXT,                          -- NULL = global agent
            preferred_provider  TEXT,
            preferred_model     TEXT,
            tags                TEXT NOT NULL DEFAULT '[]',    -- JSON array
            version             TEXT NOT NULL DEFAULT '1.0',
            ai_generated        INTEGER NOT NULL DEFAULT 0,
            max_iterations      INTEGER,
            temperature         REAL,
            max_tokens          INTEGER,
            created_at          TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at          TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_agents_project
            ON agents(project_path);

        -- Update memory scope values
        UPDATE memories SET scope = 'agent' WHERE scope = 'skill';
        ",
    )?;

    // Add agent_id column if it doesn't already exist (idempotent)
    let has_agent_id: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('conversations') WHERE name='agent_id'")?
        .query_row([], |row| row.get::<_, i32>(0))
        .map(|c| c > 0)
        .unwrap_or(false);

    if !has_agent_id {
        conn.execute_batch("ALTER TABLE conversations ADD COLUMN agent_id TEXT;")?;
    }

    // Create index AFTER ensuring the column exists
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_conversations_agent ON conversations(agent_id);")?;

    tracing::info!("Database migrated to v4 (skills → agents rename)");
    Ok(())
}

/// V5: Marketplace package configuration — allowed resources, feature flags, install state
fn migrate_v5(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        -- ============================================================
        -- MARKETPLACE PACKAGES (installed state + metadata)
        -- Tracks which packages are installed and their configuration.
        -- ============================================================
        CREATE TABLE IF NOT EXISTS marketplace_packages (
            id              TEXT PRIMARY KEY,          -- same as package name (e.g. 'google-cloud')
            name            TEXT NOT NULL UNIQUE,
            display_name    TEXT NOT NULL,
            vendor          TEXT NOT NULL,
            version         TEXT NOT NULL,
            status          TEXT NOT NULL DEFAULT 'installed',  -- installed, setup_required, disabled
            installed_at    TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- ============================================================
        -- PACKAGE ALLOWED RESOURCES
        -- Per-package resource scoping (e.g. allowed datasets, buckets).
        -- The agent can only use resources listed here.
        -- ============================================================
        CREATE TABLE IF NOT EXISTS package_resources (
            id              TEXT PRIMARY KEY,
            package_id      TEXT NOT NULL REFERENCES marketplace_packages(id) ON DELETE CASCADE,
            resource_type   TEXT NOT NULL,             -- e.g. 'datasets', 'buckets', 'repos'
            resource_id     TEXT NOT NULL,             -- e.g. 'my_dataset', 'my-bucket-123'
            display_name    TEXT,                      -- human label (optional)
            config          TEXT DEFAULT '{}',         -- JSON — extra config per resource
            created_at      TEXT NOT NULL DEFAULT (datetime('now')),
            UNIQUE(package_id, resource_type, resource_id)
        );

        CREATE INDEX IF NOT EXISTS idx_package_resources_pkg
            ON package_resources(package_id, resource_type);

        -- ============================================================
        -- PACKAGE FEATURE FLAGS
        -- Per-package feature toggles (e.g. allow_create_dataset).
        -- ============================================================
        CREATE TABLE IF NOT EXISTS package_features (
            id              TEXT PRIMARY KEY,
            package_id      TEXT NOT NULL REFERENCES marketplace_packages(id) ON DELETE CASCADE,
            feature_id      TEXT NOT NULL,             -- e.g. 'allow_create_dataset'
            enabled         INTEGER NOT NULL DEFAULT 0,
            updated_at      TEXT NOT NULL DEFAULT (datetime('now')),
            UNIQUE(package_id, feature_id)
        );

        CREATE INDEX IF NOT EXISTS idx_package_features_pkg
            ON package_features(package_id);
        ",
    )?;

    tracing::info!("Database migrated to v5 (marketplace package configuration)");
    Ok(())
}

/// V6: Agent approval mode
fn migrate_v6(conn: &Connection) -> Result<()> {
    // Add approval_mode column to agents table
    // "prompt" = ask user before sensitive actions (default)
    // "auto"   = auto-approve all actions (fully autonomous)
    conn.execute_batch(
        "ALTER TABLE agents ADD COLUMN approval_mode TEXT NOT NULL DEFAULT 'prompt';",
    )?;

    tracing::info!("Database migrated to v6 (agent approval mode)");
    Ok(())
}

/// V7: Scheduled tasks for autonomous agent execution
fn migrate_v7(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS scheduled_tasks (
            id              TEXT PRIMARY KEY,
            name            TEXT NOT NULL,
            agent_id        TEXT,
            prompt          TEXT NOT NULL,
            cron_expression TEXT NOT NULL,
            project_path    TEXT,
            enabled         INTEGER NOT NULL DEFAULT 1,
            auto_approve    INTEGER NOT NULL DEFAULT 1,
            last_run_at     TEXT,
            next_run_at     TEXT,
            created_at      TEXT NOT NULL,
            updated_at      TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_scheduled_tasks_enabled ON scheduled_tasks(enabled);
        CREATE INDEX IF NOT EXISTS idx_scheduled_tasks_next_run ON scheduled_tasks(next_run_at);",
    )?;

    tracing::info!("Database migrated to v7 (scheduled tasks)");
    Ok(())
}

/// V8: Agents — rename instructions→persona, tools→skills
///
/// Agents now select skills (composable capability packages) instead of
/// individual tools. Skills bundle instructions + tool requirements together.
/// The "persona" field replaces "instructions" — it's just who the agent IS,
/// not what it knows (domain expertise lives in skills).
fn migrate_v8(conn: &Connection) -> Result<()> {
    // SQLite 3.25.0+ supports ALTER TABLE RENAME COLUMN
    // rusqlite bundles SQLite 3.45+, so this is safe
    conn.execute_batch(
        "ALTER TABLE agents RENAME COLUMN instructions TO persona;
         ALTER TABLE agents RENAME COLUMN tools TO skills;",
    )?;

    tracing::info!("Database migrated to v8 (agents: instructions→persona, tools→skills)");
    Ok(())
}

/// V9: Agent context management configuration
fn migrate_v9(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "ALTER TABLE agents ADD COLUMN context_budget_pct INTEGER;
         ALTER TABLE agents ADD COLUMN compaction_strategy TEXT;
         ALTER TABLE agents ADD COLUMN max_conversation_turns INTEGER;",
    )?;

    tracing::info!("Database migrated to v9 (agent context management config)");
    Ok(())
}

/// V10: Persistent connections — event routing and status tracking
///
/// Marketplace packages can declare persistent background connections
/// (WebSockets, listeners, etc.). Events from these connections are
/// routed to configured agents. This migration adds the tables to
/// store routing configuration and connection runtime status.
fn migrate_v10(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS connection_event_routes (
            id              TEXT PRIMARY KEY,
            package_id      TEXT NOT NULL,
            connection_id   TEXT NOT NULL,
            event_id        TEXT NOT NULL,
            agent_id        TEXT REFERENCES agents(id) ON DELETE SET NULL,
            provider        TEXT,
            model           TEXT,
            auto_approve    INTEGER NOT NULL DEFAULT 1,
            enabled         INTEGER NOT NULL DEFAULT 1,
            created_at      TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at      TEXT NOT NULL DEFAULT (datetime('now')),
            UNIQUE(package_id, connection_id, event_id)
        );

        CREATE INDEX IF NOT EXISTS idx_conn_routes_pkg
            ON connection_event_routes(package_id, connection_id);

        CREATE TABLE IF NOT EXISTS connection_status (
            id              TEXT PRIMARY KEY,
            package_id      TEXT NOT NULL,
            connection_id   TEXT NOT NULL,
            status          TEXT NOT NULL DEFAULT 'stopped',
            error_message   TEXT,
            started_at      TEXT,
            last_heartbeat  TEXT,
            restart_count   INTEGER NOT NULL DEFAULT 0,
            updated_at      TEXT NOT NULL DEFAULT (datetime('now')),
            UNIQUE(package_id, connection_id)
        );",
    )?;

    tracing::info!("Database migrated to v10 (persistent connections: event routes + status)");
    Ok(())
}

/// V11: Package-centric orchestrator — package_id on agents + presets table
fn migrate_v11(conn: &Connection) -> Result<()> {
    // Add package_id column to agents (links agent to source marketplace package)
    conn.execute_batch(
        "ALTER TABLE agents ADD COLUMN package_id TEXT;
        CREATE INDEX IF NOT EXISTS idx_agents_package ON agents(package_id);

        -- Agent presets: saved multi-package combinations for scheduled tasks
        CREATE TABLE IF NOT EXISTS agent_presets (
            id              TEXT PRIMARY KEY,
            name            TEXT NOT NULL,
            description     TEXT,
            package_ids     TEXT NOT NULL DEFAULT '[]',
            prompt_template TEXT,
            provider        TEXT,
            model           TEXT,
            cron_expression TEXT,
            enabled         INTEGER NOT NULL DEFAULT 1,
            created_at      TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );",
    )?;

    tracing::info!("Database migrated to v11 (package-centric agents + presets)");
    Ok(())
}

/// V12: Sub-agent architecture — parent/child relationships + scoped tool configs
///
/// Packages can now generate focused sub-agents at configuration time.
/// Example: Google Cloud package creates a "WMS Data" sub-agent scoped
/// to a specific BigQuery dataset with locked tool parameters.
fn migrate_v12(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "-- Parent/child relationship for sub-agents
        ALTER TABLE agents ADD COLUMN parent_agent_id TEXT;
        CREATE INDEX IF NOT EXISTS idx_agents_parent ON agents(parent_agent_id);

        -- Scoped tool configurations for sub-agents
        -- Each row binds a tool to a sub-agent with locked-in parameter defaults
        -- that are auto-merged into every tool call the sub-agent makes.
        CREATE TABLE IF NOT EXISTS sub_agent_tools (
            id              TEXT PRIMARY KEY,
            agent_id        TEXT NOT NULL,
            tool_name       TEXT NOT NULL,
            display_name    TEXT,
            locked_params   TEXT DEFAULT '{}',
            enabled         INTEGER NOT NULL DEFAULT 1,
            created_at      TEXT NOT NULL DEFAULT (datetime('now')),
            UNIQUE(agent_id, tool_name),
            FOREIGN KEY (agent_id) REFERENCES agents(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_sub_agent_tools_agent ON sub_agent_tools(agent_id);",
    )?;

    tracing::info!("Database migrated to v12 (sub-agent architecture)");
    Ok(())
}

/// V13: Add context_length to agents for per-agent context window override
fn migrate_v13(conn: &Connection) -> Result<()> {
    // Check if column already exists before adding (idempotent migration)
    let has_column: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('agents') WHERE name='context_length'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|count| count > 0)
        .unwrap_or(false);

    if !has_column {
        conn.execute_batch(
            "ALTER TABLE agents ADD COLUMN context_length INTEGER;",
        )?;
    }

    tracing::info!("Database migrated to v13 (agent context_length)");
    Ok(())
}
