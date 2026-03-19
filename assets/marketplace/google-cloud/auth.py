"""
Google Cloud authentication helper for Chitty Workspace marketplace tools.
Uses the gcloud CLI credentials — no keys are stored by Chitty.
"""

import subprocess
import json
import sys


def get_access_token():
    """Get OAuth2 access token from gcloud CLI."""
    try:
        result = subprocess.run(
            ["gcloud", "auth", "print-access-token"],
            capture_output=True, text=True, timeout=10
        )
        if result.returncode != 0:
            return None, f"gcloud auth failed: {result.stderr.strip()}"
        return result.stdout.strip(), None
    except FileNotFoundError:
        return None, "gcloud CLI not found. Install: https://cloud.google.com/sdk/docs/install"
    except subprocess.TimeoutExpired:
        return None, "gcloud auth timed out"


def get_project_id():
    """Get the default GCP project from gcloud config."""
    try:
        result = subprocess.run(
            ["gcloud", "config", "get-value", "project"],
            capture_output=True, text=True, timeout=10
        )
        if result.returncode != 0 or not result.stdout.strip():
            return None, "No default project set. Run: gcloud config set project PROJECT_ID"
        return result.stdout.strip(), None
    except FileNotFoundError:
        return None, "gcloud CLI not found"


def check_auth():
    """Check if user is authenticated with gcloud."""
    token, err = get_access_token()
    if token:
        project, proj_err = get_project_id()
        return {
            "authenticated": True,
            "project": project,
            "project_error": proj_err
        }
    return {
        "authenticated": False,
        "error": err
    }


def auth_headers():
    """Get HTTP headers with Bearer token for GCP API calls."""
    token, err = get_access_token()
    if not token:
        raise RuntimeError(f"Not authenticated: {err}")
    return {
        "Authorization": f"Bearer {token}",
        "Content-Type": "application/json"
    }


if __name__ == "__main__":
    # When run directly, check auth status
    status = check_auth()
    print(json.dumps(status, indent=2))
