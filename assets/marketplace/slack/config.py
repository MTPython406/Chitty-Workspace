"""Slack package configuration enforcement helper."""
import os
import json
import re


def load_config():
    """Load package configuration from CHITTY_PACKAGE_CONFIG environment variable.

    Fails closed: malformed JSON returns empty config (all features default-enabled,
    no channel restrictions).
    """
    config_str = os.environ.get("CHITTY_PACKAGE_CONFIG", "")
    if not config_str:
        return {"features": {}, "resources": {}}
    try:
        config = json.loads(config_str)
        if not isinstance(config, dict):
            return {"features": {}, "resources": {}}
        config.setdefault("features", {})
        config.setdefault("resources", {})
        return config
    except (json.JSONDecodeError, TypeError, ValueError):
        return {"features": {}, "resources": {}}


def normalize_channel(channel_input):
    """Normalize a channel input: strip whitespace, strip leading #, lowercase.

    Supports both '#channel-name' and channel ID formats (e.g. 'C01ABC23DEF').
    Channel IDs (starting with C/G followed by alphanumerics) are returned as-is
    (not lowercased) since IDs are case-sensitive.
    """
    cleaned = channel_input.strip().lstrip("#").strip()
    # Channel IDs start with C or G followed by uppercase alphanumerics
    if re.match(r'^[CG][A-Z0-9]{8,}$', cleaned):
        return cleaned  # preserve case for IDs
    return cleaned.lower()


def check_channel_allowed(channel_name):
    """Check if a channel is in the allowed channels list.

    Case-insensitive comparison for channel names.
    Supports both #channel-name and channel ID formats in allowlist.
    Returns (allowed: bool, error: str or None)
    """
    config = load_config()
    allowed = config.get("resources", {}).get("channels", [])

    # Empty list = all channels allowed
    if not allowed:
        return True, None

    # Normalize the input channel
    clean_input = normalize_channel(channel_name)

    # Normalize each allowlist entry and compare case-insensitively
    for entry in allowed:
        clean_entry = normalize_channel(str(entry))
        if clean_input == clean_entry:
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
