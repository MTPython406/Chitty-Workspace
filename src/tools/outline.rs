//! Code outline tool powered by tree-sitter.
//!
//! Extracts structural overview from source files: function signatures,
//! class/struct definitions, imports, and top-level declarations.
//! Gives local models a compact view of code structure without reading
//! entire files — they can then request specific functions via file_reader.

#[cfg(feature = "tree-sitter-tools")]
use tree_sitter::{Parser, Language, Node};

#[cfg(feature = "tree-sitter-tools")]
use std::path::Path;

/// Get tree-sitter language for a file extension
#[cfg(feature = "tree-sitter-tools")]
fn language_for_ext(ext: &str) -> Option<Language> {
    match ext {
        "rs" => Some(tree_sitter_rust::LANGUAGE.into()),
        "py" => Some(tree_sitter_python::LANGUAGE.into()),
        "js" | "jsx" | "mjs" | "cjs" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "ts" | "tsx" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
        "json" => Some(tree_sitter_json::LANGUAGE.into()),
        _ => None,
    }
}

/// Extract a structural outline from a source file.
/// Returns a compact summary of functions, classes, structs, imports, etc.
#[cfg(feature = "tree-sitter-tools")]
pub fn outline_file(file_path: &Path) -> anyhow::Result<String> {
    let ext = file_path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let language = language_for_ext(ext)
        .ok_or_else(|| anyhow::anyhow!(
            "Unsupported language for outline: .{} (supported: rs, py, js, jsx, ts, tsx, go, json)",
            ext
        ))?;

    let source = std::fs::read_to_string(file_path)?;
    let mut parser = Parser::new();
    parser.set_language(&language)?;

    let tree = parser.parse(&source, None)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse {}", file_path.display()))?;

    let root = tree.root_node();
    let mut outline = Vec::new();
    let file_name = file_path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");

    outline.push(format!("# {} ({} lines)", file_name, source.lines().count()));
    outline.push(String::new());

    extract_outline(&root, &source, ext, &mut outline, 0);

    Ok(outline.join("\n"))
}

/// Recursively extract outline items from the syntax tree
#[cfg(feature = "tree-sitter-tools")]
fn extract_outline(node: &Node, source: &str, lang: &str, items: &mut Vec<String>, depth: usize) {
    let indent = "  ".repeat(depth);

    for i in 0..node.child_count() {
        let child = node.child(i).unwrap();
        let kind = child.kind();
        let start_line = child.start_position().row + 1;

        match lang {
            "rs" => extract_rust(&child, source, kind, start_line, &indent, items, depth),
            "py" => extract_python(&child, source, kind, start_line, &indent, items, depth),
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" => {
                extract_js_ts(&child, source, kind, start_line, &indent, items, depth);
            }
            "go" => extract_go(&child, source, kind, start_line, &indent, items, depth),
            _ => {}
        }
    }
}

#[cfg(feature = "tree-sitter-tools")]
fn get_node_text<'a>(node: &Node, source: &'a str) -> &'a str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}

/// Get just the signature line (first line) of a node
#[cfg(feature = "tree-sitter-tools")]
fn signature_line(node: &Node, source: &str) -> String {
    let text = get_node_text(node, source);
    let first_line = text.lines().next().unwrap_or(text);
    // Truncate long lines
    if first_line.len() > 120 {
        format!("{}...", &first_line[..117])
    } else {
        first_line.to_string()
    }
}

// ---------------------------------------------------------------------------
// Language-specific extractors
// ---------------------------------------------------------------------------

#[cfg(feature = "tree-sitter-tools")]
fn extract_rust(node: &Node, source: &str, kind: &str, line: usize, indent: &str, items: &mut Vec<String>, depth: usize) {
    match kind {
        "use_declaration" => {
            items.push(format!("{}L{}: use {}", indent, line, get_node_text(node, source).trim()));
        }
        "function_item" => {
            items.push(format!("{}L{}: fn {}", indent, line, signature_line(node, source)));
        }
        "struct_item" => {
            let name = node.child_by_field_name("name")
                .map(|n| get_node_text(&n, source))
                .unwrap_or("?");
            items.push(format!("{}L{}: struct {}", indent, line, name));
            extract_outline(node, source, "rs", items, depth + 1);
        }
        "enum_item" => {
            let name = node.child_by_field_name("name")
                .map(|n| get_node_text(&n, source))
                .unwrap_or("?");
            items.push(format!("{}L{}: enum {}", indent, line, name));
        }
        "impl_item" => {
            items.push(format!("{}L{}: {}", indent, line, signature_line(node, source)));
            extract_outline(node, source, "rs", items, depth + 1);
        }
        "trait_item" => {
            let name = node.child_by_field_name("name")
                .map(|n| get_node_text(&n, source))
                .unwrap_or("?");
            items.push(format!("{}L{}: trait {}", indent, line, name));
            extract_outline(node, source, "rs", items, depth + 1);
        }
        "mod_item" => {
            let name = node.child_by_field_name("name")
                .map(|n| get_node_text(&n, source))
                .unwrap_or("?");
            items.push(format!("{}L{}: mod {}", indent, line, name));
        }
        "const_item" | "static_item" => {
            items.push(format!("{}L{}: {}", indent, line, signature_line(node, source)));
        }
        "type_item" => {
            items.push(format!("{}L{}: {}", indent, line, signature_line(node, source)));
        }
        "macro_definition" => {
            let name = node.child_by_field_name("name")
                .map(|n| get_node_text(&n, source))
                .unwrap_or("?");
            items.push(format!("{}L{}: macro {}!", indent, line, name));
        }
        _ => {}
    }
}

#[cfg(feature = "tree-sitter-tools")]
fn extract_python(node: &Node, source: &str, kind: &str, line: usize, indent: &str, items: &mut Vec<String>, depth: usize) {
    match kind {
        "import_statement" | "import_from_statement" => {
            items.push(format!("{}L{}: {}", indent, line, get_node_text(node, source).trim()));
        }
        "function_definition" => {
            items.push(format!("{}L{}: {}", indent, line, signature_line(node, source)));
        }
        "class_definition" => {
            items.push(format!("{}L{}: {}", indent, line, signature_line(node, source)));
            // Recurse into class body for methods
            if let Some(body) = node.child_by_field_name("body") {
                extract_outline(&body, source, "py", items, depth + 1);
            }
        }
        "decorated_definition" => {
            // Extract the actual definition inside decorators
            extract_outline(node, source, "py", items, depth);
        }
        "expression_statement" => {
            // Top-level assignments (constants, etc.)
            let text = get_node_text(node, source).trim().to_string();
            if text.contains('=') && !text.starts_with('#') && depth == 0 {
                let short = if text.len() > 80 { format!("{}...", &text[..77]) } else { text };
                items.push(format!("{}L{}: {}", indent, line, short));
            }
        }
        _ => {}
    }
}

#[cfg(feature = "tree-sitter-tools")]
fn extract_js_ts(node: &Node, source: &str, kind: &str, line: usize, indent: &str, items: &mut Vec<String>, depth: usize) {
    match kind {
        "import_statement" => {
            items.push(format!("{}L{}: {}", indent, line, signature_line(node, source)));
        }
        "function_declaration" | "generator_function_declaration" => {
            items.push(format!("{}L{}: {}", indent, line, signature_line(node, source)));
        }
        "class_declaration" => {
            items.push(format!("{}L{}: {}", indent, line, signature_line(node, source)));
            if let Some(body) = node.child_by_field_name("body") {
                extract_outline(&body, source, "ts", items, depth + 1);
            }
        }
        "method_definition" => {
            items.push(format!("{}L{}: {}", indent, line, signature_line(node, source)));
        }
        "export_statement" => {
            // Recurse to find the actual declaration inside export
            extract_outline(node, source, "ts", items, depth);
        }
        "lexical_declaration" | "variable_declaration" => {
            if depth == 0 {
                items.push(format!("{}L{}: {}", indent, line, signature_line(node, source)));
            }
        }
        "interface_declaration" | "type_alias_declaration" => {
            items.push(format!("{}L{}: {}", indent, line, signature_line(node, source)));
        }
        "enum_declaration" => {
            items.push(format!("{}L{}: {}", indent, line, signature_line(node, source)));
        }
        _ => {}
    }
}

#[cfg(feature = "tree-sitter-tools")]
fn extract_go(node: &Node, source: &str, kind: &str, line: usize, indent: &str, items: &mut Vec<String>, depth: usize) {
    match kind {
        "import_declaration" => {
            items.push(format!("{}L{}: {}", indent, line, signature_line(node, source)));
        }
        "function_declaration" => {
            items.push(format!("{}L{}: {}", indent, line, signature_line(node, source)));
        }
        "method_declaration" => {
            items.push(format!("{}L{}: {}", indent, line, signature_line(node, source)));
        }
        "type_declaration" => {
            items.push(format!("{}L{}: {}", indent, line, signature_line(node, source)));
            extract_outline(node, source, "go", items, depth + 1);
        }
        "var_declaration" | "const_declaration" => {
            if depth == 0 {
                items.push(format!("{}L{}: {}", indent, line, signature_line(node, source)));
            }
        }
        _ => {}
    }
}

/// List supported language extensions
#[cfg(feature = "tree-sitter-tools")]
pub fn supported_extensions() -> &'static [&'static str] {
    &["rs", "py", "js", "jsx", "mjs", "cjs", "ts", "tsx", "go", "json"]
}

/// Check if a file extension is supported
#[cfg(feature = "tree-sitter-tools")]
pub fn is_supported(ext: &str) -> bool {
    supported_extensions().contains(&ext)
}

// ---------------------------------------------------------------------------
// Fallback when tree-sitter is not enabled
// ---------------------------------------------------------------------------

#[cfg(not(feature = "tree-sitter-tools"))]
pub fn outline_file(file_path: &std::path::Path) -> anyhow::Result<String> {
    Err(anyhow::anyhow!(
        "Code outline not available. Rebuild with: cargo build --features tree-sitter-tools"
    ))
}

#[cfg(not(feature = "tree-sitter-tools"))]
pub fn is_supported(_ext: &str) -> bool {
    false
}
