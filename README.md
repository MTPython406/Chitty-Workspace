# Chitty Workspace

A local-first AI assistant with skills, tools, and bring-your-own-key (BYOK) provider support. Runs entirely on your machine — no cloud server required.

## What is Chitty Workspace?

Chitty Workspace is a standalone desktop AI assistant that gives you full control over your AI experience:

- **BYOK Providers** — Use your own API keys for OpenAI, Anthropic, Google, xAI, or run models locally with Ollama
- **Skills System** — Reusable skill packs (instructions + tools) that make the agent an expert at specific tasks
- **Skills Builder** — AI-powered tool and skill creation — describe what you need, Chitty builds it
- **Native Tools** — File operations, terminal commands, code search, and code analysis built into the binary
- **Persistent Memory** — Chitty remembers your preferences, project context, and feedback across sessions
- **Project Context** — Drop a `chitty.md` in any project and Chitty automatically understands it
- **100% Local** — All data stays on your machine in SQLite. API keys stored in OS keyring.

## Quick Start

### Install

Download the latest installer from [Releases](https://github.com/MTPython406/Chitty-Workspace/releases) and run it. Single binary, no dependencies.

### Build from Source

```bash
# Prerequisites: Rust toolchain (https://rustup.rs)
git clone https://github.com/MTPython406/Chitty-Workspace.git
cd Chitty-Workspace
cargo build --release
```

The binary will be at `target/release/chitty-workspace.exe`.

### Run

```bash
# Start Chitty Workspace (default)
chitty-workspace

# Show configuration
chitty-workspace config

# List installed skills
chitty-workspace skills
```

## Architecture

```
┌──────────────────────────────────────────────────┐
│              Chitty Workspace                     │
├──────────────────────────────────────────────────┤
│  UI (WebView2 + System Tray)                      │
│  ├── Chat interface                               │
│  ├── Skill browser & builder                      │
│  └── Settings (providers, keys, projects)         │
├──────────────────────────────────────────────────┤
│  Chat Engine                                      │
│  ├── Context assembly (skill + chitty.md +        │
│  │   memories + tools + history)                  │
│  ├── Tool calling loop                            │
│  └── Streaming responses                          │
├──────────────────────────────────────────────────┤
│  Skills            │  Providers (BYOK)            │
│  ├── Instructions  │  ├── OpenAI                  │
│  ├── Tool sets     │  ├── Anthropic               │
│  └── AI-generated  │  ├── Google AI               │
│                    │  ├── xAI                      │
│  Tools             │  ├── Ollama (local)           │
│  ├── Native        │  └── HuggingFace (sidecar)   │
│  ├── Custom        │                               │
│  └── Integration   │  Memory System                │
│                    │  ├── User preferences          │
│                    │  ├── Feedback & corrections    │
│                    │  ├── Project context           │
│                    │  └── External references       │
├──────────────────────────────────────────────────┤
│  SQLite (local)    │  OS Keyring (API keys)        │
└──────────────────────────────────────────────────┘
```

## Core Concepts

### Skills

A Skill is the central unit of Chitty Workspace. It combines:

- **Instructions** — A system prompt that tells the agent what it can do and how to behave
- **Tools** — A set of tools the agent can use when the skill is active
- **Metadata** — Name, description, tags, preferred provider/model

Skills are savable, shareable as JSON files, and can be scoped globally or per-project.

**Example skills:**
- *Code Reviewer* — Instructions for thorough code review + file_reader + code_search tools
- *Data Analyst* — Instructions for data analysis + terminal (for Python/R) + file tools
- *DevOps Assistant* — Instructions for infrastructure work + terminal + custom deploy tools

### Skills Builder

Don't want to write tools manually? The Skills Builder uses your own BYOK key to generate tools and instructions from a natural language description:

> "I need a skill that can query my PostgreSQL database and generate CSV reports"

Chitty generates the tool definitions, parameter schemas, and instructions automatically.

### Memory System

Chitty remembers what matters across conversations:

| Type | Purpose | Example |
|------|---------|---------|
| **User** | Your role, preferences, expertise | "Prefers TypeScript, senior engineer" |
| **Feedback** | Corrections you've given | "Don't use any in TypeScript" |
| **Project** | Project decisions and context | "Migrating to gRPC by Q2" |
| **Reference** | Pointers to external docs | "CI docs at wiki/ci-setup" |

Memories are scoped — global, per-project, or per-skill — so the right context loads at the right time.

### Project Context (chitty.md)

Drop a `chitty.md` (or `.chitty/chitty.md`) in any project directory. When you chat within that project, Chitty automatically loads it as context:

```markdown
# MyProject - Chitty Context

## Tech Stack
React 18, TypeScript, Vite, TailwindCSS

## Conventions
- Use functional components with hooks
- All API calls go through src/services/api.ts
- Tests use Vitest + React Testing Library

## How to Run
npm run dev    # Start dev server on :5173
npm test       # Run tests
```

### Providers

Bring your own keys. Chitty supports:

| Provider | Type | Models |
|----------|------|--------|
| **OpenAI** | Cloud (BYOK) | GPT-4o, GPT-4o-mini, o1, o3 |
| **Anthropic** | Cloud (BYOK) | Claude Opus, Sonnet, Haiku |
| **Google AI** | Cloud (BYOK) | Gemini 2.5 Flash, Pro |
| **xAI** | Cloud (BYOK) | Grok 3, Grok 3 Mini |
| **Ollama** | Local | Llama, Qwen, Mistral, Phi, etc. |
| **HuggingFace** | Local (sidecar) | Any GGUF model |

API keys are stored in your OS keyring (Windows Credential Manager / macOS Keychain / Linux Secret Service), never in plain text.

## Data Storage

Everything stays on your machine:

```
~/.chitty-workspace/
├── config.toml         # Settings
├── workspace.db        # SQLite (conversations, memories, skills, tools)
└── models/             # Local GGUF model files
```

## Building the Installer

```bash
# 1. Build release binary
cargo build --release

# 2. Build installer (requires Inno Setup 6)
"C:\Users\pauls\AppData\Local\Programs\Inno Setup 6\ISCC.exe" installer/ChittyWorkspaceInstaller.iss

# Output: installer/output/ChittyWorkspace-Setup-0.1.0.exe
```

## Tech Stack

- **Language:** Rust (2021 edition)
- **Async Runtime:** Tokio
- **UI:** System tray (tray-icon + tao) + WebView2 (wry)
- **Storage:** SQLite (rusqlite, WAL mode)
- **HTTP:** reqwest (LLM API calls) + axum (local UI server)
- **Secure Storage:** OS keyring (keyring crate)

## License

[MIT](LICENSE)

---

Built by [DataVisions](https://datavisions.ai)
