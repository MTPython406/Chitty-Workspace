//! Project context loader (chitty.md)
//!
//! Automatically discovers, loads, and generates `chitty.md` files for project directories.
//! These files provide project-specific instructions that are injected into the system prompt.
//!
//! Similar to CLAUDE.md but designed to be compact (~500 tokens) so local models
//! can work effectively. Detailed project knowledge comes from tool calls, not the prompt.
//!
//! Auto-generates chitty.md on first use by scanning the project directory.
//! Background-refreshes via LLM after conversations end.

use anyhow::Result;
use std::collections::HashMap;
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

// ---------------------------------------------------------------------------
// Skip patterns for directory scanning
// ---------------------------------------------------------------------------

const SKIP_DIRS: &[&str] = &[
    ".git", ".hg", ".svn", "node_modules", "__pycache__", ".mypy_cache",
    ".pytest_cache", "target", "dist", "build", ".next", ".nuxt",
    "venv", ".venv", "env", ".env", ".tox", "coverage",
    ".idea", ".vscode", ".vs", "bin", "obj",
    // Asset/media directories — not useful for project context
    "Public", "public", "images", "img", "icons", "fonts",
    "screenshots", "media", "assets/icons",
];

/// File extensions that are source code (used to prioritize listing)
const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "py", "js", "jsx", "ts", "tsx", "go", "java", "kt", "rb",
    "c", "h", "cpp", "cc", "hpp", "cs", "swift", "lua", "zig",
    "html", "css", "scss", "sql", "sh", "bat", "ps1",
    "toml", "yaml", "yml", "json", "xml", "md",
];

// ---------------------------------------------------------------------------
// Stack detection
// ---------------------------------------------------------------------------

struct StackInfo {
    languages: Vec<String>,
    frameworks: Vec<String>,
    entry_points: Vec<String>,
    build_cmd: Option<String>,
}

fn detect_stack(project_path: &Path) -> StackInfo {
    let mut lang_counts: HashMap<&str, usize> = HashMap::new();
    let mut frameworks: Vec<String> = Vec::new();
    let mut entry_points: Vec<String> = Vec::new();
    let mut build_cmd: Option<String> = None;

    // Check for config files that indicate stack
    let checks: &[(&str, &str, &str, &str)] = &[
        // (file, language, framework, build_command)
        ("Cargo.toml", "Rust", "", "cargo build"),
        ("package.json", "JavaScript/TypeScript", "Node.js", "npm install && npm start"),
        ("requirements.txt", "Python", "", "pip install -r requirements.txt"),
        ("setup.py", "Python", "", "pip install -e ."),
        ("pyproject.toml", "Python", "", "pip install -e ."),
        ("go.mod", "Go", "", "go build"),
        ("Gemfile", "Ruby", "", "bundle install"),
        ("pom.xml", "Java", "Maven", "mvn package"),
        ("build.gradle", "Java/Kotlin", "Gradle", "gradle build"),
        ("CMakeLists.txt", "C/C++", "CMake", "cmake --build ."),
        ("Makefile", "", "", "make"),
        ("docker-compose.yml", "", "Docker", "docker-compose up"),
        ("Dockerfile", "", "Docker", "docker build ."),
    ];

    for (file, lang, framework, cmd) in checks {
        if project_path.join(file).exists() {
            if !lang.is_empty() {
                *lang_counts.entry(lang).or_insert(0) += 10; // Config files weight heavily
            }
            if !framework.is_empty() && !frameworks.contains(&framework.to_string()) {
                frameworks.push(framework.to_string());
            }
            if build_cmd.is_none() && !cmd.is_empty() {
                build_cmd = Some(cmd.to_string());
            }
        }
    }

    // Detect frameworks from specific files
    let framework_checks: &[(&str, &str)] = &[
        ("manage.py", "Django"),
        ("app.py", "Flask"),
        ("next.config.js", "Next.js"),
        ("next.config.ts", "Next.js"),
        ("nuxt.config.ts", "Nuxt"),
        ("angular.json", "Angular"),
        ("svelte.config.js", "Svelte"),
        ("tailwind.config.js", "Tailwind CSS"),
        ("tsconfig.json", "TypeScript"),
    ];

    for (file, framework) in framework_checks {
        if project_path.join(file).exists() && !frameworks.contains(&framework.to_string()) {
            frameworks.push(framework.to_string());
        }
    }

    // Detect entry points
    let entry_checks: &[&str] = &[
        "main.rs", "lib.rs", "main.py", "app.py", "manage.py",
        "index.ts", "index.js", "main.ts", "main.js", "server.ts", "server.js",
        "main.go", "main.c", "main.cpp", "Program.cs",
    ];

    // Check root and src/ for entry points
    for entry in entry_checks {
        if project_path.join(entry).exists() {
            entry_points.push(entry.to_string());
        }
        let src_entry = format!("src/{}", entry);
        if project_path.join(&src_entry).exists() {
            entry_points.push(src_entry);
        }
    }

    // Scan files for language counts (depth 1 only for speed)
    if let Ok(entries) = std::fs::read_dir(project_path) {
        for entry in entries.flatten() {
            if let Some(ext) = entry.path().extension().and_then(|e| e.to_str()) {
                let lang = match ext {
                    "py" => "Python",
                    "rs" => "Rust",
                    "js" | "jsx" => "JavaScript",
                    "ts" | "tsx" => "TypeScript",
                    "go" => "Go",
                    "java" | "kt" => "Java/Kotlin",
                    "rb" => "Ruby",
                    "c" | "h" => "C",
                    "cpp" | "cc" | "hpp" => "C++",
                    "cs" => "C#",
                    "html" => "HTML",
                    "css" | "scss" => "CSS",
                    "sql" => "SQL",
                    _ => "",
                };
                if !lang.is_empty() {
                    *lang_counts.entry(lang).or_insert(0) += 1;
                }
            }
        }
    }

    // Sort languages by count (most used first)
    let mut languages: Vec<(&&str, &usize)> = lang_counts.iter().collect();
    languages.sort_by(|a, b| b.1.cmp(a.1));
    let languages: Vec<String> = languages.into_iter().map(|(l, _)| l.to_string()).collect();

    StackInfo { languages, frameworks, entry_points, build_cmd }
}

// ---------------------------------------------------------------------------
// File listing
// ---------------------------------------------------------------------------

fn list_key_files(project_path: &Path, max_files: usize) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();

    fn is_source_file(name: &str) -> bool {
        if let Some(ext) = name.rsplit('.').next() {
            SOURCE_EXTENSIONS.contains(&ext)
        } else {
            // Config files without extensions (Makefile, Dockerfile, etc.)
            matches!(name, "Makefile" | "Dockerfile" | "Gemfile" | "Rakefile" | "Procfile")
        }
    }

    fn walk(dir: &Path, base: &Path, files: &mut Vec<String>, depth: usize, max: usize) {
        if depth > 2 || files.len() >= max {
            return;
        }
        let mut entries: Vec<_> = match std::fs::read_dir(dir) {
            Ok(rd) => rd.flatten().collect(),
            Err(_) => return,
        };
        // Sort: source directories first (src, app, lib), then by name
        entries.sort_by(|a, b| {
            let a_dir = a.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            let b_dir = b.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            let a_name = a.file_name().to_string_lossy().to_lowercase();
            let b_name = b.file_name().to_string_lossy().to_lowercase();
            // Prioritize src-like directories
            let a_priority = matches!(a_name.as_str(), "src" | "app" | "lib" | "cmd" | "pkg");
            let b_priority = matches!(b_name.as_str(), "src" | "app" | "lib" | "cmd" | "pkg");
            b_priority.cmp(&a_priority)
                .then_with(|| b_dir.cmp(&a_dir))
                .then_with(|| a.file_name().cmp(&b.file_name()))
        });

        for entry in entries {
            if files.len() >= max {
                break;
            }
            let name = entry.file_name().to_string_lossy().to_string();

            // Skip hidden files/dirs
            if name.starts_with('.') {
                continue;
            }

            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            if is_dir {
                if SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                let rel = entry.path().strip_prefix(base)
                    .unwrap_or(&entry.path())
                    .to_string_lossy()
                    .replace('\\', "/");
                files.push(format!("{}/", rel));
                walk(&entry.path(), base, files, depth + 1, max);
            } else {
                // Only list source/config files, skip images/binaries
                if is_source_file(&name) {
                    let rel = entry.path().strip_prefix(base)
                        .unwrap_or(&entry.path())
                        .to_string_lossy()
                        .replace('\\', "/");
                    files.push(rel);
                }
            }
        }
    }

    walk(project_path, project_path, &mut files, 0, max_files);
    files
}

// ---------------------------------------------------------------------------
// Auto-generate
// ---------------------------------------------------------------------------

/// Auto-generate a compact chitty.md for a project by scanning its directory.
///
/// Creates `.chitty/chitty.md` with detected stack, entry points, key files,
/// and build commands. Designed to be ~200-500 tokens — just enough to orient
/// the model. Detailed knowledge comes from tool calls.
pub fn auto_generate(project_path: &Path) -> Result<ProjectContext> {
    let project_name = project_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Project");

    let stack = detect_stack(project_path);
    let key_files = list_key_files(project_path, 20);

    // Build the compact chitty.md
    let mut parts: Vec<String> = Vec::new();

    parts.push(format!("# {}", project_name));

    // Stack line
    let mut stack_parts: Vec<String> = Vec::new();
    if !stack.languages.is_empty() {
        stack_parts.push(stack.languages.join(", "));
    }
    if !stack.frameworks.is_empty() {
        stack_parts.push(format!("({})", stack.frameworks.join(", ")));
    }
    if !stack_parts.is_empty() {
        parts.push(format!("**Stack:** {}", stack_parts.join(" ")));
    }

    // Entry points
    if !stack.entry_points.is_empty() {
        parts.push(format!("**Entry:** {}", stack.entry_points.join(", ")));
    }

    // Build command
    if let Some(ref cmd) = stack.build_cmd {
        parts.push(format!("**Build:** `{}`", cmd));
    }

    // Key files
    if !key_files.is_empty() {
        parts.push(String::new());
        parts.push("## Key Files".to_string());
        for f in &key_files {
            parts.push(format!("- {}", f));
        }
    }

    // Notes section (for LLM to fill in during background refresh)
    parts.push(String::new());
    parts.push("## Notes".to_string());
    parts.push("_Auto-generated by Chitty. Will be updated after conversations._".to_string());

    let content = parts.join("\n");

    // Write to .chitty/chitty.md
    let chitty_dir = project_path.join(".chitty");
    std::fs::create_dir_all(&chitty_dir)?;
    let file_path = chitty_dir.join("chitty.md");
    std::fs::write(&file_path, &content)?;

    tracing::info!("Auto-generated chitty.md at {} ({} bytes)", file_path.display(), content.len());

    Ok(ProjectContext {
        project_path: project_path.to_path_buf(),
        file_path,
        content,
    })
}

// ---------------------------------------------------------------------------
// Refresh check
// ---------------------------------------------------------------------------

/// Check if chitty.md needs a background refresh.
/// Returns true if the file exists but hasn't been modified in the last 30 minutes.
pub fn needs_refresh(project_path: &Path) -> bool {
    let candidates = [
        project_path.join(".chitty").join("chitty.md"),
        project_path.join("chitty.md"),
    ];

    for candidate in &candidates {
        if candidate.exists() {
            if let Ok(metadata) = std::fs::metadata(candidate) {
                if let Ok(modified) = metadata.modified() {
                    if let Ok(elapsed) = modified.elapsed() {
                        return elapsed.as_secs() > 1800; // 30 minutes
                    }
                }
            }
            return true; // Exists but can't read metadata — refresh to be safe
        }
    }

    false // No chitty.md exists — auto_generate handles this case
}

/// Generate a starter chitty.md template for a project (legacy — kept for compatibility)
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
