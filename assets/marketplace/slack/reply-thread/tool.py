#!/usr/bin/env python3
"""Slack tool: Reply to a message thread."""
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
    thread_ts = args.get("thread_ts", "")
    text = args.get("text", "")

    if not channel or not thread_ts or not text:
        print(json.dumps({"success": False, "error": "'channel', 'thread_ts', and 'text' are all required."}))
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

    payload = json.dumps({
        "channel": channel,
        "text": text,
        "thread_ts": thread_ts,
    }).encode()

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
                error = f"Channel '{channel}' not found."
            elif error == "thread_not_found":
                error = f"Thread '{thread_ts}' not found in channel '{channel}'."
            print(json.dumps({"success": False, "error": error}))
            return

        print(json.dumps({
            "success": True,
            "output": {
                "channel": data.get("channel"),
                "ts": data.get("ts"),
                "thread_ts": thread_ts,
                "message": f"Reply posted in thread",
            }
        }))
    except urllib.error.URLError as e:
        print(json.dumps({"success": False, "error": f"Network error: {e}"}))
    except Exception as e:
        print(json.dumps({"success": False, "error": str(e)}))

if __name__ == "__main__":
    main()
