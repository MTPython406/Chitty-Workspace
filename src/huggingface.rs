//! HuggingFace inference sidecar module.
//!
//! Manages the Python sidecar process (inference_server.py) that runs
//! GGUF models locally via llama-cpp-python. Provides lifecycle management
//! (start/stop) and REST API client functions.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::{Child, Command};
use tracing::{debug, error, info, warn};

// ─── Types ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HFStatus {
    pub running: bool,
    pub loaded_model: Option<String>,
    pub models_registered: Option<u32>,
    pub vram_free_mb: Option<i64>,
    pub sidecar_installed: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HFModel {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    pub size_gb: f64,
    pub quantization: Option<String>,
    pub loaded: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HFChatResponse {
    pub content: String,
    pub model: String,
    pub finish_reason: String,
    pub usage: HFUsage,
    pub elapsed_seconds: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HFUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

// ─── Sidecar Lifecycle ───────────────────────────────────

/// Check if the sidecar Python script exists.
pub fn is_sidecar_installed(data_dir: &Path) -> bool {
    // Check next to the executable first (bundled install)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let bundled = exe_dir.join("sidecar").join("inference_server.py");
            if bundled.exists() {
                return true;
            }
        }
    }
    // Check in data directory
    let script = data_dir.join("sidecar").join("inference_server.py");
    script.exists()
}

/// Find the sidecar script path.
pub fn find_sidecar_script(data_dir: &Path) -> Option<PathBuf> {
    // Check next to executable first (bundled installer)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let bundled = exe_dir.join("sidecar").join("inference_server.py");
            if bundled.exists() {
                return Some(bundled);
            }
            // Also check two levels up (for target/release/ during dev)
            if let Some(project_dir) = exe_dir.parent().and_then(|d| d.parent()) {
                let dev = project_dir.join("sidecar").join("inference_server.py");
                if dev.exists() {
                    return Some(dev);
                }
            }
        }
    }
    // Check in data directory
    let script = data_dir.join("sidecar").join("inference_server.py");
    if script.exists() {
        return Some(script);
    }
    None
}

/// Find Python executable.
/// Checks for venv first, then system python.
pub fn find_python(data_dir: &Path) -> Option<PathBuf> {
    // Check for bundled venv next to executable
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let venv_python = if cfg!(target_os = "windows") {
                exe_dir.join("sidecar").join("venv").join("Scripts").join("python.exe")
            } else {
                exe_dir.join("sidecar").join("venv").join("bin").join("python")
            };
            if venv_python.exists() {
                return Some(venv_python);
            }
        }
    }

    // Check for venv in data_dir
    let venv_python = if cfg!(target_os = "windows") {
        data_dir.join("sidecar").join("venv").join("Scripts").join("python.exe")
    } else {
        data_dir.join("sidecar").join("venv").join("bin").join("python")
    };
    if venv_python.exists() {
        return Some(venv_python);
    }

    // Fall back to system Python
    for name in &["python3", "python"] {
        if let Ok(path) = which::which(name) {
            return Some(path);
        }
    }

    None
}

/// Start the inference sidecar as a child process.
pub async fn start_sidecar(
    python_path: &Path,
    sidecar_script: &Path,
    port: u16,
    extra_model_dirs: &[String],
) -> Result<Child> {
    if !sidecar_script.exists() {
        anyhow::bail!("Sidecar script not found: {}", sidecar_script.display());
    }

    info!(
        "Starting Inference Server: {} {} --port {}",
        python_path.display(),
        sidecar_script.display(),
        port
    );

    let mut cmd = Command::new(python_path);
    cmd.arg(sidecar_script)
        .arg("--port")
        .arg(port.to_string());

    // Add extra model directories
    for dir in extra_model_dirs {
        cmd.arg("--models-dir").arg(dir);
    }

    cmd.stdout(Stdio::null()).stderr(Stdio::null());

    // On Windows, hide the console window
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let child = cmd.spawn().context("Failed to start Inference Server")?;
    info!("Inference Server started (PID: {:?})", child.id());

    // Wait briefly then health-check
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let base_url = format!("http://127.0.0.1:{}", port);
    let status = check_status(&base_url).await;
    if status.running {
        info!("Inference Server is healthy on port {}", port);
    } else {
        warn!(
            "Inference Server may not be ready yet: {:?}",
            status.error
        );
    }

    Ok(child)
}

/// Stop the sidecar child process.
pub async fn stop_sidecar(child: &mut Child) {
    info!("Stopping Inference Server (PID: {:?})", child.id());
    if let Err(e) = child.kill().await {
        warn!("Failed to kill sidecar process: {}", e);
    }
    let _ = child.wait().await;
    info!("Inference Server stopped");
}

// ─── API Functions ────────────────────────────────────────

/// Check if the sidecar is running via GET /health.
pub async fn check_status(base_url: &str) -> HFStatus {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_default();

    let url = format!("{}/health", base_url);
    match client.get(&url).send().await {
        Ok(resp) => {
            if let Ok(body) = resp.json::<Value>().await {
                HFStatus {
                    running: body.get("status").and_then(|s| s.as_str()) == Some("ok"),
                    loaded_model: body
                        .get("loaded_model")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    models_registered: body
                        .get("models_registered")
                        .and_then(|v| v.as_u64())
                        .map(|v| v as u32),
                    vram_free_mb: body
                        .get("vram_free_mb")
                        .and_then(|v| v.as_i64()),
                    sidecar_installed: true,
                    error: None,
                }
            } else {
                HFStatus {
                    running: true,
                    loaded_model: None,
                    models_registered: None,
                    vram_free_mb: None,
                    sidecar_installed: true,
                    error: Some("Failed to parse health response".into()),
                }
            }
        }
        Err(e) => HFStatus {
            running: false,
            loaded_model: None,
            models_registered: None,
            vram_free_mb: None,
            sidecar_installed: false,
            error: Some(format!("Cannot connect to sidecar: {}", e)),
        },
    }
}

/// List all registered models via GET /models.
pub async fn list_models(base_url: &str) -> Result<Vec<HFModel>> {
    let url = format!("{}/models", base_url);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let resp = client
        .get(&url)
        .send()
        .await
        .context("Failed to connect to sidecar /models")?;

    let body: Value = resp.json().await.context("Failed to parse /models response")?;
    let models: Vec<HFModel> = serde_json::from_value(
        body.get("models").cloned().unwrap_or(Value::Array(vec![])),
    )
    .unwrap_or_default();

    Ok(models)
}

/// Trigger a re-scan of model directories via POST /models/scan.
pub async fn scan_models(base_url: &str) -> Result<Value> {
    let url = format!("{}/models/scan", base_url);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let resp = client
        .post(&url)
        .send()
        .await
        .context("Failed to scan models")?;

    let result: Value = resp.json().await?;
    Ok(result)
}

/// Register a GGUF model file via POST /models/register.
pub async fn register_model(base_url: &str, path: &str, name: Option<&str>) -> Result<Value> {
    let url = format!("{}/models/register", base_url);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let mut body = json!({ "path": path });
    if let Some(n) = name {
        body["name"] = json!(n);
    }

    let resp = client.post(&url).json(&body).send().await
        .context("Failed to register model")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let error_body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Register model failed ({}): {}", status, error_body);
    }

    let result: Value = resp.json().await?;
    Ok(result)
}

/// Load a model into GPU memory via POST /models/load.
pub async fn load_model(
    base_url: &str,
    model: &str,
    gpu_layers: Option<i32>,
    context_length: Option<u32>,
) -> Result<Value> {
    let url = format!("{}/models/load", base_url);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let body = json!({
        "model": model,
        "gpu_layers": gpu_layers.unwrap_or(-1),
        "context_length": context_length.unwrap_or(4096),
    });

    info!("Loading HF model: {}", model);
    let resp = client.post(&url).json(&body).send().await
        .context("Failed to load model")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let error_body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Load model failed ({}): {}", status, error_body);
    }

    let result: Value = resp.json().await?;
    info!("HF model loaded: {}", model);
    Ok(result)
}

/// Unload the current model via POST /models/unload.
pub async fn unload_model(base_url: &str) -> Result<Value> {
    let url = format!("{}/models/unload", base_url);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let resp = client.post(&url).send().await
        .context("Failed to unload model")?;

    let result: Value = resp.json().await?;
    Ok(result)
}

/// Send a chat completion request via POST /chat/completions.
pub async fn chat(
    base_url: &str,
    model: &str,
    messages: Vec<Value>,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
) -> Result<HFChatResponse> {
    let url = format!("{}/chat/completions", base_url);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    let body = json!({
        "model": model,
        "messages": messages,
        "temperature": temperature.unwrap_or(0.7),
        "max_tokens": max_tokens.unwrap_or(2048),
    });

    let resp = client.post(&url).json(&body).send().await
        .context("Failed to send chat request")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let error_body = resp.text().await.unwrap_or_default();
        anyhow::bail!("HF chat failed ({}): {}", status, error_body);
    }

    let chat_resp: HFChatResponse = resp.json().await
        .context("Failed to parse chat response")?;
    Ok(chat_resp)
}
