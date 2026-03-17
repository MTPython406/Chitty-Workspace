//! Tool system
//!
//! Tools are executable functions the agent can call.
//! Native tools are built into the binary. Custom tools are user/AI-defined.

use serde::{Deserialize, Serialize};

/// Tool definition (JSON Schema compatible for LLM function calling)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Unique tool name
    pub name: String,
    /// Human-readable description (sent to the LLM)
    pub description: String,
    /// JSON Schema for parameters
    pub parameters: serde_json::Value,
    /// Detailed instructions for the LLM on when/how to use this tool
    pub instructions: Option<String>,
    /// Tool category
    pub category: ToolCategory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCategory {
    /// Built-in native tools (file, terminal, code)
    Native,
    /// User-created or AI-generated custom tools
    Custom,
    /// Integration-provided tools
    Integration,
}

/// Result of executing a tool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub success: bool,
    pub output: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// TODO: Implement
// - Native tools: file_reader, file_writer, terminal, code_search, code_analyzer
// - Custom tool execution (script-based)
// - Tool registry (list available tools)
// - Tool permission system
