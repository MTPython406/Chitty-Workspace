# Chitty Workspace - Claude Code Instructions

## Project Overview
Chitty Workspace is a standalone, local-first AI assistant with skills, tools, and BYOK (Bring Your Own Key) provider support. It runs entirely on the user's machine — no cloud server required.

Free product that promotes the DataVisions Enterprise platform.

## Technology Stack
- **Language:** Rust (2021 edition)
- **Async Runtime:** Tokio
- **UI:** System tray (tray-icon + tao) + WebView2 chat interface (wry)
- **Local Storage:** SQLite (rusqlite)
- **HTTP Client:** reqwest (for LLM API calls)
- **Local Server:** axum (serves chat UI to WebView2)
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
cargo run -- skills
```

## Project Structure

```
ChittyWorkspace/
├── src/
│   ├── main.rs              # CLI entry point (clap)
│   ├── chat/
│   │   ├── mod.rs           # Chat engine, conversation types, context assembly
│   │   ├── memory.rs        # Persistent memory system (save/recall/search)
│   │   └── context.rs       # Project context loader (chitty.md)
│   ├── config/
│   │   └── mod.rs           # App config (~/.chitty-workspace/config.toml)
│   ├── providers/
│   │   ├── mod.rs           # Provider trait, types (ChatMessage, ToolCall, StreamChunk)
│   │   ├── cloud.rs         # BYOK cloud providers (OpenAI, Anthropic, Google, xAI)
│   │   └── ollama.rs        # Ollama local model provider
│   ├── skills/
│   │   └── mod.rs           # Skills system (Skill = Instructions + Tools)
│   ├── tools/
│   │   └── mod.rs           # Tool system (native + custom + integration tools)
│   ├── integrations/
│   │   └── mod.rs           # API key-based integrations
│   ├── storage/
│   │   ├── mod.rs           # Database manager, connection, data directory
│   │   └── schema.rs        # SQLite schema, migrations, table definitions
│   └── ui/
│       └── mod.rs           # System tray + WebView2 chat UI
├── assets/                   # Icons, static resources
├── sidecar/                  # HuggingFace Python inference sidecar
├── Cargo.toml
└── .claude/CLAUDE.md         # This file
```

## Architecture

```
┌─────────────────────────────────────────────────┐
│              Chitty Workspace                    │
├─────────────────────────────────────────────────┤
│  UI Layer (WebView2 + System Tray)              │
│  ├── Chat interface                              │
│  ├── Skill browser / builder                     │
│  └── Settings (providers, keys, projects)        │
├─────────────────────────────────────────────────┤
│  Chat Engine                                     │
│  ├── Message loop (user → LLM → tools → LLM)   │
│  ├── Streaming responses                         │
│  └── Context window management                   │
├─────────────────────────────────────────────────┤
│  Skills System                                   │
│  ├── Skill = Instructions + Tool Set             │
│  ├── Skills Builder (AI-generated)               │
│  └── Import/export/share                         │
├─────────────────────────────────────────────────┤
│  Providers (BYOK)          │  Tools              │
│  ├── OpenAI                │  ├── Native (file,  │
│  ├── Anthropic             │  │   terminal, code)│
│  ├── Google                │  ├── Custom (user/  │
│  ├── xAI                   │  │   AI-generated)  │
│  ├── Ollama (local)        │  └── Integration    │
│  └── HuggingFace (sidecar) │      tools          │
├─────────────────────────────────────────────────┤
│  Storage (SQLite)           │  Config (TOML)     │
│  ├── Conversations          │  ├── Providers     │
│  ├── Messages               │  ├── Projects      │
│  ├── Skills                 │  └── UI prefs      │
│  └── Custom tools           │                    │
│                             │  Keyring (OS)      │
│                             │  └── API keys      │
└─────────────────────────────────────────────────┘
```

## Core Concepts

### Skills
A Skill is the central unit. It combines:
- **Instructions:** System prompt that tells the agent what it can do and how
- **Tools:** Set of tools the agent can use when this skill is active
- **Metadata:** Name, description, tags, preferred provider/model

Skills are savable, loadable per project or globally, and shareable as JSON files.
The Skills Builder uses the user's own BYOK key to AI-generate new tools and instructions.

### Providers
BYOK — the user provides their own API keys. Stored securely in OS keyring.
- Cloud: OpenAI, Anthropic, Google, xAI (direct API calls from Rust)
- Local: Ollama (proxy to localhost:11434), HuggingFace (Python sidecar)

### Tools
- **Native:** Built into the binary (file_reader, file_writer, terminal, code_search, code_analyzer)
- **Custom:** User-defined or AI-generated via Skills Builder
- **Integration:** Generated from configured API integrations

### Chat
Single-agent chat with tool calling loop:
1. User sends message
2. Context assembled: skill instructions + chitty.md + memories + tools + history
3. Sent to LLM (any provider)
4. LLM responds with text or tool calls
5. Tool calls executed locally, results sent back
6. LLM generates final response
7. Everything persisted to SQLite

---

## Memory System

Persistent knowledge the agent retains across sessions. Unlike simple chat logs, memories are **semantic** — the agent actively decides what to remember and recalls relevant memories in future conversations.

### Memory Types

| Type | Purpose | Example |
|------|---------|---------|
| `user` | User's role, preferences, expertise | "Senior Rust developer, prefers minimal dependencies" |
| `feedback` | Corrections and guidance | "Don't use unwrap() — always handle errors properly" |
| `project` | Project-specific context | "Migrating from REST to gRPC by end of Q2" |
| `reference` | Pointers to external resources | "CI docs are in Confluence at /team/ci-setup" |

### Memory Scoping

| Scope | When loaded | Example |
|-------|-------------|---------|
| `global` | Every conversation | User preferences, general feedback |
| `project` | When chatting in that project directory | Project-specific decisions, conventions |
| `skill` | When that skill is active | Skill-specific corrections |

### How Memories Work

1. **Auto-load:** At conversation start, relevant memories are loaded (global + project + skill)
2. **Injected:** Memories are formatted and injected into the system prompt
3. **Agent saves:** When the agent learns something important, it uses `save_memory` tool
4. **Agent recalls:** Agent can search memories with `search_memory` tool
5. **User manages:** User can list, edit, delete memories

### Agent Memory Tools

| Tool | Purpose |
|------|---------|
| `save_memory` | Save a new memory (type, scope, content) |
| `search_memory` | Search memories by keyword |
| `delete_memory` | Remove a memory by ID |
| `list_memories` | List all active memories for current context |

---

## Project Context (chitty.md)

Automatically discovered project-specific instructions. When a user chats within a project directory, Chitty loads the `chitty.md` file and injects it into the system prompt.

### Discovery Order

1. `<project>/.chitty/chitty.md` (hidden directory)
2. `<project>/chitty.md` (root level)

### What Goes in chitty.md

- Project overview and tech stack
- Coding conventions and patterns
- Important files and their purposes
- Build/run/test commands
- Special instructions for the AI

### Generation

The Skills Builder can auto-generate a `chitty.md` by scanning the project directory (file structure, package files, READMEs) using the user's BYOK key.

---

## Context Assembly

Every LLM call assembles context in this order:

```
┌─────────────────────────────────────────┐
│ 1. System Prompt (from skill or default)│
├─────────────────────────────────────────┤
│ 2. Project Context (chitty.md)          │
├─────────────────────────────────────────┤
│ 3. Active Memories (global + scoped)    │
├─────────────────────────────────────────┤
│ 4. Tool Definitions                     │
├─────────────────────────────────────────┤
│ 5. Conversation History (trimmed)       │
├─────────────────────────────────────────┤
│ 6. User Message                         │
└─────────────────────────────────────────┘
```

Context window management: when history exceeds the model's context window, older messages are summarized or trimmed while preserving tool call/result pairs.

---

## Data Storage

All data is local:

```
~/.chitty-workspace/
├── config.toml              # App settings (TOML)
├── workspace.db             # SQLite database
│   ├── conversations        # Chat sessions (title, skill, project, provider, model)
│   ├── messages             # Full message history (with tool calls, parent linking)
│   ├── memories             # Persistent memories (typed, scoped, searchable)
│   ├── skills               # Saved skill definitions
│   ├── custom_tools         # User/AI-generated tools
│   └── provider_configs     # Provider settings (keys in OS keyring)
└── models/                  # GGUF model files (for HuggingFace sidecar)
```

### SQLite Tables

| Table | Purpose | Key Fields |
|-------|---------|------------|
| `conversations` | Chat sessions | id, title, skill_id, project_path, provider, model |
| `messages` | Message history | id, conversation_id, parent_message_id, role, content, tool_calls |
| `memories` | Persistent memory | id, memory_type, name, content, scope, scope_ref |
| `skills` | Skill definitions | id, name, instructions, tools, project_path |
| `custom_tools` | Custom tools | id, name, description, instructions, parameters, script_body |
| `provider_configs` | Provider settings | id, provider_id, display_name, base_url, enabled |

API keys are stored in the **OS keyring** (Windows Credential Manager), not in SQLite.

## Code Style

- Rust 2021 edition
- Use `anyhow::Result` for application errors, `thiserror` for library errors
- Async everywhere (tokio runtime)
- Serde for all serialization
- Keep modules focused — one responsibility per module

## Relationship to DataVisions

Chitty Workspace is a **completely separate product** from DataVisions Enterprise.
- Does NOT connect to the DataVisions cloud platform
- Does NOT share code with Chitty Bridge Service
- Shares architectural inspiration but is an independent codebase
- Free product that promotes DataVisions Enterprise for teams needing governance, DataHub, multi-tenancy, etc.
