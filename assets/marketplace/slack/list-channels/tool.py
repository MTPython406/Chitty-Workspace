#!/usr/bin/env python3
"""Slack tool: List channels in the connected workspace."""
import json
import sys
import os
import urllib.request
import urllib.error

# Add parent directory to path for shared helpers
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from auth import require_bot_token

def main():
    args = json.loads(sys.stdin.read())
    token = require_bot_token()

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
            print(json.dumps({"success": False, "error": data.get("error", "Unknown Slack API error")}))
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

        print(json.dumps({
            "success": True,
            "output": {
                "channels": channels,
                "count": len(channels),
            }
        }))
    except urllib.error.URLError as e:
        print(json.dumps({"success": False, "error": f"Network error: {e}"}))
    except Exception as e:
        print(json.dumps({"success": False, "error": str(e)}))

if __name__ == "__main__":
    main()
