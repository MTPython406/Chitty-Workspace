#!/usr/bin/env python3
"""Gmail Send — Send and reply to emails via the Gmail API.

Uses Python's email package for safe RFC 2822 message generation.
Validates recipients and protects against header injection.
Uses chitty-sdk for auth, config, and HTTP helpers.
"""
import base64
import re
from email.mime.text import MIMEText
from email.utils import formataddr, parseaddr

from chitty_sdk import tool_main, require_google_token, require_feature, api_post, api_get


GMAIL_API = "https://gmail.googleapis.com/gmail/v1/users/me"

# Basic email regex for validation
EMAIL_REGEX = re.compile(r"^[a-zA-Z0-9._%+\-]+@[a-zA-Z0-9.\-]+\.[a-zA-Z]{2,}$")


def validate_email(addr: str) -> bool:
    """Validate a single email address."""
    _, email = parseaddr(addr.strip())
    return bool(email and EMAIL_REGEX.match(email))


def parse_recipients(raw: str) -> list[str]:
    """Parse a comma-separated list of email addresses. Returns validated list."""
    if not raw:
        return []
    addresses = [a.strip() for a in raw.split(",") if a.strip()]
    valid = []
    for addr in addresses:
        if validate_email(addr):
            valid.append(addr)
    return valid


def sanitize_header(value: str) -> str:
    """Remove newlines and carriage returns to prevent header injection."""
    return value.replace("\r", "").replace("\n", "").strip()


def build_message(to, subject, body, cc=None, reply_to_id=None, token=None):
    """Build an RFC 2822 email using Python's email package (safe from injection)."""
    msg = MIMEText(body, "plain", "utf-8")
    msg["To"] = sanitize_header(to)
    msg["Subject"] = sanitize_header(subject)

    if cc:
        msg["Cc"] = sanitize_header(cc)

    # Handle reply threading — fetch original Message-ID and threadId
    thread_id = None
    if reply_to_id and token:
        try:
            original = api_get(
                f"{GMAIL_API}/messages/{reply_to_id}",
                token=token,
                params={"format": "metadata", "metadataHeaders": ["Message-ID", "Subject"]},
            )
            # Get Message-ID for threading headers
            headers = original.get("payload", {}).get("headers", [])
            for h in headers:
                name = h.get("name", "").lower()
                value = h.get("value", "")
                if name == "message-id" and value:
                    msg["In-Reply-To"] = sanitize_header(value)
                    msg["References"] = sanitize_header(value)
                elif name == "subject" and value:
                    # Prepend Re: if not already there
                    if not subject or subject == "(no subject)":
                        if not value.lower().startswith("re:"):
                            msg.replace_header("Subject", f"Re: {sanitize_header(value)}")
                        else:
                            msg.replace_header("Subject", sanitize_header(value))

            # Preserve Gmail thread
            thread_id = original.get("threadId")
        except Exception:
            pass  # Continue without threading

    raw = base64.urlsafe_b64encode(msg.as_bytes()).decode("ascii")
    return raw, thread_id


@tool_main
def main(args):
    require_feature("allow_send_email")
    token = require_google_token()

    to_raw = args.get("to", "")
    if not to_raw:
        return {"success": False, "error": "Missing 'to' email address"}

    # Validate all recipients
    to_list = parse_recipients(to_raw)
    if not to_list:
        return {"success": False, "error": f"No valid email addresses found in: {to_raw}"}

    cc_raw = args.get("cc", "")
    cc_list = parse_recipients(cc_raw) if cc_raw else []

    subject = sanitize_header(args.get("subject", "(no subject)"))
    body = args.get("body", "")
    reply_to_id = args.get("reply_to_id")

    to_str = ", ".join(to_list)
    cc_str = ", ".join(cc_list) if cc_list else None

    # Build the email safely
    encoded, thread_id = build_message(
        to_str, subject, body,
        cc=cc_str, reply_to_id=reply_to_id, token=token,
    )

    # Send via Gmail API
    send_body = {"raw": encoded}
    if thread_id:
        send_body["threadId"] = thread_id

    result = api_post(
        f"{GMAIL_API}/messages/send",
        token=token,
        json_data=send_body,
    )

    return {
        "success": True,
        "message_id": result.get("id", "unknown"),
        "to": to_list,
        "cc": cc_list if cc_list else None,
        "subject": subject,
        "thread_id": result.get("threadId", ""),
    }
