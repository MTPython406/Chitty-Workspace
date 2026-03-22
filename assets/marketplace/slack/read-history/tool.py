#!/usr/bin/env python3
"""Slack tool: Read channel message history."""
import json
import sys
import os
import urllib.request
import urllib.error
from datetime import datetime

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from auth import require_bot_token
from config import check_channel_allowed

def main():
    args = json.loads(sys.stdin.read())
    token = require_bot_token()

    channel = args.get("channel", "").lstrip("#")
    limit = min(args.get("limit", 20), 100)

    if not channel:
        print(json.dumps({"success": False, "error": "'channel' is required."}))
        return

    # Check channel allowlist
    allowed, err = check_channel_allowed(channel)
    if not allowed:
        print(json.dumps({"success": False, "error": err}))
        return

    # Build URL with optional time filters
    params = f"channel={channel}&limit={limit}"
    if args.get("oldest"):
        params += f"&oldest={args['oldest']}"
    if args.get("latest"):
        params += f"&latest={args['latest']}"

    url = f"https://slack.com/api/conversations.history?{params}"
    req = urllib.request.Request(url, headers={
        "Authorization": f"Bearer {token}",
        "Content-Type": "application/json",
    })

    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
            data = json.loads(resp.read().decode())

        if not data.get("ok"):
            error = data.get("error", "Unknown error")
            if error == "channel_not_found":
                error = f"Channel '{channel}' not found. Use a channel ID or ensure the bot is invited."
            print(json.dumps({"success": False, "error": error}))
            return

        # Fetch user names for display
        users = {}
        try:
            user_req = urllib.request.Request(
                "https://slack.com/api/users.list?limit=200",
                headers={"Authorization": f"Bearer {token}"}
            )
            with urllib.request.urlopen(user_req, timeout=10) as uresp:
                udata = json.loads(uresp.read().decode())
                if udata.get("ok"):
                    for u in udata.get("members", []):
                        users[u["id"]] = u.get("real_name") or u.get("name", u["id"])
        except Exception:
            pass  # Continue without user names

        messages = []
        for msg in data.get("messages", []):
            ts = float(msg.get("ts", 0))
            messages.append({
                "user": users.get(msg.get("user"), msg.get("user", "unknown")),
                "text": msg.get("text", ""),
                "timestamp": datetime.fromtimestamp(ts).strftime("%Y-%m-%d %H:%M:%S") if ts else "",
                "ts": msg.get("ts"),
                "thread_ts": msg.get("thread_ts"),
                "reply_count": msg.get("reply_count", 0),
            })

        # Reverse to show oldest first
        messages.reverse()

        print(json.dumps({
            "success": True,
            "output": {
                "channel": channel,
                "messages": messages,
                "count": len(messages),
                "has_more": data.get("has_more", False),
            }
        }))
    except urllib.error.URLError as e:
        print(json.dumps({"success": False, "error": f"Network error: {e}"}))
    except Exception as e:
        print(json.dumps({"success": False, "error": str(e)}))

if __name__ == "__main__":
    main()
