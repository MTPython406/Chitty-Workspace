//! OAuth provider configurations
//!
//! ALL integrations are user-managed. No hardcoded credentials.
//! Users create their own OAuth apps (Google Cloud, Azure, GitHub, Slack, etc.)
//! and paste their client_id + client_secret into Chitty Settings.
//! Credentials are stored in the OS keyring — never in source code.
//!
//! Each provider template defines the OAuth URLs and default scopes.
//! The client_id and client_secret come from the user's keyring.

use super::OAuthConfig;

/// Known provider templates — OAuth URLs and default scopes.
/// client_id and client_secret always come from the user's keyring.
pub struct ProviderTemplate {
    pub provider: &'static str,
    pub display_name: &'static str,
    pub description: &'static str,
    pub auth_url: &'static str,
    pub token_url: &'static str,
    pub default_scopes: &'static [&'static str],
    pub setup_url: &'static str,  // Where user creates their OAuth app
    pub setup_instructions: &'static str,
}

pub const GOOGLE: ProviderTemplate = ProviderTemplate {
    provider: "google",
    display_name: "Google",
    description: "Gmail, Calendar, Drive, Contacts",
    auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
    token_url: "https://oauth2.googleapis.com/token",
    default_scopes: &[
        "https://www.googleapis.com/auth/gmail.readonly",
        "https://www.googleapis.com/auth/gmail.send",
        "https://www.googleapis.com/auth/gmail.modify",
        "https://www.googleapis.com/auth/calendar.readonly",
        "https://www.googleapis.com/auth/calendar.events",
        "https://www.googleapis.com/auth/drive.readonly",
        "https://www.googleapis.com/auth/contacts.readonly",
        "openid", "email", "profile",
    ],
    setup_url: "https://console.cloud.google.com/apis/credentials",
    setup_instructions: "1. Go to Google Cloud Console → APIs & Services → Credentials\n\
        2. Create a new project (or select existing)\n\
        3. Enable APIs: Gmail, Calendar, Drive, People\n\
        4. Create OAuth consent screen (External, add your email as test user)\n\
        5. Create Credentials → OAuth client ID → Desktop app\n\
        6. Copy the Client ID and Client Secret",
};

pub const MICROSOFT: ProviderTemplate = ProviderTemplate {
    provider: "microsoft",
    display_name: "Microsoft",
    description: "Outlook, OneDrive, Teams, Calendar",
    auth_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
    token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
    default_scopes: &[
        "Mail.Read", "Mail.Send", "Calendars.ReadWrite",
        "Files.Read.All", "Contacts.Read", "User.Read", "offline_access",
    ],
    setup_url: "https://portal.azure.com/#view/Microsoft_AAD_RegisteredApps/ApplicationsListBlade",
    setup_instructions: "1. Go to Azure Portal → App registrations → New registration\n\
        2. Name: Chitty Workspace, Accounts: Any org + personal\n\
        3. Redirect URI: Public client → http://localhost:8770/oauth/callback\n\
        4. Copy the Application (client) ID\n\
        5. Certificates & secrets → New client secret → copy value",
};

pub const GITHUB: ProviderTemplate = ProviderTemplate {
    provider: "github",
    display_name: "GitHub",
    description: "Repos, Issues, Pull Requests",
    auth_url: "https://github.com/login/oauth/authorize",
    token_url: "https://github.com/login/oauth/access_token",
    default_scopes: &["repo", "read:user", "read:org"],
    setup_url: "https://github.com/settings/developers",
    setup_instructions: "1. Go to GitHub → Settings → Developer settings → OAuth Apps\n\
        2. New OAuth App\n\
        3. Application name: Chitty Workspace\n\
        4. Authorization callback URL: http://localhost:8770/oauth/callback\n\
        5. Copy the Client ID and Client Secret",
};

pub const SLACK: ProviderTemplate = ProviderTemplate {
    provider: "slack",
    display_name: "Slack",
    description: "Messages, Channels, Users",
    auth_url: "https://slack.com/oauth/v2/authorize",
    token_url: "https://slack.com/api/oauth.v2.access",
    default_scopes: &[
        "chat:write", "channels:read", "channels:history", "users:read",
        "app_mentions:read", "im:history", "im:read", "commands",
    ],
    setup_url: "https://api.slack.com/apps",
    setup_instructions: "1. Go to api.slack.com/apps → Create New App → From scratch\n\
        2. Name: Chitty Workspace, select your workspace\n\
        3. OAuth & Permissions → Redirect URLs → Add: http://localhost:8770/oauth/callback\n\
        4. Bot Token Scopes → Add: chat:write, channels:read, channels:history, users:read, app_mentions:read, im:history, im:read, commands\n\
        5. Socket Mode → Enable Socket Mode → Generate App-Level Token (scope: connections:write)\n\
        6. Event Subscriptions → Enable → Subscribe to: app_mention, message.im\n\
        7. Basic Information → Copy Client ID and Client Secret\n\
        8. Install App to workspace",
};

/// All known provider templates
pub const ALL_TEMPLATES: &[&ProviderTemplate] = &[&GOOGLE, &MICROSOFT, &GITHUB, &SLACK];

/// Look up a provider template by name
pub fn get_template(provider: &str) -> Option<&'static ProviderTemplate> {
    ALL_TEMPLATES.iter().find(|t| t.provider == provider).copied()
}

/// First-party provider client_id (we manage the OAuth app)
/// Google is first-party because we host the Chitty Marketplace on GCP.
const GOOGLE_CLIENT_ID: &str = "706685776923-gp53edqle1d36fph5o8durm5iqc1ehm2.apps.googleusercontent.com";

/// Build an OAuthConfig from a template + credentials.
/// Google: first-party (client_id shipped, secret in keyring from installer)
/// Everything else: user provides their own client_id + client_secret
pub fn get_config(provider: &str) -> Option<OAuthConfig> {
    let template = get_template(provider)?;

    // Google is first-party — client_id is shipped, secret from keyring
    let (client_id, client_secret) = if provider == "google" {
        let secret = crate::config::get_api_key("oauth_google_client_secret")
            .ok()
            .flatten();
        (GOOGLE_CLIENT_ID.to_string(), secret)
    } else {
        // User-managed — both credentials from keyring
        let id = crate::config::get_api_key(&format!("oauth_{}_client_id", provider))
            .ok()
            .flatten()?; // Return None if not configured
        let secret = crate::config::get_api_key(&format!("oauth_{}_client_secret", provider))
            .ok()
            .flatten();
        (id, secret)
    };

    Some(OAuthConfig {
        provider: provider.to_string(),
        client_id,
        client_secret,
        auth_url: template.auth_url.to_string(),
        token_url: template.token_url.to_string(),
        scopes: template.default_scopes.iter().map(|s| s.to_string()).collect(),
        redirect_uri: "http://localhost:8770/oauth/callback".to_string(),
    })
}
