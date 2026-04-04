//! Configuration management
//!
//! App config stored in ~/.chitty-workspace/config.toml
//! API keys stored securely via OS keyring.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const KEYRING_SERVICE: &str = "chitty-workspace";

/// Application configuration
///
/// `deny_unknown_fields` is NOT set so that old configs with removed sections
/// (e.g. [ollama]) are silently ignored during deserialization.
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
    /// Local model sidecar settings (backward compat: reads [huggingface] too)
    #[serde(alias = "huggingface")]
    pub local: LocalModelConfig,
    /// System-wide defaults for each capability (chat, image, video, tts, stt)
    #[serde(default)]
    pub defaults: SystemDefaults,
    /// Legacy field — ignored, kept for backward compat with old config files
    #[serde(default, skip_serializing)]
    pub ollama: Option<toml::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    pub theme: String,
    pub window_width: u32,
    pub window_height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalModelConfig {
    pub enabled: bool,
    pub sidecar_port: u16,
    /// Primary models directory (e.g. "C:\\LLM Models")
    pub models_dir: Option<String>,
    /// Additional directories to scan for GGUF files
    #[serde(default)]
    pub extra_model_dirs: Vec<String>,
}

/// System-wide defaults for each capability type.
/// Each tool/feature reads its default provider/model from here.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SystemDefaults {
    /// Default provider/model for chat conversations
    pub chat_provider: Option<String>,
    pub chat_model: Option<String>,
    /// Default provider/model for system operations (memory, compaction, summarization)
    pub system_agent_provider: Option<String>,
    pub system_agent_model: Option<String>,
    /// Default provider/model for image generation
    pub image_provider: Option<String>,
    pub image_model: Option<String>,
    /// Default provider/model for video generation
    pub video_provider: Option<String>,
    pub video_model: Option<String>,
    /// Default provider/model for text-to-speech
    pub tts_provider: Option<String>,
    pub tts_model: Option<String>,
    /// Default provider/model for speech-to-text
    pub stt_provider: Option<String>,
    pub stt_model: Option<String>,
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
            local: LocalModelConfig {
                enabled: true,
                sidecar_port: 8766,
                models_dir: Some("C:\\LLM Models".to_string()),
                extra_model_dirs: Vec::new(),
            },
            defaults: SystemDefaults::default(),
            ollama: None,
        }
    }
}

impl AppConfig {
    /// Load config from disk, or create default if missing.
    /// Automatically migrates old configs (removes legacy [ollama] section).
    pub fn load(data_dir: &PathBuf) -> Result<Self> {
        let path = data_dir.join("config.toml");
        if path.exists() {
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read config: {:?}", path))?;
            let mut config: AppConfig =
                toml::from_str(&content).with_context(|| "Failed to parse config.toml")?;
            // Migrate: if old [ollama] section was present, strip it and re-save
            if config.ollama.is_some() {
                config.ollama = None;
                let _ = config.save(data_dir);
            }
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
