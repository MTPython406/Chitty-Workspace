//! Configuration management
//!
//! App config stored in ~/.chitty-workspace/config.toml
//! API keys stored securely via OS keyring.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const KEYRING_SERVICE: &str = "chitty-workspace";

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

impl AppConfig {
    /// Load config from disk, or create default if missing.
    pub fn load(data_dir: &PathBuf) -> Result<Self> {
        let path = data_dir.join("config.toml");
        if path.exists() {
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read config: {:?}", path))?;
            let config: AppConfig =
                toml::from_str(&content).with_context(|| "Failed to parse config.toml")?;
            Ok(config)
        } else {
            let config = Self::default();
            config.save(data_dir)?;
            Ok(config)
        }
    }

    /// Save config to disk.
    pub fn save(&self, data_dir: &PathBuf) -> Result<()> {
        let path = data_dir.join("config.toml");
        let content = toml::to_string_pretty(self).context("Failed to serialize config")?;
        std::fs::write(&path, content)
            .with_context(|| format!("Failed to write config: {:?}", path))?;
        Ok(())
    }
}

/// Store an API key in the OS keyring (Windows Credential Manager).
pub fn set_api_key(provider_id: &str, key: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, provider_id)
        .context("Failed to create keyring entry")?;
    entry
        .set_password(key)
        .context("Failed to store API key in keyring")?;
    tracing::info!("API key stored for provider: {}", provider_id);
    Ok(())
}

/// Retrieve an API key from the OS keyring.
pub fn get_api_key(provider_id: &str) -> Result<Option<String>> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, provider_id)
        .context("Failed to create keyring entry")?;
    match entry.get_password() {
        Ok(key) => Ok(Some(key)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("Keyring error: {}", e)),
    }
}

/// Delete an API key from the OS keyring.
pub fn delete_api_key(provider_id: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, provider_id)
        .context("Failed to create keyring entry")?;
    match entry.delete_password() {
        Ok(()) => {
            tracing::info!("API key deleted for provider: {}", provider_id);
            Ok(())
        }
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("Keyring error: {}", e)),
    }
}
