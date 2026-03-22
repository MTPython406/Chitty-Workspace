"""Slack authentication helper — reads bot token from OS keyring."""
import os
import sys

def get_bot_token():
    """Get the Slack bot token from OS keyring or environment variable."""
    # First check environment variable (set by executor or user)
    token = os.environ.get("SLACK_BOT_TOKEN")
    if token:
        return token

    # Try OS keyring (where OAuth tokens are stored by Chitty)
    try:
        import keyring
        token = keyring.get_password("chitty-workspace", "oauth_slack_access_token")
        if token:
            return token
    except ImportError:
        pass
    except Exception:
        pass

    return None

def get_app_token():
    """Get the Slack App-Level Token for Socket Mode."""
    token = os.environ.get("SLACK_APP_TOKEN")
    if token:
        return token

    try:
        import keyring
        token = keyring.get_password("chitty-workspace", "slack_app_token")
        if token:
            return token
    except ImportError:
        pass
    except Exception:
        pass

    return None

def require_bot_token():
    """Get bot token or exit with error."""
    token = get_bot_token()
    if not token:
        import json
        print(json.dumps({
            "success": False,
            "error": "Slack bot token not found. Run the Slack package setup wizard first, or set SLACK_BOT_TOKEN environment variable."
        }))
        sys.exit(0)
    return token
