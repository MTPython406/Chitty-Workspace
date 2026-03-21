"""
Package configuration helper for Chitty Workspace marketplace tools.
Reads CHITTY_PACKAGE_CONFIG env var to enforce allowed resources and feature flags.
"""

import os
import json


def get_package_config():
    """Load the package config from the CHITTY_PACKAGE_CONFIG env var."""
    raw = os.environ.get("CHITTY_PACKAGE_CONFIG", "")
    if not raw:
        return {"resources": {}, "features": {}}
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        return {"resources": {}, "features": {}}


def get_allowed_resources(resource_type):
    """Get list of allowed resource IDs for a resource type (e.g., 'datasets', 'buckets').
    Returns empty list if no restrictions configured (allow all).
    """
    config = get_package_config()
    resources = config.get("resources", {}).get(resource_type, [])
    # Resources can be strings or objects with 'id' field
    result = []
    for r in resources:
        if isinstance(r, str):
            result.append(r)
        elif isinstance(r, dict) and "id" in r:
            result.append(r["id"])
    return result


def is_feature_enabled(feature_id):
    """Check if a feature flag is enabled. Returns True if not configured (default allow)."""
    config = get_package_config()
    features = config.get("features", {})
    # If no features configured at all, default to True (no restrictions)
    if not features:
        return True
    return features.get(feature_id, True)


def check_resource_allowed(resource_type, resource_id):
    """Check if a specific resource is in the allowed list.
    Returns (allowed, error_message). If no restrictions configured, allows all.
    """
    allowed = get_allowed_resources(resource_type)
    if not allowed:
        # No restrictions configured — allow all
        return True, None
    if resource_id in allowed:
        return True, None
    return False, (
        f"Resource '{resource_id}' is not in the allowed {resource_type}. "
        f"Allowed: {', '.join(allowed)}. "
        f"Configure allowed {resource_type} in Settings > Marketplace > Google Cloud Platform."
    )


def check_feature_allowed(feature_id, action_label=None):
    """Check if a feature flag is enabled.
    Returns (allowed, error_message).
    """
    if is_feature_enabled(feature_id):
        return True, None
    label = action_label or feature_id
    return False, (
        f"'{label}' is disabled in package configuration. "
        f"Enable it in Settings > Marketplace > Google Cloud Platform > Feature Flags."
    )
