# Chitty Workspace - Claude Code Instructions

## Project Overview
Chitty Workspace is a standalone, local-first AI assistant with agents, tools, marketplace packages, browser automation, and BYOK (Bring Your Own Key) provider support. It runs entirely on the user's machine — no cloud server required.

Free, open-source product that promotes the DataVisions Enterprise platform.

## Technology Stack
- **Language:** Rust (2021 edition)
- **Async Runtime:** Tokio
- **UI:** System tray (tray-icon + tao) + WebView2 chat interface (wry)
- **Local Storage:** SQLite (rusqlite)
- **HTTP Client:** reqwest (for LLM API calls)
- **Local Server:** axum (serves chat UI to WebView2, port 8770)
- **Scheduling:** cron crate (expression parsing for agent scheduler)
- **Secure Storage:** OS keyring (keyring crate) for API keys

## Quick Start

### Building
```bash
cd c:\github\ChittyWorkspace
C:\Users\pauls\.cargo\bin\cargo.exe build --release
```

### Running
```bash
cargo run
cargo run -- config
cargo run -- agents
```

### Forcing HTML rebuild
The chat UI is embedded via `include_str!("../assets/chat.html")`. Cargo doesn't track this dependency, so after editing chat.html:
```bash
touch src/server.rs && cargo build
```

## Project Structure

```
ChittyWorkspace/
├── src/
│   ├── main.rs              # CLI entry point (clap), spawns server + UI
│   ├── chat/
│   │   ├── mod.rs           # Chat engine, CHITTY_SYSTEM_PROMPT, context assembly
│   │   ├── memory.rs        # Persistent memory system (save/recall/search)
│   │   └── context.rs       # Project context loader (chitty.md)
│   ├── config/
│   │   └── mod.rs           # App config (~/.chitty-workspace/config.toml)
│   ├── providers/
│   │   ├── mod.rs           # Provider trait, types (ChatMessage, ToolCall, StreamChunk)
│   │   ├── cloud.rs         # BYOK cloud providers (OpenAI, Anthropic, Google, xAI)
│   │   └── ollama.rs        # Ollama local model provider
│   ├── agents/
│   │   └── mod.rs           # Agent CRUD, sub-agent creation, scoped tools
│   ├── skills/
│   │   └── mod.rs           # Skills system — SKILL.md parser, SkillRegistry, discovery
│   ├── tools/
│   │   ├── mod.rs           # Native tools (file_reader, file_writer, terminal, web_search, etc.)
│   │   ├── web.rs           # Native web_search + web_scraper tools (DuckDuckGo)
│   │   ├── runtime.rs       # Tool runtime (native + custom + marketplace dispatch)
│   │   ├── executor.rs      # Custom tool executor (Python, Node, PowerShell, Shell)
│   │   └── manifest.rs      # Tool manifest parser, PackageManifest struct
│   ├── scheduler.rs         # Agent scheduler — cron-based autonomous task execution
│   ├── integrations/
│   │   └── mod.rs           # API key-based integrations
│   ├── oauth/
│   │   └── mod.rs           # OAuth PKCE flows (Google, etc.)
│   ├── server.rs            # Axum HTTP server, all API endpoints, SSE chat streaming
│   ├── storage/
│   │   ├── mod.rs           # Database manager, connection, data directory
│   │   └── schema.rs        # SQLite schema, migrations V1-V12
│   ├── gpu.rs               # GPU detection for local models
│   ├── huggingface.rs       # HuggingFace Python sidecar
│   └── ui/
│       └── mod.rs           # System tray + WebView2 chat UI
├── assets/
│   ├── chat.html            # Full frontend (HTML + CSS + JS, embedded at compile time)
│   └── marketplace/         # Built-in marketplace packages (bundled, seeded on first run)
│       ├── chitty/
│       ├── google-cloud/
│       ├── google-gmail/
│       ├── google-calendar/
│       ├── slack/
│       ├── social-media/
│       └── web-tools/
├── Cargo.toml
└── .claude/CLAUDE.md         # This file
```

## Key Architecture Patterns

### Default Chitty Agent
When no agent is selected, the `CHITTY_SYSTEM_PROMPT` in `src/chat/mod.rs` is used. Chitty is the system administrator — knows all skills, tools, packages, providers, local API endpoints. It can help build agents, create custom skills, and generate artifacts. AI-first: the default agent is an expert at building agents by recommending skills and writing personas.

### Sub-Agent Architecture (V12)
Packages can generate focused sub-agents at configuration time:
- Example: Google Cloud package creates "WMS Data" sub-agent scoped to a specific BigQuery dataset
- `parent_agent_id` column on agents table links children to parent package agents
- `sub_agent_tools` table stores scoped tool configs with `locked_params` JSON
- Locked params auto-merge into every tool call the sub-agent makes
- Cascade delete: removing parent removes all sub-agents
- Sub-agents appear nested under parent in the agent dropdown (`<optgroup>`)
- `AgentsManager::create_sub_agent()`, `list_children()`, `load_sub_agent_tools()`
- API: `GET /api/agents/:id/children`, `POST /api/agents/:id/sub-agents`
- Package defines `sub_agent_template` in package.json with form_fields, scoped_tools, persona_template

### Skills (Agent Skills Open Standard)
- Skills are composable capability packages following the agentskills.io open standard
- Each skill is a folder with a `SKILL.md` file (YAML frontmatter + markdown instructions)
- Skills bundle domain expertise + tool requirements (`allowed-tools` field)
- `src/skills/mod.rs` — Skill struct, SkillRegistry, SKILL.md parser, multi-path discovery
- Discovery paths: `.agents/skills/` (cross-client), `.chitty/skills/`, `~/.chitty-workspace/skills/`, marketplace packages
- Progressive loading: catalog metadata at startup → full SKILL.md via `load_skill` tool → resources via file_reader
- Agents select skills (not tools) — skills bring their own tools automatically
- API: `GET /api/skills`, `GET /api/skills/:name`

### Artifacts
- Rendered previews of rich content (HTML apps, charts, code, SVG, markdown)
- Agent wraps output in `<artifact type="html" title="Name">...</artifact>` tags
- Frontend detects artifact tags in `formatContent()`, renders clickable cards in chat
- Clicking opens live preview in Dynamic View (sandboxed iframe for HTML, code view, etc.)
- Iterate bar lets users request changes without leaving the preview
- In-memory versioning (per-session, not persisted to DB)

### Dynamic Action Panel
The Action Panel (right side) has fixed tabs (Activity, Providers, Marketplace) and a dynamic view container. Components like browser, media, agent config, package editor, and artifact previews open dynamically via `openDynamicView(icon, title, html)` in the frontend. The Action Panel is resizable with a drag handle.

### Package Editor UI
When viewing a package detail in the Marketplace tab:
- Setup steps, tools list, capabilities
- **Configured Agents** section: lists sub-agents created from this package
- **"+ Add Dataset Agent"** button opens a configuration form in the dynamic view
- Form driven by `sub_agent_template` in package.json (form_fields, scoped_tools, persona_template)
- Feature flags with toggle switches
- Allowed resources with discover button

### Chat Markdown Rendering
`formatContent()` renders full markdown in chat messages:
- Headings (h1, h2, h3), bold, italic, inline code
- Fenced code blocks with language labels, dark theme, copy buttons
- Unordered/ordered lists, tables, blockquotes, horizontal rules, links
- Paragraph breaks between tool-call iterations (no mashed text)

### Slash Commands
Frontend intercepts messages starting with `/` in `sendMessageInPanel()` before they reach the LLM. Commands are handled by `handleSlashCommand()` which routes to specific handlers.

### Agent Scheduler
- `src/scheduler.rs` — Background Tokio task polling every 30 seconds
- `scheduled_tasks` SQLite table (migration V7)
- CRUD API at `/api/schedules`
- Cron expression parsing via the `cron` crate
- Frontend UI via `/schedule` slash command

### Approval System
- `action_requires_approval()` in server.rs checks tool/action pairs
- Frontend shows Deny / Always allow for session / Allow once buttons
- `sessionAutoApprove` state flag auto-approves after user opts in
- Denied results saved to DB to prevent conversation corruption
- Agents can be set to `approval_mode: "auto"` to bypass entirely

### Terminal Tool (Cross-Platform)
- PowerShell on Windows (with CREATE_NO_WINDOW flag)
- zsh on macOS
- sh on Linux
- PATH extended with common tool locations (gcloud SDK, etc.) automatically
- HTTP: `Invoke-RestMethod` (Windows) or `curl` (Linux/Mac)

### Native Web Tools
`web_search` and `web_scraper` are native Rust tools (not marketplace packages):
- `web_search` — DuckDuckGo search via HTML scraping
- `web_scraper` — HTTP fetch + HTML-to-text extraction
- Registered in `ORCHESTRATOR_TOOLS` so Chitty always has them

### Marketplace Packages
Each package is its own GitHub repository (e.g., `chitty-pkg-google-cloud`). Bundled packages are also embedded in `assets/marketplace/` and seeded to the data directory on first run.

Package structure:
- `package.json` — name, vendor, description, tools[], setup_steps[], configurable_resources[], feature_flags[], sub_agent_template, agent_config
- `SKILL.md` — Agent instructions following the skills open standard
- Tool directories with `manifest.json` + script (Python/Node/PowerShell/Shell)
- Scripts read JSON from stdin, write JSON to stdout

**Package auth standards (BYOK):**
- Application-level: API keys, OAuth client credentials (stored in OS keyring)
- User-level: OAuth flows triggered at runtime ("Click here to login")
- Some packages need both (e.g., Slack: app credentials + user OAuth)
- Never use the DataVisions GCP project for user-facing services

**Community packages:**
- Anyone can build a package in their own repo following the package structure
- Users can install from GitHub URL
- Community can submit packages to the Chitty Marketplace for approval

### open_agent_panel Tool
Frontend-intercepted tool that lets Chitty open agents in new panels. Backend returns a UI command, frontend creates the panel and optionally sends a message.

## Core Concepts

### Agents
Agents combine: persona + skills + execution config. The persona is who the agent IS (short identity). Skills provide domain expertise and bring their own tools.
Fields: name, description, persona, skills[], preferred_provider/model, max_iterations, temperature, max_tokens, approval_mode ("prompt" or "auto"), parent_agent_id, package_id.
DB columns renamed in V8: `instructions` → `persona`, `tools` → `skills`. API accepts both old and new field names via serde aliases.

### Sub-Agents
Sub-agents are children of package agents, created at configuration time:
- Deterministic ID: `{parent_id}-{slug(name)}` (e.g., `pkg-google-cloud-wms-data`)
- Inherit package_id from parent
- Have scoped tools with locked_params (auto-merged into tool calls)
- Appear nested in agent dropdown under parent package
- Know their caller context (dispatched by parent vs direct user chat)

### Chat Flow
1. User sends message (or `/command` intercepted)
2. Context assembled: agent instructions + chitty.md + memories + tools + history
3. Sent to LLM (any provider) via streaming SSE
4. LLM responds with text or tool calls
5. Tool calls: approval gate → execute → results sent back
6. LLM generates final response
7. Everything persisted to SQLite

### Memory System
Types: user, feedback, project, reference. Scopes: global, project, agent.
Auto-loaded at conversation start, injected into system prompt.

### Context Assembly Order
1. Agent Persona (from agent or CHITTY_SYSTEM_PROMPT)
2. Project Context (chitty.md)
3. Active Memories (global + scoped)
4. Skill Catalog (available skills — names + descriptions, ~50-100 tokens each)
5. Tool Instructions (auto-injected from tools, filtered by skills' allowed-tools)
6. Tool Definitions (OpenAI function calling format, filtered by skills)
7. Conversation History (trimmed to fit context window)
8. User Message

## Data Storage

```
~/.chitty-workspace/  (AppData/Roaming/datavisions/chitty-workspace/ on Windows)
├── config.toml              # App settings (TOML)
├── workspace.db             # SQLite database (schema V12)
│   ├── conversations        # Chat sessions (agent_id, project, provider, model)
│   ├── messages             # Full message history (with tool calls)
│   ├── agents               # Agent definitions (with parent_agent_id for sub-agents)
│   ├── sub_agent_tools      # Scoped tool configs with locked_params
│   ├── memories             # Persistent memories (typed, scoped)
│   ├── scheduled_tasks      # Cron-based agent schedules
│   ├── custom_tools         # User/AI-generated tools
│   ├── token_usage          # Token tracking per conversation
│   ├── provider_configs     # Provider settings (keys in OS keyring)
│   ├── marketplace_packages # Installed package metadata
│   ├── package_resources    # Allowed resources per package
│   ├── package_features     # Feature flags per package
│   ├── connection_event_routes  # WebSocket event routing
│   ├── connection_status    # Connection health tracking
│   └── agent_presets        # Saved multi-package combinations
├── tools/
│   ├── marketplace/         # Installed marketplace packages
│   └── custom/              # User-created tools
└── models/                  # GGUF model files (for HuggingFace sidecar)
```

## Code Style

- Rust 2021 edition
- Use `anyhow::Result` for application errors, `thiserror` for library errors
- Async everywhere (tokio runtime)
- Serde for all serialization
- Keep modules focused — one responsibility per module
- Frontend is a single `assets/chat.html` file (embedded at compile time via `include_str!`)

## Relationship to DataVisions

Chitty Workspace is a **completely separate product** from DataVisions Enterprise.
- Does NOT connect to the DataVisions cloud platform
- Does NOT share code with Chitty Bridge Service
- Shares architectural inspiration but is an independent codebase
- Free, open-source product that promotes DataVisions Enterprise for teams needing governance, DataHub, multi-tenancy, etc.
- The DataVisions GCP project is ONLY used to serve the marketplace catalog (browsing + installing approved packages) — never for user-facing services
