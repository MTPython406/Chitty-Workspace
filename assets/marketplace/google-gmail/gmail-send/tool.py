#!/usr/bin/env python3
"""Gmail Send — Send and reply to emails via the Gmail API.

Uses chitty-sdk for auth, config, and HTTP helpers.
"""
import base64

from chitty_sdk import tool_main, require_google_token, require_feature, api_post, api_get


GMAIL_API = "https://gmail.googleapis.com/gmail/v1/users/me"


def build_rfc2822(to, subject, body, cc=None, reply_to_id=None, token=None):
    """Build an RFC 2822 email message."""
    lines = []
    lines.append(f"To: {to}")
    if cc:
        lines.append(f"Cc: {cc}")
    lines.append(f"Subject: {subject}")
    lines.append("Content-Type: text/plain; charset=utf-8")
    lines.append("MIME-Version: 1.0")

    # Handle reply threading
    if reply_to_id and token:
        try:
            original = api_get(
                f"{GMAIL_API}/messages/{reply_to_id}",
                token=token,
                params={"format": "metadata", "metadataHeaders": ["Message-ID"]},
            )
            headers = original.get("payload", {}).get("headers", [])
            for h in headers:
                if h.get("name", "").lower() == "message-id":
                    msg_id = h.get("value", "")
                    lines.append(f"In-Reply-To: {msg_id}")
                    lines.append(f"References: {msg_id}")
                    break
        except Exception:
            pass  # Continue without threading headers

    lines.append("")  # Blank line before body
    lines.append(body)

    raw = "\r\n".join(lines)
    return base64.urlsafe_b64encode(raw.encode("utf-8")).decode("ascii")


@tool_main
def main(args):
    require_feature("allow_send_email")
    token = require_google_token()

    to = args.get("to")
    if not to:
        return {"success": False, "error": "Missing 'to' email address"}

    subject = args.get("subject", "(no subject)")
    body = args.get("body", "")
    cc = args.get("cc")
    reply_to_id = args.get("reply_to_id")

    # Build and encode the email
    encoded = build_rfc2822(to, subject, body, cc=cc, reply_to_id=reply_to_id, token=token)

    # Send via Gmail API
    result = api_post(
        f"{GMAIL_API}/messages/send",
        token=token,
        json_data={"raw": encoded},
    )

    return {
        "success": True,
        "message_id": result.get("id", "unknown"),
        "to": to,
        "subject": subject,
        "thread_id": result.get("threadId", ""),
    }
