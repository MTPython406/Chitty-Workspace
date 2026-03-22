#!/usr/bin/env python3
"""Slack tool: List channels in the connected workspace."""
import os
import sys
import urllib.request
import urllib.error

# Add parent directory to path for shared helpers (fallback)
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

try:
    from chitty_sdk import tool_main, require_slack_token
except ImportError:
    from auth import require_bot_token as require_slack_token
    from chitty_sdk_shim import tool_main


@tool_main
def main(args):
    import json
    token = require_slack_token()

    exclude_archived = args.get("exclude_archived", True)
    limit = min(args.get("limit", 100), 1000)

    url = f"https://slack.com/api/conversations.list?exclude_archived={str(exclude_archived).lower()}&limit={limit}&types=public_channel"

    req = urllib.request.Request(url, headers={
        "Authorization": f"Bearer {token}",
        "Content-Type": "application/json",
    })

    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
            data = json.loads(resp.read().decode())

        if not data.get("ok"):
            from chitty_sdk import error
            error(data.get("error", "Unknown Slack API error"))
            return

        channels = []
        for ch in data.get("channels", []):
            channels.append({
                "id": ch.get("id"),
                "name": ch.get("name"),
                "topic": ch.get("topic", {}).get("value", ""),
                "purpose": ch.get("purpose", {}).get("value", ""),
                "num_members": ch.get("num_members", 0),
                "is_member": ch.get("is_member", False),
            })

        return {
            "channels": channels,
            "count": len(channels),
        }
    except urllib.error.URLError as e:
        from chitty_sdk import error
        error(f"Network error: {e}")
    except Exception as e:
        from chitty_sdk import error
        error(str(e))
