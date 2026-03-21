---
name: project-scanner
description: >
  Scan a project directory and generate or update a chitty.md project context
  file. Use when the user asks to scan a project, generate project context,
  create a chitty.md, or when starting work in an unfamiliar codebase.
allowed-tools: file_reader file_writer terminal code_search
---

# Project Scanner

## Purpose

Analyze a project directory to understand its structure, tech stack, conventions,
and key files. Generate or update a `.chitty/chitty.md` file that provides
project context to all future Chitty conversations.

## Workflow

1. **Scan directory structure** using terminal:
   - `ls` or `dir` for top-level files
   - Look for package.json, Cargo.toml, pyproject.toml, go.mod, etc.
   - Identify the tech stack from these files

2. **Read key files** using file_reader:
   - README.md (if exists)
   - Config files (package.json, Cargo.toml, etc.)
   - Entry points (src/main.*, src/index.*, app.*)

3. **Identify conventions** using code_search:
   - Naming patterns (camelCase, snake_case)
   - Directory organization (src/, lib/, tests/)
   - Test patterns (jest, pytest, cargo test)

4. **Generate chitty.md** using file_writer:
   - Create `.chitty/chitty.md` (or update if exists)
   - Follow the template below

## Template

```markdown
# {Project Name} - Chitty Workspace Context

## Project Overview
{What this project does, in 1-2 sentences}

## Tech Stack
{Languages, frameworks, key dependencies}

## Key Conventions
{Coding style, naming patterns, architectural patterns discovered}

## Important Files
{List the most important files and what they do}

## How to Build & Run
{Build and run commands discovered from config files}

## Notes for the Agent
{Any special instructions based on what was discovered}
```

## Gotchas

- Always check if `.chitty/chitty.md` already exists before overwriting
- If it exists, merge new discoveries rather than replacing
- On Windows, use PowerShell commands for directory listing
- Don't include node_modules, .git, build artifacts in the scan
- Keep the output concise — focus on what's useful for future conversations
