"""Slack package configuration enforcement helper."""
import os
import json

def load_config():
    """Load package configuration from CHITTY_PACKAGE_CONFIG environment variable."""
    config_str = os.environ.get("CHITTY_PACKAGE_CONFIG", "{}")
    try:
        return json.loads(config_str)
    except json.JSONDecodeError:
        return {}

def check_channel_allowed(channel_name):
    """Check if a channel is in the allowed channels list.
    Returns (allowed: bool, error: str or None)
    """
    config = load_config()
    allowed = config.get("resources", {}).get("channels", [])

    # Empty list = all channels allowed
    if not allowed:
        return True, None

    # Strip # prefix for comparison
    clean = channel_name.lstrip("#")
    if clean in allowed or channel_name in allowed:
        return True, None

    return False, f"Channel '{channel_name}' is not in the allowed channels list. Allowed: {', '.join(allowed)}"

def check_feature_allowed(feature_id):
    """Check if a feature flag is enabled.
    Returns (allowed: bool, error: str or None)
    """
    config = load_config()
    features = config.get("features", {})

    # Default to True if feature not in config (backwards compatible)
    enabled = features.get(feature_id, True)
    if enabled:
        return True, None

    return False, f"Feature '{feature_id}' is disabled in package configuration."
