//! Configuration management
//!
//! App config stored in ~/.chitty-workspace/config.toml
//! API keys stored securely via OS keyring.

use serde::{Deserialize, Serialize};

/// Application configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Default provider for new chats
    pub default_provider: Option<String>,
    /// Default model for new chats
    pub default_model: Option<String>,
    /// Managed project directories
    pub project_dirs: Vec<String>,
    /// UI preferences
    pub ui: UiConfig,
    /// Ollama settings
    pub ollama: OllamaConfig,
    /// HuggingFace sidecar settings
    pub huggingface: HuggingFaceConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    pub theme: String,
    pub window_width: u32,
    pub window_height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OllamaConfig {
    pub enabled: bool,
    pub base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HuggingFaceConfig {
    pub enabled: bool,
    pub sidecar_port: u16,
    pub models_dir: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_provider: None,
            default_model: None,
            project_dirs: Vec::new(),
            ui: UiConfig {
                theme: "dark".to_string(),
                window_width: 1200,
                window_height: 800,
            },
            ollama: OllamaConfig {
                enabled: true,
                base_url: "http://localhost:11434".to_string(),
            },
            huggingface: HuggingFaceConfig {
                enabled: false,
                sidecar_port: 8766,
                models_dir: None,
            },
        }
    }
}

// TODO: Implement
// - Load/save config from ~/.chitty-workspace/config.toml
// - API key storage via keyring crate (OS-level secure storage)
// - Config validation
// - First-run setup flow
