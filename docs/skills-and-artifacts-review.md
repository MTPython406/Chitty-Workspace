# Skills & Artifacts — Chitty Workspace Architecture Review

**Date:** March 21, 2026
**Status:** Active design document

---

## The Simplified Model

```
CURRENT:
  Agent = instructions + tools[] + config
  Marketplace Package = tools + setup + agent_config
  Output = text in chat

PROPOSED:
  Skill = instructions + tools + scripts + references  (the composable unit)
  Agent = persona + skills[] + config                  (simplified)
  Marketplace Package = skills + custom tools + integrations  (container)
  Artifact = rendered preview                          (images, charts, HTML, code, video)
```

**Three changes:**
1. **Skills become first-class.** They bundle instructions AND tools together. They follow the open Agent Skills standard (agentskills.io).
2. **Agents get simpler.** An agent is just a persona (who it is) + which skills it has (what it can do) + execution config. No more separate tool selection.
3. **Artifacts are rendered previews.** When the agent produces rich content (charts, HTML apps, images, code, video), it renders in a preview panel alongside chat. Like Claude.ai artifacts.

---

## Part 1: Skills

### What a Skill Is

A skill is a folder containing a `SKILL.md` file. It bundles:

- **Instructions** — What the agent should know and how to approach work
- **Tool requirements** — Which tools this skill needs (file_writer, terminal, browser, etc.)
- **Scripts** — Reusable code the agent can execute
- **References** — Detailed docs loaded on demand
- **Assets** — Templates, schemas, examples

```
web-app-builder/
├── SKILL.md              # Instructions + metadata (required)
├── scripts/
│   └── bundle.py         # Reusable script
├── references/
│   └── design-patterns.md
└── assets/
    └── starter.html      # Template
```

The `SKILL.md` file:

```yaml
---
name: web-app-builder
description: >
  Build modern single-page HTML applications and dashboards.
  Use when the user asks to create a web app, dashboard, tool,
  calculator, or interactive prototype.
allowed-tools: file_writer file_reader terminal browser
---

# Web App Builder

## Design Rules
- Clean, asymmetric, content-first layouts
- Dark mode by default
- System fonts or modern sans-serif stacks
- No purple gradients, no excessive rounded corners

## Workflow
1. Create project directory in artifacts path
2. Build self-contained HTML (inline CSS + JS, no build tools)
3. Use CDN imports for React/Tailwind if needed
4. Test by opening in browser tool
5. Register as artifact for preview

## Gotchas
- Chitty runs primarily on Windows — use PowerShell-compatible commands
- No Node.js guaranteed — avoid npm/build steps in v1
- Keep everything in a single HTML file when possible
```

**Key insight:** The `allowed-tools` field from the open standard maps directly to what Chitty's agents currently do with `tools: Vec<String>`. When an agent activates a skill, those tools become available. The skill bundles the expertise AND the capabilities together.

### How Skills Replace Tool Selection on Agents

**Current agent model:**
```
Agent "Frontend Developer"
├── instructions: "You are a frontend developer..."  (monolithic blob)
├── tools: [file_reader, file_writer, terminal, browser, code_search]
├── preferred_provider: "anthropic"
├── preferred_model: "claude-sonnet-4-20250514"
├── max_iterations: 25
└── approval_mode: "prompt"
```

**New agent model:**
```
Agent "Frontend Developer"
├── persona: "You are a senior frontend developer focused on clean, accessible UI."
├── skills: [web-app-builder, react-standards, accessibility-checker]
├── preferred_provider: "anthropic"
├── preferred_model: "claude-sonnet-4-20250514"
├── max_iterations: 25
└── approval_mode: "prompt"
```

The agent no longer needs a giant instructions field or manual tool selection. Skills bring both:
- `web-app-builder` → brings file_writer, file_reader, terminal, browser + instructions for building apps
- `react-standards` → brings file_reader, code_search + instructions for React conventions
- `accessibility-checker` → brings browser, terminal + instructions for a11y auditing

Tools are **unioned** across all active skills. If two skills both need `terminal`, it's available once.

The `persona` field replaces `instructions` — it's just who the agent IS, not what it knows. Domain expertise lives in skills.

### Progressive Loading (From the Standard)

Skills load in three tiers:

**Tier 1 — Catalog (always loaded, ~50-100 tokens per skill):**

At conversation start, the agent sees a list of available skills:

```xml
<available_skills>
  <skill>
    <name>web-app-builder</name>
    <description>Build modern single-page HTML applications and dashboards. Use when the user asks to create a web app, dashboard, tool, calculator, or interactive prototype.</description>
  </skill>
  <skill>
    <name>react-standards</name>
    <description>React coding standards, component patterns, and testing conventions. Use when writing or reviewing React code.</description>
  </skill>
</available_skills>
```

**Tier 2 — Instructions (loaded when activated, <5K tokens):**

When the agent decides a skill is relevant (or the user types `/skill web-app-builder`), it calls `load_skill`. The full SKILL.md body loads into context, wrapped in tags:

```xml
<skill_content name="web-app-builder">
# Web App Builder
...full instructions...

Skill directory: ~/.chitty-workspace/skills/web-app-builder

<skill_resources>
  <file>scripts/bundle.py</file>
  <file>references/design-patterns.md</file>
  <file>assets/starter.html</file>
</skill_resources>
</skill_content>
```

**Tier 3 — Resources (loaded as needed):**

If the instructions reference `scripts/bundle.py` or `references/design-patterns.md`, the agent uses file_reader or terminal to access them. Only loaded when actually needed.

### Context Assembly (Updated)

```
1. Agent persona (short — who the agent is)
2. Project context (chitty.md — project-specific conventions)
3. Memories (global + project + agent scoped)
4. Skill catalog (available skills — names + descriptions)         ← NEW
5. Active skill instructions (loaded via load_skill tool)          ← NEW
6. Tool definitions (unioned from active skills, OpenAI format)    ← CHANGED
7. Conversation messages
```

The big change: tool definitions at step 6 are no longer manually selected. They're determined by which skills are active. If no skills are loaded yet, the agent gets the base tools (file_reader, terminal, save_memory, load_skill). When skills activate, their required tools become available.

### Skill Discovery Paths

Following the open standard for cross-client compatibility:

| Scope | Path | Purpose |
|-------|------|---------|
| **Project** | `<project>/.agents/skills/` | Cross-client standard (Claude Code, Cursor, VS Code, etc.) |
| **Project** | `<project>/.chitty/skills/` | Chitty-specific |
| **User** | `~/.agents/skills/` | Cross-client standard |
| **User** | `~/.chitty-workspace/skills/` | Chitty-specific |
| **Marketplace** | `~/.chitty-workspace/tools/marketplace/*/` | Skills inside marketplace packages |
| **Built-in** | (compiled into binary) | Ships with Chitty |

**Precedence:** Project overrides User. First-found wins within same scope. Log warnings on name collisions.

**Trust:** Project-level skills from unfamiliar repos require user confirmation before loading.

### Custom Skills

Users can create skills three ways:

1. **Drop a folder** — Create a SKILL.md in any discovery path. Chitty finds it automatically.
2. **Skill Builder UI** — In the Agent Builder, a "Create Skill" option that walks through name, description, instructions, tool selection, and optionally generates scripts.
3. **AI-generated** — Ask Chitty: "Create a skill for reviewing Python code." Chitty writes the SKILL.md and saves it to `~/.chitty-workspace/skills/`.

### Open Standard Compliance

The Agent Skills standard (agentskills.io) is adopted by 30+ platforms. Chitty follows it exactly:

| Standard Requirement | Chitty Implementation |
|---------------------|----------------------|
| SKILL.md with YAML frontmatter | Full support — name, description, license, compatibility, metadata, allowed-tools |
| Progressive disclosure (3-tier loading) | Catalog in system prompt → load_skill tool → file_reader for resources |
| Portable format | Skills from Claude Code/Cursor/VS Code work in Chitty, and vice versa |
| Cross-client discovery path (`.agents/skills/`) | Scanned at both project and user level |
| Lenient YAML parsing | Warn on minor issues, skip only if description is missing |

**For Chitty's open source community:**

- Publish Chitty's built-in skills to GitHub using the standard format
- Any skill from the community (github.com/anthropics/skills or third-party) works by dropping into `.agents/skills/` or `.chitty/skills/`
- Chitty marketplace skills use the same SKILL.md format + a package.json wrapper for marketplace metadata (icon, vendor, categories, setup steps)
- Skills created in Chitty are portable to any other platform that supports the standard

---

## Part 2: Marketplace Packages (Updated)

### What a Package Contains Now

A marketplace package is a **container** that can hold any combination of:

| Component | Required? | Example |
|-----------|-----------|---------|
| **Skills** | At least one | SKILL.md with instructions + tool requirements |
| **Custom Tools** | Optional | Python/Node/PowerShell scripts with manifest.json |
| **Integrations** | Optional | API connections, OAuth configs, credential setup |
| **Setup Steps** | Optional | Installation wizard (check/install commands) |

### Package Types

**Skill-only package** (simplest):
```
web-app-builder/
├── package.json          # Marketplace metadata
└── SKILL.md              # Just instructions
```

**Skill + Tools package** (current pattern, enhanced):
```
google-cloud/
├── package.json          # Marketplace metadata + setup steps + auth
├── SKILL.md              # Instructions for using Google Cloud via Chitty
├── bigquery/
│   ├── manifest.json     # Tool definition
│   └── tool.py           # Executable tool script
└── cloud-storage/
    ├── manifest.json
    └── tool.py
```

**Integration package:**
```
slack-integration/
├── package.json          # OAuth config + setup
├── SKILL.md              # Instructions for using Slack
└── slack-send/
    ├── manifest.json
    └── tool.js
```

### How Existing Packages Change

The three existing marketplace packages (google-cloud, web-tools, social-media) each get a `SKILL.md` added to them:

- **google-cloud/SKILL.md** — Instructions for BigQuery querying, Cloud Storage management, when to use which tool, gotchas about auth and quotas
- **web-tools/SKILL.md** — Instructions for web search strategies, scraping best practices, handling rate limits
- **social-media/SKILL.md** — Instructions for posting, thread management, media handling

The `agent_config.default_instructions` field in `package.json` migrates into the SKILL.md. This is cleaner — instructions live in a standard portable format instead of buried in JSON.

### Discovery Flow

```
On startup:
1. Scan marketplace directory tree
2. For each package:
   a. Parse package.json (metadata, auth, setup)
   b. If SKILL.md exists → parse frontmatter, add to skill catalog
   c. For each tool subdirectory → parse manifest.json, register tool
   d. Store as unified package (skills + tools + integrations)
3. Scan .agents/skills/ and .chitty/skills/ for standalone skills
4. Merge all into unified skill catalog + tool registry
```

### Agent ↔ Skill ↔ Package Relationship

```
Agent "Data Analyst"
  ├── persona: "You analyze data and create visualizations."
  ├── skills:
  │   ├── google-cloud (from marketplace package)
  │   │   ├── SKILL.md instructions loaded on activation
  │   │   └── brings tools: bigquery, cloud-storage
  │   ├── data-viz (from .agents/skills/ — standalone)
  │   │   ├── SKILL.md instructions loaded on activation
  │   │   └── brings tools: file_writer, terminal
  │   └── web-tools (from marketplace package)
  │       ├── SKILL.md instructions loaded on activation
  │       └── brings tools: web_search, web_scraper
  └── config: { max_iterations: 15, approval_mode: "prompt" }
```

The agent doesn't know or care whether a skill comes from a marketplace package or a standalone folder. It just sees skills.

---

## Part 3: Artifacts (Rendered Previews)

### What Artifacts Are

Artifacts are **rich content previews** that appear alongside chat when the agent produces something visual or interactive. They are NOT managed applications — they're rendered outputs.

| Content Type | Preview |
|-------------|---------|
| **HTML/Web App** | Live iframe preview (sandboxed) |
| **Chart/Visualization** | Rendered image or interactive chart |
| **Image** | Displayed inline or in preview panel |
| **Code** | Syntax-highlighted code block with copy button |
| **Markdown/Report** | Rendered markdown document |
| **Video** | Video player (when using vision/video models) |
| **Data Table** | Formatted table view |

### How Artifacts Work

When the agent produces significant content, it wraps it as an artifact using a convention in its response:

```
Here's your sales dashboard:

<artifact type="html" title="Sales Dashboard">
<!DOCTYPE html>
<html>
<head>
  <script src="https://cdn.tailwindcss.com"></script>
</head>
<body class="bg-gray-900 text-white p-8">
  <h1 class="text-2xl font-bold">Sales Dashboard</h1>
  ...full HTML content...
</body>
</html>
</artifact>
```

The frontend detects `<artifact>` blocks in the assistant's response and renders them in the **Artifact Preview panel** (right side, in the Action Panel area).

This is a **frontend convention** — no new native tools needed. The agent learns to use `<artifact>` tags from its skill instructions (e.g., the web-app-builder skill teaches the agent to wrap outputs in artifact tags).

### Artifact Types and Rendering

| Type Attribute | How It Renders | Sandbox |
|---------------|----------------|---------|
| `type="html"` | Sandboxed iframe (`sandbox="allow-scripts"`) | Yes — no access to Chitty APIs |
| `type="code"` | Syntax-highlighted code block | No |
| `type="markdown"` | Rendered markdown | No |
| `type="image"` | `<img>` tag (base64 or file path) | No |
| `type="svg"` | Inline SVG render | No |
| `type="chart"` | Rendered via simple charting (or iframe if HTML-based) | Yes |
| `type="video"` | Video player element | No |

### Frontend Implementation

The artifact viewer lives in the **Dynamic View** area of the Action Panel (same slot as browser tool):

```
┌─ Chat Panel ──────────────┐  ┌─ Action Panel ─────────────┐
│                            │  │ [Activity][Agents][Providers]│
│ User: Build me a dashboard│  │                              │
│                            │  │ ┌──────────────────────────┐ │
│ Agent: Here's your         │  │ │  Sales Dashboard    v1   │ │
│ dashboard:                 │  │ │  ───────────────────────  │ │
│                            │  │ │                          │ │
│ [📊 Sales Dashboard]  ←link│  │ │  (live iframe preview)   │ │
│                            │  │ │                          │ │
│ I've created a responsive  │  │ │                          │ │
│ dashboard with...          │  │ └──────────────────────────┘ │
│                            │  │ [Code] [Copy] [Download]     │
│ User: Add a date filter    │  │                              │
│                            │  │ Iterate:                     │
│ Agent: Updated!            │  │ [________________] [Send]    │
│                            │  │                              │
└────────────────────────────┘  └──────────────────────────────┘
```

**Key UI elements:**

1. **Artifact link in chat** — Where the `<artifact>` tag appears, the chat shows a clickable card/link. Clicking it opens the preview in the Action Panel.

2. **Preview panel** — Renders the artifact content. For HTML: sandboxed iframe. For code: syntax-highlighted block. For images: displayed directly.

3. **Action buttons:**
   - **[Code]** — Toggle between preview and raw code view
   - **[Copy]** — Copy the artifact content to clipboard
   - **[Download]** — Save as file (HTML, PNG, etc.)

4. **Iterate bar** — Text input at the bottom. User types a change request, it sends a message to the active chat panel with the artifact context. Agent modifies and produces a new artifact version. The preview updates.

### Artifact Versioning (Lightweight)

Artifacts version within a conversation. Each time the agent produces a new `<artifact>` with the same title, it's a new version. The frontend tracks:

```javascript
// In panel state
panel.artifacts = [
  {
    title: "Sales Dashboard",
    type: "html",
    versions: [
      { content: "...v1 html...", messageId: "msg-1" },
      { content: "...v2 html...", messageId: "msg-5" }
    ],
    currentVersion: 1  // 0-indexed
  }
]
```

A version dropdown lets the user switch between versions. No database storage needed — artifacts live in the conversation messages. They can be downloaded/exported, but they're not persisted separately.

### How Skills Teach Artifact Creation

Skills include instructions on how to format outputs as artifacts. The `web-app-builder` skill would include:

```markdown
## Output Format

When you've built the application, wrap the complete HTML in an artifact tag:

<example>
<artifact type="html" title="App Name">
<!DOCTYPE html>
...complete self-contained HTML...
</artifact>
</example>

This will render a live preview in the Artifact panel. The user can iterate
by describing changes, and you produce an updated artifact with the same title.
```

This means artifact creation is **skill-driven**, not tool-driven. Different skills teach different artifact patterns:
- web-app-builder → `type="html"` artifacts
- data-viz → `type="svg"` or `type="html"` chart artifacts
- code-review → `type="code"` artifacts with annotated snippets
- report-writer → `type="markdown"` artifacts

### What Artifacts Are NOT

- NOT persisted outside the conversation (they live in chat messages)
- NOT managed applications (no start/stop/process management)
- NOT registered in a database (no artifact table needed)
- NOT separate from the chat flow (they're part of the response)

They're simply a **rendering convention** — when the agent produces content wrapped in `<artifact>` tags, the frontend renders it richly instead of showing raw text.

---

## Part 4: What Changes in Chitty's Codebase

### Agent Struct (Simplified)

```rust
pub struct Agent {
    pub id: String,
    pub name: String,
    pub description: String,
    pub persona: String,               // Was: instructions (now shorter, just persona)
    pub skills: Vec<String>,           // Was: tools (now references skills, not tools)
    pub project_path: Option<String>,
    pub preferred_provider: Option<String>,
    pub preferred_model: Option<String>,
    pub tags: Vec<String>,
    pub version: String,
    pub ai_generated: bool,
    pub max_iterations: Option<u32>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub approval_mode: String,
}
```

The `instructions` field becomes `persona` (shorter, just identity). The `tools` field becomes `skills` (skills bring their own tools).

### Skill Struct (New)

```rust
pub struct Skill {
    pub name: String,                  // From SKILL.md frontmatter
    pub description: String,           // From SKILL.md frontmatter
    pub allowed_tools: Vec<String>,    // From allowed-tools field
    pub skill_path: PathBuf,           // Path to SKILL.md
    pub source: SkillSource,           // Marketplace, Project, User, BuiltIn
    pub compatibility: Option<String>, // Environment requirements
    pub license: Option<String>,
    pub metadata: HashMap<String, String>,
}

pub enum SkillSource {
    Marketplace { vendor: String, package: String },
    Project,     // From .agents/skills/ or .chitty/skills/ in project
    User,        // From ~/.agents/skills/ or ~/.chitty-workspace/skills/
    BuiltIn,     // Compiled into binary
}
```

### Context Assembly (New Flow)

```rust
pub fn assemble_context(...) -> Result<(AssembledContext, ExecutionConfig)> {
    // 1. Agent persona (short)
    let persona = agent.persona;

    // 2. Project context (chitty.md)
    let project_ctx = load_project_context(project_path);

    // 3. Memories
    let memories = load_relevant(conn, project_path, agent_id);

    // 4. Skill catalog (all available skills — metadata only)
    let catalog = build_skill_catalog(&available_skills, &agent.skills);

    // 5. Tool definitions — union of tools required by agent's skills
    let required_tools = union_skill_tools(&agent.skills, &skill_registry);
    // Always include base tools: load_skill, save_memory, file_reader
    let tools = base_tools.union(required_tools);

    // 6. Conversation messages
    let messages = load_messages(conn, conversation_id);

    // Build system prompt
    let system_prompt = format!(
        "{persona}\n\n{project_ctx}\n\n{memories}\n\n{catalog}\n\n{tool_instructions}"
    );
}
```

### Frontend Changes

| Component | Change |
|-----------|--------|
| **Artifact rendering** | Parse `<artifact>` tags in assistant messages. Render in Dynamic View. |
| **Artifact link in chat** | Replace `<artifact>` block in chat with clickable card |
| **Preview panel** | Sandboxed iframe for HTML, syntax highlight for code, img for images |
| **Iterate bar** | Text input in artifact preview, sends message to chat panel |
| **Version toggle** | Dropdown when multiple artifacts share the same title |
| **Agent Builder** | Replace tool checkboxes with skill selection |
| **Skill Builder** | New UI for creating custom skills (name, description, instructions, tools) |
| **Marketplace** | Filter by Skills vs. Tool Packages vs. Integrations |

### Migration

| What | How |
|------|-----|
| `agents.instructions` → `agents.persona` | Rename column in migration V8 |
| `agents.tools` → `agents.skills` | Rename column. Existing agents keep working — their tool lists become temporary "legacy skills" until migrated |
| `agent_config.default_instructions` → `SKILL.md` | For each marketplace package, extract instructions into a SKILL.md file |
| `tool.instructions` | Stay as-is — tool-level instructions still auto-inject (they're per-tool, not per-skill) |

---

## Part 5: Implementation Roadmap

### Phase 1 — Skills Foundation (2 weeks)

- [ ] `Skill` struct and SKILL.md parser (YAML frontmatter extraction)
- [ ] Skill discovery: scan marketplace + .agents/skills/ + .chitty/skills/ + ~/.agents/skills/
- [ ] `load_skill` native tool
- [ ] Skill catalog injection in context assembly
- [ ] Tool unioning from active skills
- [ ] Agent struct: `instructions` → `persona`, `tools` → `skills`
- [ ] SQLite migration V8 (rename columns)
- [ ] Ship 2-3 built-in skills (web-app-builder, project-scanner)

### Phase 2 — Agent Builder + Marketplace Updates (2 weeks)

- [ ] Agent Builder UI: skill selection replaces tool checkboxes
- [ ] Skill Builder UI: create custom skills
- [ ] Marketplace: skill category filter
- [ ] Package SKILL.md: add SKILL.md to existing packages (google-cloud, web-tools, social-media)
- [ ] `/skill <name>` slash command for user-explicit activation
- [ ] Trust prompting for project-level skills

### Phase 3 — Artifacts (2-3 weeks)

- [ ] Frontend: parse `<artifact>` tags in assistant responses
- [ ] Artifact preview in Dynamic View (iframe for HTML, syntax highlight for code)
- [ ] Artifact link card in chat (clickable)
- [ ] Code/Preview toggle
- [ ] Copy + Download buttons
- [ ] Iterate bar (sends modification request to chat)
- [ ] Version tracking within conversation
- [ ] Skills teach artifact patterns (update built-in skills)

### Phase 4 — Polish + Community (1-2 weeks)

- [ ] Publish built-in skills to GitHub in standard format
- [ ] Cross-client skill compatibility testing (Claude Code, Cursor)
- [ ] Skill eval framework (evals.json for testing built-in skills)
- [ ] Documentation: how to create and share skills

### Total: 7-9 weeks

---

## Part 6: Summary — What Goes Where

| "I want to..." | Use |
|-----------------|-----|
| Give an agent domain expertise | **Skill** (SKILL.md with instructions + tool requirements) |
| Define who an agent is | **Agent persona** (short identity text) |
| Add executable capabilities | **Custom tools** in marketplace packages or standalone |
| Set project-specific conventions | **chitty.md** |
| Remember user preferences | **Memories** |
| Preview a web app the agent built | **Artifact** (HTML rendered in iframe) |
| Preview a chart or visualization | **Artifact** (SVG/HTML rendered in preview panel) |
| View generated code | **Artifact** (syntax-highlighted code block) |
| Iterate on agent output | **Artifact iterate bar** (sends changes back to chat) |
| Share skills with the community | **Open standard** (SKILL.md format, portable to 30+ platforms) |
| Browse and install capabilities | **Marketplace** (packages containing skills + tools + integrations) |

### The Key Relationships

```
Marketplace Package
├── SKILL.md          → loaded into agent context on activation
├── Custom Tools      → available when skill is active
├── Integrations      → API connections, OAuth
└── Setup Steps       → installation wizard

Agent
├── Persona           → "You are a data analyst..."
├── Skills[]          → [google-cloud, data-viz, web-tools]
│   └── each skill brings its own tools
└── Config            → iterations, temperature, approval

Artifact
├── Created by        → agent response with <artifact> tags
├── Rendered in       → Action Panel (Dynamic View)
├── Types             → html, code, markdown, image, svg, chart, video
└── Iterable          → user requests changes, agent updates
```
