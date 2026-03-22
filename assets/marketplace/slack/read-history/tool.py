#!/usr/bin/env python3
"""Slack tool: Read channel message history."""
import os
import sys
import urllib.request
import urllib.error
import urllib.parse
from datetime import datetime

# Add parent directory to path for shared helpers (fallback)
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

try:
    from chitty_sdk import tool_main, require_slack_token, require_feature, error as sdk_error
    from config import check_channel_allowed, normalize_channel
except ImportError:
    from auth import require_bot_token as require_slack_token
    from config import check_channel_allowed, normalize_channel

    import json as _json
    import functools

    def tool_main(fn):
        @functools.wraps(fn)
        def wrapper():
            try:
                raw = sys.stdin.read()
                args = _json.loads(raw) if raw.strip() else {}
            except _json.JSONDecodeError:
                args = {}
            try:
                result = fn(args)
                if result is not None:
                    print(_json.dumps({"success": True, "output": result}))
            except SystemExit:
                raise
            except Exception as exc:
                print(_json.dumps({"success": False, "error": str(exc)}))
        if __name__ == "__main__":
            wrapper()
        return wrapper

    def require_feature(fid):
        from config import check_feature_allowed
        allowed, err = check_feature_allowed(fid)
        if not allowed:
            print(_json.dumps({"success": False, "error": err}))
            sys.exit(0)

    def sdk_error(msg):
        print(_json.dumps({"success": False, "error": msg}))
        sys.exit(0)


MAX_LIMIT = 100


@tool_main
def main(args):
    import json
    token = require_slack_token()

    # Check allow_read_history feature flag
    require_feature("allow_read_history")

    channel_raw = args.get("channel", "")
    channel = normalize_channel(channel_raw)
    limit = max(1, min(args.get("limit", 20), MAX_LIMIT))

    if not channel:
        sdk_error("'channel' is required.")
        return

    # Check channel allowlist
    allowed, err = check_channel_allowed(channel)
    if not allowed:
        sdk_error(err)
        return

    # Build URL with safely encoded query parameters
    params = {"channel": channel, "limit": str(limit)}
    if args.get("oldest"):
        params["oldest"] = str(args["oldest"])
    if args.get("latest"):
        params["latest"] = str(args["latest"])

    url = "https://slack.com/api/conversations.history?" + urllib.parse.urlencode(params)
    req = urllib.request.Request(url, headers={
        "Authorization": f"Bearer {token}",
        "Content-Type": "application/json",
    })

    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
            data = json.loads(resp.read().decode())

        if not data.get("ok"):
            error_msg = data.get("error", "Unknown error")
            if error_msg == "channel_not_found":
                error_msg = f"Channel '{channel}' not found. Use a channel ID or ensure the bot is invited."
            sdk_error(error_msg)
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

        return {
            "channel": channel,
            "messages": messages,
            "count": len(messages),
            "has_more": data.get("has_more", False),
        }
    except urllib.error.URLError as e:
        sdk_error(f"Network error: {e}")
    except Exception as e:
        sdk_error(str(e))
