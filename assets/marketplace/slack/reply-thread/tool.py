#!/usr/bin/env python3
"""Slack tool: Reply to a message thread."""
import os
import re
import sys
import urllib.request
import urllib.error

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


# Slack limit for chat.postMessage text field
SLACK_MAX_TEXT_LENGTH = 4000

# Slack timestamp format: digits.digits (e.g. "1234567890.123456")
THREAD_TS_PATTERN = re.compile(r'^\d+\.\d+$')


@tool_main
def main(args):
    import json
    token = require_slack_token()

    channel = normalize_channel(args.get("channel", ""))
    thread_ts = args.get("thread_ts", "").strip()
    text = args.get("text", "").strip()

    if not channel:
        sdk_error("'channel' is required.")
        return
    if not thread_ts:
        sdk_error("'thread_ts' is required.")
        return
    if not text:
        sdk_error("'text' is required and cannot be empty.")
        return

    # Validate thread_ts format (must be Slack timestamp: digits.digits)
    if not THREAD_TS_PATTERN.match(thread_ts):
        sdk_error(f"Invalid thread_ts format: '{thread_ts}'. Expected Slack timestamp like '1234567890.123456'.")
        return

    # Reject oversized messages
    if len(text) > SLACK_MAX_TEXT_LENGTH:
        sdk_error(f"Message too long ({len(text)} chars). Slack limit is {SLACK_MAX_TEXT_LENGTH} characters.")
        return

    # Check feature flag
    require_feature("allow_send_message")

    # Check channel allowlist
    allowed, err = check_channel_allowed(channel)
    if not allowed:
        sdk_error(err)
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
            error_msg = data.get("error", "Unknown error")
            if error_msg == "channel_not_found":
                error_msg = f"Channel '{channel}' not found."
            elif error_msg == "thread_not_found":
                error_msg = f"Thread '{thread_ts}' not found in channel '{channel}'."
            sdk_error(error_msg)
            return

        return {
            "channel": data.get("channel"),
            "ts": data.get("ts"),
            "thread_ts": thread_ts,
            "message": "Reply posted in thread",
        }
    except urllib.error.URLError as e:
        sdk_error(f"Network error: {e}")
    except Exception as e:
        sdk_error(str(e))
