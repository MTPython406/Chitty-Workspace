//! Custom tool executor — runs script-based tools in sandboxed subprocesses
//!
//! Each custom tool is a script (Python, Node, PowerShell, Shell) that:
//! 1. Receives parameters as JSON on stdin
//! 2. Does its work
//! 3. Returns a JSON result on stdout: {"success": bool, "output": ..., "error": ...}
//!
//! The executor handles:
//! - Runtime detection (python/python3, node, pwsh, etc.)
//! - Sandboxed working directory
//! - Timeout enforcement
//! - Output parsing and truncation

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::tools::manifest::{RuntimeType, ToolManifest};
use crate::tools::ToolResult;

/// Execute a custom tool script
///
/// If `package_config` is provided (JSON string), it will be passed to the tool
/// as the `CHITTY_PACKAGE_CONFIG` environment variable so the tool can enforce
/// allowed resources and feature flags.
pub async fn execute_custom(
    manifest: &ToolManifest,
    tool_dir: &Path,
    args: &serde_json::Value,
    sandbox_dir: &Path,
    packages_dir: &Path,
    package_config: Option<&str>,
) -> ToolResult {
    // Validate manifest
    if let Err(e) = manifest.validate() {
        return ToolResult::err(format!("Invalid tool manifest: {}", e));
    }

    let script_path = tool_dir.join(&manifest.entry_point);
    if !script_path.exists() {
        return ToolResult::err(format!(
            "Tool script not found: {}",
            script_path.display()
        ));
    }

    // Create sandbox working directory
    let sandbox_work_dir = sandbox_dir.join(format!(
        "{}_{}", manifest.name,
        uuid::Uuid::new_v4().to_string().split('-').next().unwrap_or("x")
    ));
    if let Err(e) = tokio::fs::create_dir_all(&sandbox_work_dir).await {
        return ToolResult::err(format!("Failed to create sandbox directory: {}", e));
    }

    let timeout = Duration::from_secs(manifest.timeout_seconds as u64);
    let args_json = serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string());

    // Build the command based on runtime type
    let result = match build_and_run_command(
        &manifest.runtime,
        &script_path,
        &args_json,
        &sandbox_work_dir,
        tool_dir,
        packages_dir,
        &manifest.name,
        timeout,
        package_config,
    ).await {
        Ok(output) => parse_tool_output(&output.stdout, &output.stderr, output.success),
        Err(e) => ToolResult::err(e),
    };

    // Clean up sandbox (best-effort)
    let _ = tokio::fs::remove_dir_all(&sandbox_work_dir).await;

    result
}

struct ProcessOutput {
    stdout: String,
    stderr: String,
    success: bool,
}

async fn build_and_run_command(
    runtime: &RuntimeType,
    script_path: &Path,
    args_json: &str,
    sandbox_dir: &Path,
    tool_dir: &Path,
    packages_dir: &Path,
    tool_name: &str,
    timeout: Duration,
    package_config: Option<&str>,
) -> Result<ProcessOutput, String> {
    let (cmd, cmd_args) = match runtime {
        RuntimeType::Python => {
            let python = find_python().await;
            (python, vec![script_path.to_string_lossy().to_string()])
        }
        RuntimeType::Node => {
            ("node".to_string(), vec![script_path.to_string_lossy().to_string()])
        }
        RuntimeType::PowerShell => {
            let ps = if cfg!(target_os = "windows") { "powershell" } else { "pwsh" };
            (ps.to_string(), vec![
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-File".to_string(),
                script_path.to_string_lossy().to_string(),
            ])
        }
        RuntimeType::Shell => {
            if cfg!(target_os = "windows") {
                ("cmd".to_string(), vec!["/C".to_string(), script_path.to_string_lossy().to_string()])
            } else {
                ("sh".to_string(), vec![script_path.to_string_lossy().to_string()])
            }
        }
        RuntimeType::Binary => {
            (script_path.to_string_lossy().to_string(), vec![])
        }
    };

    tracing::info!("Executing custom tool '{}': {} {:?}", tool_name, cmd, cmd_args);

    let mut command = tokio::process::Command::new(&cmd);
    command
        .args(&cmd_args)
        .current_dir(sandbox_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // Set environment variables
    command.env("CHITTY_TOOL_NAME", tool_name);
    command.env("CHITTY_SANDBOX_DIR", sandbox_dir);
    command.env("CHITTY_TOOL_DIR", tool_dir);
    command.env("PYTHONIOENCODING", "utf-8");

    // Inject package configuration (allowed resources + feature flags) if available
    if let Some(config_json) = package_config {
        command.env("CHITTY_PACKAGE_CONFIG", config_json);
    }

    // Add package paths to runtime search paths
    // Use platform-appropriate path separator (';' on Windows, ':' on Unix)
    let path_sep = if cfg!(target_os = "windows") { ";" } else { ":" };

    let python_packages = packages_dir.join("python").join(tool_name);
    if python_packages.exists() {
        if let Ok(existing) = std::env::var("PYTHONPATH") {
            command.env("PYTHONPATH", format!("{}{}{}", python_packages.display(), path_sep, existing));
        } else {
            command.env("PYTHONPATH", python_packages.to_string_lossy().to_string());
        }
    }

    let node_packages = packages_dir.join("node").join(tool_name);
    if node_packages.exists() {
        if let Ok(existing) = std::env::var("NODE_PATH") {
            command.env("NODE_PATH", format!("{}{}{}", node_packages.join("node_modules").display(), path_sep, existing));
        } else {
            command.env("NODE_PATH", node_packages.join("node_modules").to_string_lossy().to_string());
        }
    }

    // On Windows, prevent console window from flashing
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    let mut child = command.spawn().map_err(|e| {
        format!("Failed to start {} (is {} installed?): {}", tool_name, cmd, e)
    })?;

    // Write args to stdin
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(args_json.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }

    // Wait with timeout
    // Note: wait_with_output() consumes child, so we can't kill after timeout.
    // Instead, we grab the PID and use a separate kill approach if needed.
    let child_id = child.id();
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let success = output.status.success();

            tracing::info!("Custom tool '{}' completed: success={}, stdout={}chars, stderr={}chars",
                tool_name, success, stdout.len(), stderr.len());

            Ok(ProcessOutput { stdout, stderr, success })
        }
        Ok(Err(e)) => {
            Err(format!("Process error: {}", e))
        }
        Err(_) => {
            // Kill the timed-out process by PID if available
            if let Some(pid) = child_id {
                #[cfg(target_os = "windows")]
                { let _ = tokio::process::Command::new("taskkill").args(&["/F", "/PID", &pid.to_string()]).output().await; }
                #[cfg(not(target_os = "windows"))]
                { let _ = tokio::process::Command::new("kill").arg("-9").arg(pid.to_string()).output().await; }
            }
            Err(format!("Tool '{}' timed out after {}s", tool_name, timeout.as_secs()))
        }
    }
}

/// Parse the output from a custom tool process
fn parse_tool_output(stdout: &str, stderr: &str, process_success: bool) -> ToolResult {
    // Try parsing stdout as our expected JSON format
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        let success = v.get("success").and_then(|s| s.as_bool()).unwrap_or(process_success);
        let output = v.get("output").cloned().unwrap_or_else(|| {
            v.get("result").cloned().unwrap_or(serde_json::Value::String(stdout.to_string()))
        });
        let error = v.get("error").and_then(|e| e.as_str()).map(String::from);

        return ToolResult {
            success,
            output,
            error,
        };
    }

    // If stdout isn't JSON, treat raw text as output
    let max_chars = 8_000;
    let mut output_text = stdout.to_string();
    if !stderr.is_empty() && !process_success {
        output_text.push_str("\n--- stderr ---\n");
        output_text.push_str(stderr);
    }

    // Truncate if too long
    if output_text.len() > max_chars {
        let head_len = max_chars * 3 / 4;
        let tail_start = output_text.len().saturating_sub(max_chars / 4);
        output_text = format!(
            "{}\n\n... [truncated: {} total chars]\n\n{}",
            &output_text[..head_len],
            output_text.len(),
            &output_text[tail_start..]
        );
    }

    if process_success {
        ToolResult::ok(output_text)
    } else {
        ToolResult {
            success: false,
            output: serde_json::Value::String(output_text),
            error: Some(format!("Process exited with error. stderr: {}",
                &stderr[..stderr.len().min(500)])),
        }
    }
}

/// Find the Python interpreter on this system
async fn find_python() -> String {
    // Try python3 first (Linux/Mac), then python (Windows)
    for candidate in &["python3", "python", "py"] {
        let result = tokio::process::Command::new(candidate)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;

        if let Ok(status) = result {
            if status.success() {
                return candidate.to_string();
            }
        }
    }

    // Default fallback
    if cfg!(target_os = "windows") {
        "python".to_string()
    } else {
        "python3".to_string()
    }
}
