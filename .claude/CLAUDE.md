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
│   │   └── mod.rs           # Chat engine, conversation management, tool call loop
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
│   │   └── mod.rs           # SQLite persistence
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
2. Message + skill instructions + tool definitions sent to LLM
3. LLM responds with text or tool calls
4. Tool calls executed locally, results sent back
5. LLM generates final response
6. Everything persisted to SQLite

## Data Storage

All data is local:
- **Config:** `~/.chitty-workspace/config.toml`
- **Database:** `~/.chitty-workspace/workspace.db` (SQLite)
- **API Keys:** OS keyring (Windows Credential Manager)
- **Models:** `~/.chitty-workspace/models/` (GGUF files)

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
