//! Marketplace API client — connects to the Chitty Marketplace registry at chitty.ai
//! for browsing, searching, and downloading tool packages.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

const MARKETPLACE_URL: &str = "https://marketplace.chitty.ai";

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
    /// Returns the path where the package was installed.
    pub async fn install_package(&self, name: &str, version: &str, marketplace_dir: &Path) -> Result<std::path::PathBuf> {
        // Download the .tar.gz package
        let resp = self.client
            .get(format!("{}/api/v1/packages/{}/{}/download", self.base_url, name, version))
            .send()
            .await?;

        let bytes = resp.error_for_status()?.bytes().await?;
        tracing::info!("Downloaded {}@{} ({} bytes)", name, version, bytes.len());

        // Extract to marketplace dir
        let pkg_dir = marketplace_dir.join(name);
        std::fs::create_dir_all(&pkg_dir)?;

        // Write the tar.gz and extract
        let tar_path = pkg_dir.join(format!("{}.tar.gz", version));
        std::fs::write(&tar_path, &bytes)?;

        // Extract using tar (available on Windows via Git Bash / WSL)
        let output = tokio::process::Command::new("tar")
            .args(["-xzf", &tar_path.to_string_lossy(), "-C", &pkg_dir.to_string_lossy()])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to extract package: {}", stderr);
        }

        // Clean up the tar.gz
        let _ = std::fs::remove_file(&tar_path);

        tracing::info!("Installed {}@{} to {:?}", name, version, pkg_dir);
        Ok(pkg_dir)
    }
}
