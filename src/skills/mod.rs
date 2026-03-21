//! Agent Skills system — composable capability packages following the open Agent Skills standard
//!
//! A Skill is a folder containing a SKILL.md file with YAML frontmatter + markdown instructions.
//! Skills bundle domain expertise + tool requirements + scripts + references.
//!
//! Skills follow the open standard at agentskills.io (adopted by Claude Code, Cursor, VS Code,
//! GitHub Copilot, Gemini CLI, and 30+ other platforms).
//!
//! Three-tier progressive loading:
//! 1. Catalog (always loaded, ~50-100 tokens per skill): name + description in system prompt
//! 2. Instructions (loaded on activation): full SKILL.md body via load_skill tool
//! 3. Resources (loaded as needed): scripts/, references/, assets/ via file_reader/terminal

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Where a skill was discovered
#[derive(Debug, Clone, PartialEq)]
pub enum SkillSource {
    /// From a marketplace package (vendor/package)
    Marketplace { vendor: String, package: String },
    /// From project-level .agents/skills/ or .chitty/skills/
    Project,
    /// From user-level ~/.agents/skills/ or ~/.chitty-workspace/skills/
    User,
    /// Compiled into the binary
    BuiltIn,
}

impl std::fmt::Display for SkillSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillSource::Marketplace { vendor, .. } => write!(f, "marketplace ({})", vendor),
            SkillSource::Project => write!(f, "project"),
            SkillSource::User => write!(f, "user"),
            SkillSource::BuiltIn => write!(f, "built-in"),
        }
    }
}

/// A discovered skill (metadata only — body loaded on demand)
#[derive(Debug, Clone)]
pub struct Skill {
    /// Skill name (from SKILL.md frontmatter, e.g. "web-app-builder")
    pub name: String,
    /// Description of what the skill does and when to use it
    pub description: String,
    /// Tools this skill requires (from allowed-tools field)
    pub allowed_tools: Vec<String>,
    /// Path to the SKILL.md file
    pub skill_path: PathBuf,
    /// Where this skill was discovered
    pub source: SkillSource,
    /// Environment requirements (optional)
    pub compatibility: Option<String>,
    /// License (optional)
    pub license: Option<String>,
    /// Arbitrary metadata key-value pairs (optional)
    pub metadata: HashMap<String, String>,
}

/// Parsed SKILL.md frontmatter
#[derive(Debug, Clone)]
struct SkillFrontmatter {
    name: String,
    description: String,
    allowed_tools: Vec<String>,
    compatibility: Option<String>,
    license: Option<String>,
    metadata: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// SKILL.md Parser
// ---------------------------------------------------------------------------

/// Parse YAML frontmatter from a SKILL.md file.
///
/// Format:
/// ```text
/// ---
/// name: skill-name
/// description: What this skill does and when to use it.
/// allowed-tools: file_writer terminal browser
/// compatibility: Requires Python 3.10+
/// license: MIT
/// metadata:
///   author: example-org
///   version: "1.0"
/// ---
/// # Skill Instructions (markdown body follows)
/// ```
///
/// Uses a simple hand-rolled parser to avoid adding a YAML dependency.
/// Handles the common cases from the open standard specification.
fn parse_frontmatter(content: &str) -> Option<SkillFrontmatter> {
    let trimmed = content.trim_start();

    // Must start with ---
    if !trimmed.starts_with("---") {
        return None;
    }

    // Find the closing ---
    let after_open = &trimmed[3..].trim_start_matches(['\r', '\n']);
    let close_pos = after_open.find("\n---")?;
    let yaml_block = &after_open[..close_pos];

    let mut name = String::new();
    let mut description = String::new();
    let mut allowed_tools = Vec::new();
    let mut compatibility = None;
    let mut license = None;
    let mut metadata = HashMap::new();
    let mut in_metadata = false;
    let mut in_multiline_desc = false;
    let mut desc_parts = Vec::new();

    for line in yaml_block.lines() {
        let trimmed_line = line.trim();

        // Handle metadata sub-keys (indented key: value under metadata:)
        if in_metadata {
            if line.starts_with("  ") || line.starts_with("\t") {
                if let Some((k, v)) = trimmed_line.split_once(':') {
                    let key = k.trim().to_string();
                    let val = v.trim().trim_matches('"').trim_matches('\'').to_string();
                    if !key.is_empty() && !val.is_empty() {
                        metadata.insert(key, val);
                    }
                }
                continue;
            } else {
                in_metadata = false;
            }
        }

        // Handle multiline description (YAML block scalar or continuation)
        if in_multiline_desc {
            if line.starts_with("  ") || line.starts_with("\t") {
                desc_parts.push(trimmed_line.to_string());
                continue;
            } else {
                description = desc_parts.join(" ");
                in_multiline_desc = false;
            }
        }

        if trimmed_line.is_empty() || trimmed_line.starts_with('#') {
            continue;
        }

        // Parse top-level key: value
        if let Some((key, value)) = trimmed_line.split_once(':') {
            let key = key.trim();
            let value = value.trim();

            match key {
                "name" => {
                    name = value.trim_matches('"').trim_matches('\'').to_string();
                }
                "description" => {
                    let clean = value.trim_matches('"').trim_matches('\'');
                    if clean.is_empty() || clean == ">" || clean == "|" {
                        // Multiline description
                        in_multiline_desc = true;
                        desc_parts.clear();
                    } else {
                        // Handle "description: Use this skill when: ..." (unquoted colons)
                        // Re-join everything after the first colon
                        let full_value = trimmed_line
                            .strip_prefix("description:")
                            .unwrap_or(value)
                            .trim()
                            .trim_matches('"')
                            .trim_matches('\'');
                        description = full_value.to_string();
                    }
                }
                "allowed-tools" => {
                    allowed_tools = value
                        .split_whitespace()
                        .map(|s| s.to_string())
                        .collect();
                }
                "compatibility" => {
                    let v = value.trim_matches('"').trim_matches('\'').to_string();
                    if !v.is_empty() {
                        compatibility = Some(v);
                    }
                }
                "license" => {
                    let v = value.trim_matches('"').trim_matches('\'').to_string();
                    if !v.is_empty() {
                        license = Some(v);
                    }
                }
                "metadata" => {
                    in_metadata = true;
                }
                _ => {
                    // Unknown field — store in metadata for forward compatibility
                }
            }
        }
    }

    // Finalize multiline description if still open
    if in_multiline_desc && !desc_parts.is_empty() {
        description = desc_parts.join(" ");
    }

    // Validate required fields
    if name.is_empty() || description.is_empty() {
        tracing::warn!(
            "SKILL.md missing required fields (name={:?}, description_empty={})",
            name,
            description.is_empty()
        );
        return None;
    }

    Some(SkillFrontmatter {
        name,
        description,
        allowed_tools,
        compatibility,
        license,
        metadata,
    })
}

/// Extract the body content from a SKILL.md file (everything after the closing ---)
pub fn extract_body(content: &str) -> String {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return content.to_string();
    }

    let after_open = trimmed[3..].trim_start_matches(['\r', '\n']);
    if let Some(close_pos) = after_open.find("\n---") {
        let after_close = &after_open[close_pos + 4..];
        after_close.trim_start_matches(['\r', '\n']).to_string()
    } else {
        content.to_string()
    }
}

/// List bundled resource files in a skill directory (scripts/, references/, assets/)
pub fn list_resources(skill_dir: &Path) -> Vec<String> {
    let mut resources = Vec::new();
    let subdirs = ["scripts", "references", "assets"];

    for subdir in &subdirs {
        let dir = skill_dir.join(subdir);
        if dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                        if let Some(name) = entry.file_name().to_str() {
                            resources.push(format!("{}/{}", subdir, name));
                        }
                    }
                }
            }
        }
    }

    // Also include any .md files in the skill root (besides SKILL.md)
    if let Ok(entries) = std::fs::read_dir(skill_dir) {
        for entry in entries.flatten() {
            if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                if let Some(name) = entry.file_name().to_str() {
                    if name.ends_with(".md") && name != "SKILL.md" {
                        resources.push(name.to_string());
                    }
                }
            }
        }
    }

    resources
}

// ---------------------------------------------------------------------------
// Skill Registry
// ---------------------------------------------------------------------------

/// Registry of discovered skills, providing catalog generation and tool unioning
pub struct SkillRegistry {
    /// All discovered skills, keyed by name
    skills: HashMap<String, Skill>,
    /// Ordered list for consistent output
    order: Vec<String>,
}

impl SkillRegistry {
    /// Create a new registry by scanning all discovery paths
    pub fn new(data_dir: &Path, project_path: Option<&Path>) -> Self {
        let mut registry = Self {
            skills: HashMap::new(),
            order: Vec::new(),
        };

        // Scan in priority order (later entries DON'T override earlier ones)
        // Project-level skills (highest priority)
        if let Some(project) = project_path {
            registry.scan_directory(&project.join(".agents").join("skills"), SkillSource::Project);
            registry.scan_directory(&project.join(".chitty").join("skills"), SkillSource::Project);
        }

        // User-level skills
        let home = directories::UserDirs::new()
            .map(|u| u.home_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        registry.scan_directory(&home.join(".agents").join("skills"), SkillSource::User);
        registry.scan_directory(&data_dir.join("skills"), SkillSource::User);

        // Marketplace skills (look for SKILL.md alongside package.json)
        let marketplace_dir = data_dir.join("tools").join("marketplace");
        registry.scan_marketplace_skills(&marketplace_dir);

        let count = registry.skills.len();
        if count > 0 {
            tracing::info!("Discovered {} skills: {:?}", count, registry.order);
        }

        registry
    }

    /// Scan a directory for skill folders (each containing SKILL.md)
    fn scan_directory(&mut self, dir: &Path, source: SkillSource) {
        if !dir.is_dir() {
            return;
        }

        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            if !entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                continue;
            }

            let skill_md = entry.path().join("SKILL.md");
            if !skill_md.exists() {
                continue;
            }

            self.load_skill_from_path(&skill_md, source.clone());
        }
    }

    /// Scan marketplace packages for SKILL.md files alongside package.json.
    /// Supports both flat layout (marketplace/<package>/SKILL.md) and
    /// vendor-nested layout (marketplace/<vendor>/<package>/SKILL.md).
    fn scan_marketplace_skills(&mut self, marketplace_dir: &Path) {
        if !marketplace_dir.is_dir() {
            return;
        }

        let entries = match std::fs::read_dir(marketplace_dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            if !entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                continue;
            }
            let dir_name = entry.file_name().to_str().unwrap_or("").to_string();

            // Check flat layout: marketplace/<package>/SKILL.md
            let skill_md = entry.path().join("SKILL.md");
            if skill_md.exists() {
                self.load_skill_from_path(
                    &skill_md,
                    SkillSource::Marketplace {
                        vendor: dir_name.clone(),
                        package: dir_name.clone(),
                    },
                );
                continue; // Don't scan subdirs if this directory IS a skill package
            }

            // Check vendor-nested layout: marketplace/<vendor>/<package>/SKILL.md
            let sub_entries = match std::fs::read_dir(entry.path()) {
                Ok(e) => e,
                Err(_) => continue,
            };

            for sub_entry in sub_entries.flatten() {
                if !sub_entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                    continue;
                }

                let pkg_skill_md = sub_entry.path().join("SKILL.md");
                if pkg_skill_md.exists() {
                    let pkg_name = sub_entry.file_name().to_str().unwrap_or("").to_string();
                    self.load_skill_from_path(
                        &pkg_skill_md,
                        SkillSource::Marketplace {
                            vendor: dir_name.clone(),
                            package: pkg_name,
                        },
                    );
                }
            }
        }
    }

    /// Load a single skill from a SKILL.md path
    fn load_skill_from_path(&mut self, skill_md: &Path, source: SkillSource) {
        let content = match std::fs::read_to_string(skill_md) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to read {}: {}", skill_md.display(), e);
                return;
            }
        };

        let frontmatter = match parse_frontmatter(&content) {
            Some(fm) => fm,
            None => {
                tracing::warn!(
                    "Failed to parse frontmatter from {} (missing name or description)",
                    skill_md.display()
                );
                return;
            }
        };

        // Check for name collision (first-found wins)
        if self.skills.contains_key(&frontmatter.name) {
            tracing::warn!(
                "Skill name collision: '{}' from {} shadowed by earlier discovery",
                frontmatter.name,
                skill_md.display()
            );
            return;
        }

        // Warn if name doesn't match directory name (lenient — load anyway)
        if let Some(dir_name) = skill_md.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()) {
            if dir_name != frontmatter.name {
                tracing::warn!(
                    "Skill name '{}' doesn't match directory name '{}' (loading anyway)",
                    frontmatter.name,
                    dir_name
                );
            }
        }

        let skill = Skill {
            name: frontmatter.name.clone(),
            description: frontmatter.description,
            allowed_tools: frontmatter.allowed_tools,
            skill_path: skill_md.to_path_buf(),
            source,
            compatibility: frontmatter.compatibility,
            license: frontmatter.license,
            metadata: frontmatter.metadata,
        };

        self.order.push(skill.name.clone());
        self.skills.insert(skill.name.clone(), skill);
    }

    /// Get all discovered skills
    pub fn list(&self) -> Vec<&Skill> {
        self.order
            .iter()
            .filter_map(|name| self.skills.get(name))
            .collect()
    }

    /// Get a skill by name
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// Get all skill names
    pub fn names(&self) -> Vec<String> {
        self.order.clone()
    }

    /// Build the XML skill catalog for injection into the system prompt.
    ///
    /// If `agent_skills` is provided, only include those skills.
    /// If empty, include all skills (for the default Chitty agent).
    pub fn build_catalog_xml(&self, agent_skills: &[String]) -> String {
        let skills: Vec<&Skill> = if agent_skills.is_empty() {
            // Default agent: show all skills
            self.list()
        } else {
            agent_skills
                .iter()
                .filter_map(|name| self.skills.get(name))
                .collect()
        };

        if skills.is_empty() {
            return String::new();
        }

        let mut xml = String::from("## Available Skills\n\nThe following skills provide specialized instructions for specific tasks.\nWhen a task matches a skill's description, call the load_skill tool with the skill's name to load its full instructions.\n\n<available_skills>\n");

        for skill in &skills {
            xml.push_str(&format!(
                "  <skill>\n    <name>{}</name>\n    <description>{}</description>\n  </skill>\n",
                skill.name, skill.description
            ));
        }

        xml.push_str("</available_skills>");
        xml
    }

    /// Compute the union of all tools required by a set of skills.
    ///
    /// Returns a HashSet of tool names. If a skill has empty allowed_tools,
    /// it doesn't restrict tools (contributes nothing to the union).
    pub fn union_tools(&self, skill_names: &[String]) -> HashSet<String> {
        let mut tools = HashSet::new();

        for name in skill_names {
            if let Some(skill) = self.skills.get(name) {
                for tool in &skill.allowed_tools {
                    tools.insert(tool.clone());
                }
            }
        }

        tools
    }

    /// Load the full body content of a skill (for the load_skill tool).
    /// Strips frontmatter, wraps in <skill_content> tags, and lists bundled resources.
    pub fn load_skill_content(&self, name: &str) -> Option<String> {
        let skill = self.skills.get(name)?;
        let content = std::fs::read_to_string(&skill.skill_path).ok()?;
        let body = extract_body(&content);

        let skill_dir = skill.skill_path.parent()?;
        let resources = list_resources(skill_dir);

        let mut result = format!(
            "<skill_content name=\"{}\">\n{}\n\nSkill directory: {}\nRelative paths in this skill are relative to the skill directory.",
            skill.name,
            body,
            skill_dir.display()
        );

        if !resources.is_empty() {
            result.push_str("\n\n<skill_resources>\n");
            for resource in &resources {
                result.push_str(&format!("  <file>{}</file>\n", resource));
            }
            result.push_str("</skill_resources>");
        }

        result.push_str("\n</skill_content>");
        Some(result)
    }

    /// Check if a skill exists by name
    pub fn has_skill(&self, name: &str) -> bool {
        self.skills.contains_key(name)
    }
}

// ---------------------------------------------------------------------------
// Serialization for API responses
// ---------------------------------------------------------------------------

/// Skill summary for API responses
#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub allowed_tools: Vec<String>,
    pub source: String,
    pub compatibility: Option<String>,
    pub path: String,
}

impl From<&Skill> for SkillSummary {
    fn from(skill: &Skill) -> Self {
        Self {
            name: skill.name.clone(),
            description: skill.description.clone(),
            allowed_tools: skill.allowed_tools.clone(),
            source: skill.source.to_string(),
            compatibility: skill.compatibility.clone(),
            path: skill.skill_path.display().to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_frontmatter() {
        let content = r#"---
name: test-skill
description: A test skill for unit testing.
allowed-tools: file_reader terminal
---
# Test Skill

Instructions here.
"#;

        let fm = parse_frontmatter(content).unwrap();
        assert_eq!(fm.name, "test-skill");
        assert_eq!(fm.description, "A test skill for unit testing.");
        assert_eq!(fm.allowed_tools, vec!["file_reader", "terminal"]);
    }

    #[test]
    fn test_parse_multiline_description() {
        let content = r#"---
name: multi-desc
description: >
  This is a multiline description
  that spans multiple lines.
---
Body here.
"#;

        let fm = parse_frontmatter(content).unwrap();
        assert_eq!(fm.name, "multi-desc");
        assert!(fm.description.contains("multiline description"));
    }

    #[test]
    fn test_parse_description_with_colon() {
        let content = r#"---
name: colon-desc
description: Use this skill when: the user asks about PDFs
---
Body.
"#;

        let fm = parse_frontmatter(content).unwrap();
        assert_eq!(fm.name, "colon-desc");
        assert!(fm.description.contains("Use this skill when:"));
    }

    #[test]
    fn test_missing_description_returns_none() {
        let content = r#"---
name: no-desc
---
Body.
"#;

        assert!(parse_frontmatter(content).is_none());
    }

    #[test]
    fn test_extract_body() {
        let content = r#"---
name: test
description: Test skill.
---

# Hello

Instructions here."#;

        let body = extract_body(content);
        assert!(body.starts_with("# Hello"));
        assert!(body.contains("Instructions here."));
    }

    #[test]
    fn test_parse_metadata() {
        let content = r#"---
name: meta-test
description: Skill with metadata.
metadata:
  author: test-org
  version: "2.0"
---
Body.
"#;

        let fm = parse_frontmatter(content).unwrap();
        assert_eq!(fm.metadata.get("author").unwrap(), "test-org");
        assert_eq!(fm.metadata.get("version").unwrap(), "2.0");
    }
}
