//! Marketplace API client — connects to the Chitty Marketplace registry at chitty.ai
//! for browsing, searching, and downloading tool packages.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

const MARKETPLACE_URL: &str = "https://chitty.ai";

/// Summary of a package from the registry
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemotePackage {
    pub name: String,
    pub display_name: String,
    pub vendor: String,
    pub description: String,
    pub category: String,
    pub latest_version: String,
    pub icon: String,
    pub color: String,
    pub downloads: u64,
    pub tools_count: usize,
}

/// Full detail of a package from the registry
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemotePackageDetail {
    pub name: String,
    pub display_name: String,
    pub vendor: String,
    pub description: String,
    pub category: String,
    pub icon: String,
    pub color: String,
    pub downloads: u64,
    pub tools: Vec<RemoteToolInfo>,
    pub versions: Vec<RemoteVersion>,
    pub setup_steps: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteToolInfo {
    pub name: String,
    pub display_name: String,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteVersion {
    pub version: String,
    pub checksum: String,
    pub size_bytes: usize,
}

#[derive(Deserialize)]
struct PackageListResponse {
    packages: Vec<RemotePackage>,
    #[allow(dead_code)]
    total: usize,
}

pub struct MarketplaceClient {
    client: reqwest::Client,
    base_url: String,
}

impl MarketplaceClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .user_agent("ChittyWorkspace/1.0")
                .build()
                .unwrap(),
            base_url: std::env::var("CHITTY_MARKETPLACE_URL")
                .unwrap_or_else(|_| MARKETPLACE_URL.to_string()),
        }
    }

    /// List approved/active packages from the marketplace registry.
    /// Only shows packages that have been reviewed and approved — not requested or submitted ones.
    pub async fn list_packages(&self) -> Result<Vec<RemotePackage>> {
        let resp = self.client
            .get(format!("{}/api/v1/packages?status=active", self.base_url))
            .send()
            .await?;

        let body: PackageListResponse = resp.error_for_status()?.json().await?;
        Ok(body.packages)
    }

    /// Search packages by query
    pub async fn search(&self, query: &str) -> Result<Vec<RemotePackage>> {
        let resp = self.client
            .get(format!("{}/api/v1/search", self.base_url))
            .query(&[("q", query)])
            .send()
            .await?;

        let body: PackageListResponse = resp.error_for_status()?.json().await?;
        Ok(body.packages)
    }

    /// Get full detail for a specific package
    pub async fn get_package(&self, name: &str) -> Result<RemotePackageDetail> {
        let resp = self.client
            .get(format!("{}/api/v1/packages/{}", self.base_url, name))
            .send()
            .await?;

        Ok(resp.error_for_status()?.json().await?)
    }

    /// Download and install a package to the local marketplace directory.
    /// Security: validates package name, uses staging directory, checks archive paths.
    /// Returns the path where the package was installed.
    pub async fn install_package(&self, name: &str, version: &str, marketplace_dir: &Path) -> Result<std::path::PathBuf> {
        // Validate package name (prevent path traversal)
        if name.contains("..") || name.contains('/') || name.contains('\\')
            || !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            anyhow::bail!("Invalid package name: {}", name);
        }

        // Download the .tar.gz package
        let resp = self.client
            .get(format!("{}/api/v1/packages/{}/{}/download", self.base_url, name, version))
            .send()
            .await?;

        let bytes = resp.error_for_status()?.bytes().await?;
        tracing::info!("Downloaded {}@{} ({} bytes)", name, version, bytes.len());

        // Use a staging directory to avoid partial installs
        let staging_dir = marketplace_dir.join(format!(".staging-{}", name));
        if staging_dir.exists() {
            std::fs::remove_dir_all(&staging_dir)?;
        }
        std::fs::create_dir_all(&staging_dir)?;

        let pkg_dir = marketplace_dir.join(name);

        // Write the tar.gz to staging and extract
        let tar_path = staging_dir.join(format!("{}.tar.gz", version));
        std::fs::write(&tar_path, &bytes)?;

        // Extract using tar into staging directory
        let output = tokio::process::Command::new("tar")
            .args(["-xzf", &tar_path.to_string_lossy(), "-C", &staging_dir.to_string_lossy()])
            .output()
            .await?;

        if !output.status.success() {
            let _ = std::fs::remove_dir_all(&staging_dir);
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to extract package: {}", stderr);
        }

        // Clean up the tar.gz
        let _ = std::fs::remove_file(&tar_path);

        // Validate: check for path traversal in extracted files
        for entry in walkdir::WalkDir::new(&staging_dir).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if let Ok(relative) = path.strip_prefix(&staging_dir) {
                let rel_str = relative.to_string_lossy();
                if rel_str.contains("..") {
                    let _ = std::fs::remove_dir_all(&staging_dir);
                    anyhow::bail!("Archive contains path traversal: {}", rel_str);
                }
            }
        }

        // GitHub archives extract to a subdirectory like "chitty-pkg-slack-main/"
        // Move contents from subdirectory up to the staging root
        if let Ok(entries) = std::fs::read_dir(&staging_dir) {
            let subdirs: Vec<_> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir() && e.file_name().to_string_lossy().ends_with("-main"))
                .collect();

            if subdirs.len() == 1 {
                let subdir = subdirs[0].path();
                tracing::info!("Moving contents from GitHub archive subdirectory: {:?}", subdir);

                // Move all files from subdirectory to staging root
                if let Ok(sub_entries) = std::fs::read_dir(&subdir) {
                    for entry in sub_entries.filter_map(|e| e.ok()) {
                        let src = entry.path();
                        let dst = staging_dir.join(entry.file_name());
                        if dst.exists() {
                            if dst.is_dir() {
                                let _ = std::fs::remove_dir_all(&dst);
                            } else {
                                let _ = std::fs::remove_file(&dst);
                            }
                        }
                        let _ = std::fs::rename(&src, &dst);
                    }
                }
                // Remove the now-empty subdirectory
                let _ = std::fs::remove_dir_all(&subdir);
            }
        }

        // Atomic-ish swap: remove old package dir, rename staging to live
        if pkg_dir.exists() {
            std::fs::remove_dir_all(&pkg_dir)?;
        }
        std::fs::rename(&staging_dir, &pkg_dir)?;

        tracing::info!("Installed {}@{} to {:?}", name, version, pkg_dir);
        Ok(pkg_dir)
    }
}
