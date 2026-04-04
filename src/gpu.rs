//! GPU monitoring module.
//!
//! Queries NVIDIA GPU stats via nvidia-smi for VRAM usage, utilization,
//! temperature, and power draw. Handles gracefully when no NVIDIA GPU
//! or nvidia-smi is not available.

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuStats {
    pub available: bool,
    pub gpu_name: Option<String>,
    pub vram_total_mb: Option<u64>,
    pub vram_used_mb: Option<u64>,
    pub vram_free_mb: Option<u64>,
    pub utilization_pct: Option<u32>,
    pub temperature_c: Option<u32>,
    pub power_watts: Option<f64>,
    pub error: Option<String>,
}

impl Default for GpuStats {
    fn default() -> Self {
        Self {
            available: false,
            gpu_name: None,
            vram_total_mb: None,
            vram_used_mb: None,
            vram_free_mb: None,
            utilization_pct: None,
            temperature_c: None,
            power_watts: None,
            error: None,
        }
    }
}

/// Get GPU stats by running nvidia-smi.
///
/// Returns a GpuStats struct. If nvidia-smi is not available or fails,
/// returns GpuStats with available=false and an error message.
pub async fn get_gpu_stats() -> GpuStats {
    let mut cmd = tokio::process::Command::new("nvidia-smi");
    cmd.args([
        "--query-gpu=name,memory.total,memory.used,memory.free,utilization.gpu,temperature.gpu,power.draw",
        "--format=csv,noheader,nounits",
    ]);

    // On Windows, hide the console window so nvidia-smi doesn't flash on screen
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let output = cmd.output().await;

    match output {
        Ok(out) => {
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                warn!("nvidia-smi failed: {}", stderr);
                return GpuStats {
                    error: Some(format!("nvidia-smi failed: {}", stderr.trim())),
                    ..Default::default()
                };
            }

            let stdout = String::from_utf8_lossy(&out.stdout);
            parse_nvidia_smi_output(stdout.trim())
        }
        Err(e) => {
            debug!("nvidia-smi not available: {}", e);
            GpuStats {
                error: Some("nvidia-smi not found. No NVIDIA GPU detected.".to_string()),
                ..Default::default()
            }
        }
    }
}

/// Estimate VRAM needed to load a GGUF model.
///
/// Returns (estimated_mb, free_mb, fits).
/// Heuristic: GGUF VRAM ≈ file_size * 1.1 (10% overhead for KV cache at default context).
pub async fn estimate_vram(file_size_bytes: u64) -> (u64, Option<u64>, bool) {
    let estimated_mb = ((file_size_bytes as f64 * 1.1) / (1024.0 * 1024.0)).ceil() as u64;
    let stats = get_gpu_stats().await;
    let free_mb = stats.vram_free_mb;
    let fits = free_mb.map(|f| estimated_mb <= f).unwrap_or(false);
    (estimated_mb, free_mb, fits)
}

/// Parse nvidia-smi CSV output into GpuStats.
///
/// Expected format: "NVIDIA GeForce RTX 5090, 32768, 1234, 31534, 45, 52, 120.50"
fn parse_nvidia_smi_output(line: &str) -> GpuStats {
    // Take first GPU line only (multi-GPU: extend later)
    let first_line = line.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split(',').map(|s| s.trim()).collect();

    if parts.len() < 7 {
        return GpuStats {
            error: Some(format!("Unexpected nvidia-smi output: {}", first_line)),
            ..Default::default()
        };
    }

    GpuStats {
        available: true,
        gpu_name: Some(parts[0].to_string()),
        vram_total_mb: parts[1].parse().ok(),
        vram_used_mb: parts[2].parse().ok(),
        vram_free_mb: parts[3].parse().ok(),
        utilization_pct: parts[4].parse().ok(),
        temperature_c: parts[5].parse().ok(),
        power_watts: parts[6].parse().ok(),
        error: None,
    }
}
