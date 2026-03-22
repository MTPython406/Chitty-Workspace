# Chitty Workspace

A local-first AI assistant with agents, tools, marketplace packages, and bring-your-own-key (BYOK) provider support. Runs entirely on your machine — no cloud server required.

## Documentation

- [Application Feature List and Usage Guide](docs/application-feature-list.md)

## What is Chitty Workspace?

Chitty Workspace is a standalone desktop AI assistant that gives you full control over your AI experience:

- **Default Chitty Agent** — Built-in system administrator that knows your entire setup — tools, packages, providers, models, and can help you build anything
- **Agent System** — Create specialized AI agents with custom instructions, tool sets, and execution configs
- **Agent Builder** — AI-powered agent creation — describe what you need, Chitty builds it
- **Marketplace Packages** — Install or build tool packages (web scraping, Google Cloud, social media, and more)
- **Browser Automation** — Control your real Chrome/Edge browser via the Chitty Browser Extension
- **Slash Commands** — `/schedule`, `/help`, and more — extensible command system
- **Agent Scheduler** — Schedule agents to run tasks autonomously on cron expressions
- **Native Tools** — File operations, terminal commands, code search built into the binary
- **Persistent Memory** — Chitty remembers your preferences, project context, and feedback across sessions
- **Project Context** — Drop a `chitty.md` in any project and Chitty automatically understands it
- **Multi-Panel** — Run multiple agents side-by-side, Chitty can delegate tasks between panels
- **Session Auto-Approve** — Approve once, auto-approve the rest of the session for smooth workflows
- **BYOK Providers** — Use your own API keys for OpenAI, Anthropic, Google, xAI, or run models locally with Ollama
- **Cross-Platform Terminal** — PowerShell (Windows), zsh (macOS), sh (Linux) — works everywhere
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
# Start Chitty Workspace (default — opens system tray + WebView2 UI)
chitty-workspace

# Show configuration
chitty-workspace config

# List installed agents
chitty-workspace agents
```

## Architecture

```
┌──────────────────────────────────────────────────┐
│              Chitty Workspace                     │
├──────────────────────────────────────────────────┤
│  UI (WebView2 + System Tray)                      │
│  ├── Multi-panel chat interface                   │
│  ├── Dynamic Action Panel (browser, media, config)│
│  ├── Agent Builder & Marketplace                  │
│  └── Settings (providers, keys, integrations)     │
├──────────────────────────────────────────────────┤
│  Chat Engine                                      │
│  ├── Context assembly (agent + chitty.md +        │
│  │   memories + tools + history)                  │
│  ├── Tool calling loop with approval system       │
│  ├── Streaming SSE responses                      │
│  └── Slash command system (/schedule, /help)       │
├──────────────────────────────────────────────────┤
│  Agents            │  Providers (BYOK)            │
│  ├── Chitty (sys)  │  ├── OpenAI                  │
│  ├── Custom agents │  ├── Anthropic               │
│  └── Agent Builder │  ├── Google AI               │
│                    │  ├── xAI                      │
│  Tools             │  ├── Ollama (local)           │
│  ├── Native        │  └── HuggingFace (sidecar)   │
│  ├── Custom        │                               │
│  ├── Marketplace   │  Scheduler                    │
│  └── Browser ext.  │  └── Cron-based agent tasks   │
├──────────────────────────────────────────────────┤
│  SQLite (local)    │  OS Keyring (API keys)        │
└──────────────────────────────────────────────────┘
```

## Core Concepts

### Agents

An Agent is the central unit of Chitty Workspace. It combines:

- **Instructions** — System prompt that defines the agent's role and behavior
- **Tools** — Which tools the agent can use (empty = all tools)
- **Execution Config** — Max iterations, temperature, max tokens, approval mode
- **Preferences** — Preferred provider/model, project scope

The default **Chitty** agent is a system administrator that understands the entire platform — tools, packages, providers, models, scheduling, and troubleshooting.

### Marketplace Packages

Packages are bundles of tools that extend Chitty's capabilities. Built-in packages include:

| Package | Tools | Description |
|---------|-------|-------------|
| **web-tools** | web_search, web_scraper | Search the web and scrape websites |
| **google-cloud** | gcloud_bigquery, cloud_storage | BigQuery queries and Cloud Storage |
| **social-media** | x_twitter | Post and search on X/Twitter |

See [Building Packages](#building-marketplace-packages) below to create your own.

### Slash Commands

Type `/` in the chat to use commands:

| Command | Description |
|---------|-------------|
| `/schedule` | Create a new scheduled agent task |
| `/schedules` | List all scheduled tasks |
| `/help` | Show available commands |

### Agent Scheduler

Schedule agents to run tasks autonomously:

```
/schedule
→ Agent: [Personal Assistant]
→ Task: "Check my email and calendar, give me a morning briefing"
→ Schedule: Weekdays at 9:00 AM
→ Create Schedule ✓
```

Scheduled tasks run in the background with auto-approve enabled. View and manage with `/schedules`.

### Browser Automation

The Chitty Browser Extension gives agents full control of your real Chrome/Edge browser:

- **Navigate** — Open any URL
- **Click & Type** — Interact with page elements
- **Screenshot** — Capture page state
- **Read Text** — Extract content from pages
- **Execute JS** — Run JavaScript on pages

Your login sessions are available — agents can check Gmail, LinkedIn, GitHub, etc. using your existing sessions.

### Approval System

Sensitive actions (terminal commands, file writes, browser interactions) require user approval:

- **Allow once** — Approve this single action
- **Always allow for session** — Auto-approve all remaining actions this session
- **Deny** — Reject the action

Agents can also be configured with `approval_mode: "auto"` to skip approval entirely.

### Memory System

Chitty remembers what matters across conversations:

| Type | Purpose | Scope |
|------|---------|-------|
| **User** | Preferences, expertise | Global |
| **Feedback** | Corrections you've given | Global / Project |
| **Project** | Project decisions and context | Project |
| **Reference** | Pointers to external docs | Any |

### Project Context (chitty.md)

Drop a `chitty.md` (or `.chitty/chitty.md`) in any project directory. Chitty automatically loads it as context:

```markdown
# MyProject
## Tech Stack
React 18, TypeScript, Vite
## How to Run
npm run dev
## Conventions
- Use functional components with hooks
- All API calls go through src/services/api.ts
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

---

## Building Marketplace Packages

Marketplace packages let you extend Chitty with custom tools. Each package is a directory with a `package.json` manifest and one or more tool directories.

### Package Structure

```
~/.chitty-workspace/tools/marketplace/
└── my-package/
    ├── package.json              # Package manifest
    ├── my-tool/
    │   ├── manifest.json         # Tool definition
    │   └── tool.py               # Tool script (Python, Node, PowerShell, or Shell)
    └── another-tool/
        ├── manifest.json
        └── tool.js
```

### package.json

```json
{
  "name": "my-package",
  "display_name": "My Custom Package",
  "vendor": "YourName",
  "description": "A package that does something useful",
  "version": "1.0.0",
  "icon": "wrench",
  "color": "#8b5cf6",
  "tools": ["my-tool", "another-tool"],
  "setup_steps": [
    {
      "id": "install_deps",
      "label": "Install Python dependencies",
      "check_command": "pip show requests",
      "install_command": "pip install requests",
      "help_text": "Installs the requests library for HTTP calls.",
      "required": true
    }
  ]
}
```

### Tool manifest.json

```json
{
  "name": "my-tool",
  "display_name": "My Tool",
  "description": "Does something useful with a URL",
  "runtime": "python",
  "timeout_seconds": 30,
  "parameters": {
    "type": "object",
    "properties": {
      "url": {
        "type": "string",
        "description": "The URL to process"
      },
      "format": {
        "type": "string",
        "description": "Output format (json or text)",
        "enum": ["json", "text"]
      }
    },
    "required": ["url"]
  },
  "instructions": "Use my-tool when the user asks to process a URL. Pass the URL and optional format parameter."
}
```

### Tool Script

Scripts receive JSON on stdin and must output JSON to stdout:

**Python (tool.py):**
```python
import sys
import json

input_data = json.loads(sys.stdin.read())
params = input_data.get("parameters", {})
url = params.get("url", "")

# Do your work here
result = {"processed": url, "status": "ok"}

print(json.dumps({"success": True, "result": result}))
```

**Node.js (tool.js):**
```javascript
const input = JSON.parse(require('fs').readFileSync('/dev/stdin', 'utf8'));
const params = input.parameters || {};

const result = { processed: params.url, status: 'ok' };
console.log(JSON.stringify({ success: true, result }));
```

### Supported Runtimes

| Runtime | File Extension | Command |
|---------|---------------|---------|
| `python` | `.py` | `python tool.py` |
| `node` | `.js` | `node tool.js` |
| `powershell` | `.ps1` | `powershell -File tool.ps1` |
| `shell` | `.sh` | `sh tool.sh` |

### Setup Steps

Setup steps run when the package is first installed. Each step has:

| Field | Description |
|-------|-------------|
| `id` | Unique identifier |
| `label` | Human-readable description |
| `check_command` | Command to verify if already set up (exit 0 = done) |
| `install_command` | Command to run if check fails |
| `help_text` | Shown to user during setup |
| `required` | If true, package won't work without it |

### Tool Instructions

The `instructions` field in `manifest.json` is injected into the LLM's system prompt. Write clear guidance for when and how the agent should use your tool:

```
"instructions": "Use web_scraper to extract structured data from web pages. Pass the URL and specify what elements to extract (text, links, tables, or specific CSS selectors). Returns clean extracted content."
```

### Testing Your Tool

```bash
# Test directly from the command line
echo '{"parameters":{"url":"https://example.com"}}' | python my-tool/tool.py
```

### Publishing

Packages can be shared as directories or published to the [Chitty Marketplace](https://chitty.ai/marketplace) for others to install.

---

## Data Storage

Everything stays on your machine:

```
~/.chitty-workspace/
├── config.toml              # Settings
├── workspace.db             # SQLite database
│   ├── conversations        # Chat sessions
│   ├── messages             # Full message history
│   ├── agents               # Agent definitions
│   ├── memories             # Persistent memories
│   ├── scheduled_tasks      # Cron-based agent schedules
│   ├── custom_tools         # User/AI-generated tools
│   ├── token_usage          # Token tracking per conversation
│   └── provider_configs     # Provider settings
├── tools/
│   ├── marketplace/         # Installed marketplace packages
│   └── custom/              # User-created tools
└── models/                  # Local GGUF model files
```

## Local API

Chitty runs a local server at `http://localhost:8770` with a REST API:

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/agents` | GET/POST | List or create agents |
| `/api/agents/:id` | GET/PUT/DELETE | Manage a specific agent |
| `/api/schedules` | GET/POST | List or create scheduled tasks |
| `/api/schedules/:id` | GET/PUT/DELETE | Manage a scheduled task |
| `/api/schedules/:id/run` | POST | Manually trigger a task |
| `/api/tools` | GET | List all available tools |
| `/api/conversations` | GET | List conversations |
| `/api/providers` | GET | List configured providers |
| `/api/marketplace/packages` | GET | List installed packages |

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
- **Scheduling:** cron crate (expression parsing)
- **Secure Storage:** OS keyring (keyring crate)

## License

[MIT](LICENSE)

---

Built by [DataVisions](https://datavisions.ai) — Chitty Workspace is a free, open-source product. For enterprise teams needing governance, multi-tenancy, and DataHub, see [DataVisions Enterprise](https://datavisions.ai).
