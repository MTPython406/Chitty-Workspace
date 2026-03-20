//! OAuth 2.0 PKCE Integration Framework
//!
//! Handles OAuth flows entirely on the client machine — no server needed.
//! Uses PKCE (Proof Key for Code Exchange) so client_secret is NOT required.
//! Tokens are stored in the OS keyring (Windows Credential Manager).
//!
//! Supported providers:
//! - Google (Gmail, Calendar, Drive, Contacts) — Desktop PKCE
//! - Microsoft (Outlook, OneDrive, Teams) — Desktop PKCE (future)
//! - GitHub (Repos, Issues, PRs) — Device Flow (future)

pub mod providers;

use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// OAuth provider configuration — shipped in the binary, public client_id
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub provider: String,
    pub client_id: String,
    /// Optional client_secret — required by Google even for Desktop apps.
    /// For open source: stored in OS keyring, not in code.
    /// Users set it once via Settings or it's bundled in the installer.
    pub client_secret: Option<String>,
    pub auth_url: String,
    pub token_url: String,
    pub scopes: Vec<String>,
    pub redirect_uri: String,
}

/// Tokens returned after a successful OAuth flow
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,     // Unix timestamp
    pub token_type: Option<String>,
    pub scopes: Vec<String>,
}

/// Pending OAuth flow (stored in memory while user is authenticating)
#[derive(Debug, Clone)]
pub struct PendingFlow {
    pub provider: String,
    pub code_verifier: String,
    pub created_at: std::time::Instant,
}

/// In-memory store for pending OAuth flows (keyed by state parameter)
pub type PendingFlows = Arc<Mutex<HashMap<String, PendingFlow>>>;

/// Integration status for UI display
#[derive(Debug, Clone, serde::Serialize)]
pub struct IntegrationStatus {
    pub provider: String,
    pub display_name: String,
    pub description: String,
    pub connected: bool,
    pub configured: bool,       // Has OAuth credentials been set up
    pub services: Vec<String>,
    pub scopes: Vec<String>,
    pub setup_url: String,
    pub setup_instructions: String,
}

// ---------------------------------------------------------------------------
// PKCE Helpers
// ---------------------------------------------------------------------------

/// Generate a cryptographically random code_verifier (43-128 chars, URL-safe)
pub fn generate_code_verifier() -> String {
    let mut rng = rand::rng();
    let bytes: Vec<u8> = (0..32).map(|_| rng.random::<u8>()).collect();
    URL_SAFE_NO_PAD.encode(&bytes)
}

/// Derive code_challenge from code_verifier using S256
pub fn generate_code_challenge(verifier: &str) -> String {
    let hash = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(&hash)
}

/// Generate a random state parameter for CSRF protection
pub fn generate_state() -> String {
    let mut rng = rand::rng();
    let bytes: Vec<u8> = (0..16).map(|_| rng.random::<u8>()).collect();
    URL_SAFE_NO_PAD.encode(&bytes)
}

// ---------------------------------------------------------------------------
// OAuth Flow
// ---------------------------------------------------------------------------

/// Build the authorization URL that opens in the user's browser
pub fn build_auth_url(config: &OAuthConfig, state: &str, code_challenge: &str) -> String {
    let scopes = config.scopes.join(" ");
    format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&scope={}&state={}&code_challenge={}&code_challenge_method=S256&access_type=offline&prompt=consent",
        config.auth_url,
        urlencoding::encode(&config.client_id),
        urlencoding::encode(&config.redirect_uri),
        urlencoding::encode(&scopes),
        urlencoding::encode(state),
        urlencoding::encode(code_challenge),
    )
}

/// Exchange authorization code for tokens (PKCE + client_secret for Google)
pub async fn exchange_code(
    config: &OAuthConfig,
    code: &str,
    code_verifier: &str,
) -> Result<OAuthTokens> {
    let client = reqwest::Client::new();

    let empty = String::new();
    let secret = config.client_secret.as_ref().unwrap_or(&empty);

    let mut params: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", &config.redirect_uri),
        ("client_id", &config.client_id),
        ("code_verifier", code_verifier),
    ];
    if !secret.is_empty() {
        params.push(("client_secret", secret));
    }

    let resp = client
        .post(&config.token_url)
        .form(&params)
        .send()
        .await
        .context("Failed to exchange auth code")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Token exchange failed ({}): {}", status, body);
    }

    let body: serde_json::Value = resp.json().await.context("Failed to parse token response")?;

    let access_token = body["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No access_token in response"))?
        .to_string();

    let refresh_token = body["refresh_token"].as_str().map(|s| s.to_string());

    let expires_in = body["expires_in"].as_i64().unwrap_or(3600);
    let expires_at = chrono::Utc::now().timestamp() + expires_in;

    let scopes = body["scope"]
        .as_str()
        .map(|s| s.split_whitespace().map(|s| s.to_string()).collect())
        .unwrap_or_else(|| config.scopes.clone());

    Ok(OAuthTokens {
        access_token,
        refresh_token,
        expires_at: Some(expires_at),
        token_type: body["token_type"].as_str().map(|s| s.to_string()),
        scopes,
    })
}

/// Refresh an expired access token using the refresh token
pub async fn refresh_access_token(
    config: &OAuthConfig,
    refresh_token: &str,
) -> Result<OAuthTokens> {
    let client = reqwest::Client::new();

    let empty = String::new();
    let secret = config.client_secret.as_ref().unwrap_or(&empty);

    let mut params: Vec<(&str, &str)> = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", &config.client_id),
    ];
    if !secret.is_empty() {
        params.push(("client_secret", secret));
    }

    let resp = client
        .post(&config.token_url)
        .form(&params)
        .send()
        .await
        .context("Failed to refresh token")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Token refresh failed ({}): {}", status, body);
    }

    let body: serde_json::Value = resp.json().await.context("Failed to parse refresh response")?;

    let access_token = body["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No access_token in refresh response"))?
        .to_string();

    let expires_in = body["expires_in"].as_i64().unwrap_or(3600);
    let expires_at = chrono::Utc::now().timestamp() + expires_in;

    Ok(OAuthTokens {
        access_token,
        refresh_token: body["refresh_token"]
            .as_str()
            .map(|s| s.to_string())
            .or_else(|| Some(refresh_token.to_string())),
        expires_at: Some(expires_at),
        token_type: body["token_type"].as_str().map(|s| s.to_string()),
        scopes: body["scope"]
            .as_str()
            .map(|s| s.split_whitespace().map(|s| s.to_string()).collect())
            .unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------
// Token Storage (OS Keyring)
// ---------------------------------------------------------------------------

/// Keyring key names for a provider
fn key_access(provider: &str) -> String {
    format!("oauth_{}_access_token", provider)
}
fn key_refresh(provider: &str) -> String {
    format!("oauth_{}_refresh_token", provider)
}
fn key_expires(provider: &str) -> String {
    format!("oauth_{}_expires_at", provider)
}
fn key_scopes(provider: &str) -> String {
    format!("oauth_{}_scopes", provider)
}

/// Save OAuth tokens to OS keyring
pub fn save_tokens(provider: &str, tokens: &OAuthTokens) -> Result<()> {
    config::set_api_key(&key_access(provider), &tokens.access_token)?;
    if let Some(ref rt) = tokens.refresh_token {
        config::set_api_key(&key_refresh(provider), rt)?;
    }
    if let Some(exp) = tokens.expires_at {
        config::set_api_key(&key_expires(provider), &exp.to_string())?;
    }
    let scope_str = tokens.scopes.join(" ");
    if !scope_str.is_empty() {
        config::set_api_key(&key_scopes(provider), &scope_str)?;
    }
    tracing::info!("OAuth tokens saved for provider: {}", provider);
    Ok(())
}

/// Check if a provider has valid (non-expired) tokens
pub fn is_connected(provider: &str) -> bool {
    let has_token = config::get_api_key(&key_access(provider))
        .ok()
        .flatten()
        .is_some();
    if !has_token {
        return false;
    }
    // Check expiry — if expired but we have refresh token, still "connected"
    if let Some(expires_str) = config::get_api_key(&key_expires(provider)).ok().flatten() {
        if let Ok(expires_at) = expires_str.parse::<i64>() {
            let now = chrono::Utc::now().timestamp();
            if now >= expires_at {
                // Expired — still connected if we have refresh token
                return config::get_api_key(&key_refresh(provider))
                    .ok()
                    .flatten()
                    .is_some();
            }
        }
    }
    true
}

/// Get a valid access token, auto-refreshing if expired
pub async fn get_access_token(provider: &str) -> Result<String> {
    let access = config::get_api_key(&key_access(provider))?
        .ok_or_else(|| anyhow::anyhow!("{} not connected. Go to Settings → Integrations → Connect.", provider))?;

    // Check if expired
    if let Some(expires_str) = config::get_api_key(&key_expires(provider))? {
        if let Ok(expires_at) = expires_str.parse::<i64>() {
            let now = chrono::Utc::now().timestamp();
            if now >= expires_at - 60 {
                // Expired (or within 60s of expiry) — refresh
                let refresh = config::get_api_key(&key_refresh(provider))?
                    .ok_or_else(|| anyhow::anyhow!("No refresh token for {}. Reconnect in Settings.", provider))?;

                let oauth_config = providers::get_config(provider)
                    .ok_or_else(|| anyhow::anyhow!("Unknown provider: {}", provider))?;

                tracing::info!("Refreshing expired OAuth token for {}", provider);
                let new_tokens = refresh_access_token(&oauth_config, &refresh).await?;
                save_tokens(provider, &new_tokens)?;
                return Ok(new_tokens.access_token);
            }
        }
    }

    Ok(access)
}

/// Remove all tokens for a provider (disconnect)
pub fn disconnect(provider: &str) -> Result<()> {
    let _ = config::delete_api_key(&key_access(provider));
    let _ = config::delete_api_key(&key_refresh(provider));
    let _ = config::delete_api_key(&key_expires(provider));
    let _ = config::delete_api_key(&key_scopes(provider));
    tracing::info!("OAuth disconnected for provider: {}", provider);
    Ok(())
}

/// Check if a provider has OAuth credentials configured
/// Google is always configured (first-party, client_id shipped in code)
/// Others need user to paste their client_id
pub fn is_configured(provider: &str) -> bool {
    if provider == "google" {
        return true; // First-party — client_id shipped
    }
    config::get_api_key(&format!("oauth_{}_client_id", provider))
        .ok()
        .flatten()
        .is_some()
}

/// Get integration status for first-party providers only (Google).
/// Marketplace integrations (Microsoft, GitHub, Slack) are shown in the Marketplace tab.
pub fn get_all_status() -> Vec<IntegrationStatus> {
    providers::ALL_TEMPLATES
        .iter()
        .filter(|t| t.provider == "google") // Only first-party integrations
        .map(|t| {
            let configured = is_configured(t.provider);
            IntegrationStatus {
                provider: t.provider.to_string(),
                display_name: t.display_name.to_string(),
                description: t.description.to_string(),
                connected: configured && is_connected(t.provider),
                configured,
                services: t.description.split(", ").map(|s| s.to_string()).collect(),
                scopes: t.default_scopes.iter().map(|s| s.to_string()).collect(),
                setup_url: t.setup_url.to_string(),
                setup_instructions: t.setup_instructions.to_string(),
            }
        })
        .collect()
}
