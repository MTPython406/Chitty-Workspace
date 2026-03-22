//! TLS certificate generation for localhost OAuth callbacks
//!
//! Generates a self-signed certificate for localhost so OAuth providers
//! that require HTTPS redirect URIs (e.g., Slack) can redirect back
//! to Chitty Workspace running on the user's machine.
//!
//! The certificate is generated once and cached in ~/.chitty-workspace/tls/

use anyhow::Result;
use std::path::{Path, PathBuf};

/// TLS certificate and key paths
pub struct TlsCerts {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

/// Ensure a self-signed TLS certificate exists for localhost.
/// Generates one if it doesn't exist or if it's expired/invalid.
/// Returns paths to the cert and key PEM files.
pub fn ensure_localhost_cert(data_dir: &Path) -> Result<TlsCerts> {
    let tls_dir = data_dir.join("tls");
    let cert_path = tls_dir.join("localhost.crt");
    let key_path = tls_dir.join("localhost.key");

    // If both files exist and cert is still valid, reuse them
    if cert_path.exists() && key_path.exists() {
        if is_cert_valid(&cert_path) {
            tracing::info!("Reusing existing localhost TLS certificate");
            return Ok(TlsCerts { cert_path, key_path });
        }
        tracing::info!("Existing TLS certificate expired, regenerating");
    }

    // Generate a new self-signed certificate
    std::fs::create_dir_all(&tls_dir)?;
    generate_self_signed_cert(&cert_path, &key_path)?;

    Ok(TlsCerts { cert_path, key_path })
}

/// Check if an existing certificate file is still valid (not expired).
fn is_cert_valid(cert_path: &Path) -> bool {
    // Simple heuristic: if the file was created less than 365 days ago, it's valid.
    // rcgen generates certs valid for 365 days by default.
    match std::fs::metadata(cert_path) {
        Ok(meta) => {
            if let Ok(created) = meta.created().or_else(|_| meta.modified()) {
                let age = std::time::SystemTime::now()
                    .duration_since(created)
                    .unwrap_or_default();
                age < std::time::Duration::from_secs(300 * 24 * 60 * 60) // 300 days
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

/// Generate a self-signed certificate for localhost using rcgen.
fn generate_self_signed_cert(cert_path: &Path, key_path: &Path) -> Result<()> {
    use rcgen::{CertificateParams, KeyPair, SanType};

    tracing::info!("Generating self-signed TLS certificate for localhost OAuth callbacks");

    let mut params = CertificateParams::new(vec!["localhost".to_string()])?;
    params.subject_alt_names = vec![
        SanType::DnsName("localhost".try_into()?),
        SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))),
    ];

    let key_pair = KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    // Write PEM files
    std::fs::write(cert_path, cert.pem())?;
    std::fs::write(key_path, key_pair.serialize_pem())?;

    tracing::info!("TLS certificate written to {:?}", cert_path);
    Ok(())
}
