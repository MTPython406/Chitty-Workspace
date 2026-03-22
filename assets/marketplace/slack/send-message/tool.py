#!/usr/bin/env python3
"""Slack tool: Send a message to a channel."""
import json
import sys
import os
import urllib.request
import urllib.error

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from auth import require_bot_token
from config import check_channel_allowed, check_feature_allowed

def main():
    args = json.loads(sys.stdin.read())
    token = require_bot_token()

    channel = args.get("channel", "").lstrip("#")
    text = args.get("text", "")

    if not channel or not text:
        print(json.dumps({"success": False, "error": "Both 'channel' and 'text' are required."}))
        return

    # Check feature flag
    allowed, err = check_feature_allowed("allow_send_message")
    if not allowed:
        print(json.dumps({"success": False, "error": err}))
        return

    # Check channel allowlist
    allowed, err = check_channel_allowed(channel)
    if not allowed:
        print(json.dumps({"success": False, "error": err}))
        return

    payload = json.dumps({"channel": channel, "text": text}).encode()
    req = urllib.request.Request(
        "https://slack.com/api/chat.postMessage",
        data=payload,
        headers={
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json; charset=utf-8",
        },
    )

    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
            data = json.loads(resp.read().decode())

        if not data.get("ok"):
            error = data.get("error", "Unknown error")
            if error == "channel_not_found":
                error = f"Channel '{channel}' not found. Make sure the bot is invited to the channel."
            elif error == "not_in_channel":
                error = f"Bot is not a member of '{channel}'. Invite the bot first with /invite @ChittyWorkspace"
            print(json.dumps({"success": False, "error": error}))
            return

        print(json.dumps({
            "success": True,
            "output": {
                "channel": data.get("channel"),
                "ts": data.get("ts"),
                "message": f"Message sent to #{channel}",
            }
        }))
    except urllib.error.URLError as e:
        print(json.dumps({"success": False, "error": f"Network error: {e}"}))
    except Exception as e:
        print(json.dumps({"success": False, "error": str(e)}))

if __name__ == "__main__":
    main()
