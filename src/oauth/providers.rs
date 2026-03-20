//! OAuth provider configurations
//!
//! Each provider's client_id is PUBLIC and safe to ship in open source.
//! Desktop OAuth with PKCE does NOT require a client_secret.

use super::OAuthConfig;

/// Google OAuth config — covers Gmail, Calendar, Drive, Contacts
pub fn google_config() -> OAuthConfig {
    OAuthConfig {
        provider: "google".into(),
        // TODO: Replace with real client_id from GCP project "Chitty Workspace"
        // Created at: console.cloud.google.com → Credentials → OAuth 2.0 Client IDs → Desktop app
        client_id: "PLACEHOLDER.apps.googleusercontent.com".into(),
        auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
        token_url: "https://oauth2.googleapis.com/token".into(),
        scopes: vec![
            "https://www.googleapis.com/auth/gmail.readonly".into(),
            "https://www.googleapis.com/auth/gmail.send".into(),
            "https://www.googleapis.com/auth/gmail.modify".into(),
            "https://www.googleapis.com/auth/calendar.readonly".into(),
            "https://www.googleapis.com/auth/calendar.events".into(),
            "https://www.googleapis.com/auth/drive.readonly".into(),
            "https://www.googleapis.com/auth/contacts.readonly".into(),
            "openid".into(),
            "email".into(),
            "profile".into(),
        ],
        redirect_uri: "http://localhost:8770/oauth/callback".into(),
    }
}

/// Microsoft OAuth config — covers Outlook, OneDrive, Teams, Calendar
pub fn microsoft_config() -> OAuthConfig {
    OAuthConfig {
        provider: "microsoft".into(),
        // TODO: Replace with real client_id from Azure AD app registration
        client_id: "PLACEHOLDER".into(),
        auth_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize".into(),
        token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token".into(),
        scopes: vec![
            "Mail.Read".into(),
            "Mail.Send".into(),
            "Calendars.ReadWrite".into(),
            "Files.Read.All".into(),
            "Contacts.Read".into(),
            "User.Read".into(),
            "offline_access".into(),
        ],
        redirect_uri: "http://localhost:8770/oauth/callback".into(),
    }
}

/// GitHub OAuth config — covers Repos, Issues, PRs
pub fn github_config() -> OAuthConfig {
    OAuthConfig {
        provider: "github".into(),
        // TODO: Replace with real client_id from GitHub OAuth App
        client_id: "PLACEHOLDER".into(),
        auth_url: "https://github.com/login/oauth/authorize".into(),
        token_url: "https://github.com/login/oauth/access_token".into(),
        scopes: vec![
            "repo".into(),
            "read:user".into(),
            "read:org".into(),
        ],
        redirect_uri: "http://localhost:8770/oauth/callback".into(),
    }
}

/// Look up config by provider name
pub fn get_config(provider: &str) -> Option<OAuthConfig> {
    match provider {
        "google" => Some(google_config()),
        "microsoft" => Some(microsoft_config()),
        "github" => Some(github_config()),
        _ => None,
    }
}
