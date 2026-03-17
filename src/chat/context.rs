//! Project context loader (chitty.md)
//!
//! Automatically discovers and loads `chitty.md` files from project directories.
//! These files provide project-specific instructions, conventions, and tool preferences
//! that are injected into the agent's system prompt.
//!
//! Similar to CLAUDE.md but for any project the user works in.
//! Users or the Skills Builder can generate/update chitty.md files.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Project context loaded from chitty.md
#[derive(Debug, Clone)]
pub struct ProjectContext {
    /// Path to the project directory
    pub project_path: PathBuf,
    /// Path to the chitty.md file
    pub file_path: PathBuf,
    /// Raw content of the chitty.md
    pub content: String,
}

impl ProjectContext {
    /// Format as context string for injection into system prompt
    pub fn as_system_context(&self) -> String {
        format!(
            "\n## Project Context (from {})\n\n{}\n",
            self.file_path.display(),
            self.content
        )
    }
}

/// Discover and load chitty.md from a project directory.
///
/// Search order (first found wins):
/// 1. `<project>/.chitty/chitty.md` (hidden directory, like .claude/)
/// 2. `<project>/chitty.md` (root level)
///
/// Returns None if no chitty.md is found.
pub fn load_project_context(project_path: &Path) -> Result<Option<ProjectContext>> {
    let candidates = [
        project_path.join(".chitty").join("chitty.md"),
        project_path.join("chitty.md"),
    ];

    for candidate in &candidates {
        if candidate.exists() {
            let content = std::fs::read_to_string(candidate)?;
            if !content.trim().is_empty() {
                return Ok(Some(ProjectContext {
                    project_path: project_path.to_path_buf(),
                    file_path: candidate.clone(),
                    content,
                }));
            }
        }
    }

    Ok(None)
}

/// Generate a starter chitty.md template for a project
pub fn generate_template(project_path: &Path) -> String {
    let project_name = project_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Project");

    format!(
        r#"# {} - Chitty Workspace Context

## Project Overview
<!-- Describe what this project does -->

## Tech Stack
<!-- List languages, frameworks, key dependencies -->

## Key Conventions
<!-- Coding style, naming conventions, patterns to follow -->

## Important Files
<!-- List the most important files and what they do -->

## How to Build & Run
<!-- Commands to build, test, and run this project -->

## Notes for the Agent
<!-- Any special instructions for the AI assistant -->
"#,
        project_name
    )
}
