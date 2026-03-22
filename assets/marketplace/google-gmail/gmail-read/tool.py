#!/usr/bin/env python3
"""Gmail Read — List, search, and read emails via the Gmail API.

Hardened input handling, safe base64 decoding, recursive attachment extraction.
Uses chitty-sdk for auth, config, and HTTP helpers.
"""
import base64
import html as html_mod
import re

from chitty_sdk import tool_main, require_google_token, api_get


GMAIL_API = "https://gmail.googleapis.com/gmail/v1/users/me"

# Valid actions (normalized to lowercase)
VALID_ACTIONS = {"list", "search", "read"}


def safe_b64_decode(data: str) -> str:
    """Safely decode Gmail's URL-safe base64 data."""
    if not data:
        return ""
    try:
        # Gmail uses URL-safe base64 without padding
        padded = data + "=" * (-len(data) % 4)
        return base64.urlsafe_b64decode(padded).decode("utf-8", errors="replace")
    except Exception:
        return ""


def decode_body(payload, depth: int = 0) -> str:
    """Extract plain text body from Gmail message payload (recursive, depth-limited)."""
    if depth > 10:
        return ""  # Prevent infinite recursion on malformed payloads

    mime = payload.get("mimeType", "")

    if mime == "text/plain":
        text = safe_b64_decode(payload.get("body", {}).get("data", ""))
        if text:
            return text

    if mime == "text/html" and not payload.get("parts"):
        raw_html = safe_b64_decode(payload.get("body", {}).get("data", ""))
        if raw_html:
            text = re.sub(r"<[^>]+>", " ", raw_html)
            text = html_mod.unescape(text)
            return re.sub(r"\s+", " ", text).strip()

    # Recurse into multipart parts
    for part in payload.get("parts", []):
        body = decode_body(part, depth + 1)
        if body:
            return body

    return ""


def collect_attachments(payload, depth: int = 0) -> list:
    """Recursively collect all attachments from nested parts."""
    if depth > 10:
        return []

    attachments = []
    filename = payload.get("filename", "")
    if filename:
        attachments.append({
            "filename": filename,
            "mimeType": payload.get("mimeType", ""),
            "size": payload.get("body", {}).get("size", 0),
            "attachmentId": payload.get("body", {}).get("attachmentId", ""),
        })

    for part in payload.get("parts", []):
        attachments.extend(collect_attachments(part, depth + 1))

    return attachments


def get_headers(detail, *names):
    """Extract specific headers from a Gmail message detail response."""
    result = {n.lower(): "" for n in names}
    headers = detail.get("payload", {}).get("headers", [])
    for h in headers:
        name = h.get("name", "")
        if name.lower() in result:
            result[name.lower()] = h.get("value", "")
    return result


def clamp_max_results(value, default: int = 10, maximum: int = 50) -> int:
    """Safely parse and clamp max_results."""
    try:
        n = int(value)
    except (TypeError, ValueError):
        n = default
    return max(1, min(n, maximum))


@tool_main
def main(args):
    token = require_google_token()

    # Normalize action
    action = str(args.get("action", "list")).strip().lower()
    if action not in VALID_ACTIONS:
        return {
            "success": False,
            "error": f"Unknown action: '{action}'. Use: {', '.join(sorted(VALID_ACTIONS))}",
        }

    if action in ("list", "search"):
        query = args.get("query", "in:inbox") if action == "search" else "in:inbox"
        max_results = clamp_max_results(args.get("max_results"))

        data = api_get(
            f"{GMAIL_API}/messages",
            token=token,
            params={"q": query, "maxResults": max_results},
        )

        messages = data.get("messages", [])
        if not messages:
            return {"emails": [], "count": 0, "query": query}

        # Fetch metadata for each message
        results = []
        for msg in messages[:max_results]:
            mid = msg.get("id", "")
            try:
                detail = api_get(
                    f"{GMAIL_API}/messages/{mid}",
                    token=token,
                    params={"format": "metadata", "metadataHeaders": ["Subject", "From", "Date"]},
                )
                hdrs = get_headers(detail, "Subject", "From", "Date")
                results.append({
                    "id": mid,
                    "thread_id": detail.get("threadId", ""),
                    "subject": hdrs["subject"],
                    "from": hdrs["from"],
                    "date": hdrs["date"],
                    "snippet": detail.get("snippet", ""),
                    "labels": detail.get("labelIds", []),
                })
            except Exception:
                results.append({"id": mid, "error": "Failed to fetch metadata"})

        return {"emails": results, "count": len(results), "query": query}

    elif action == "read":
        message_id = args.get("message_id")
        if not message_id:
            return {"success": False, "error": "Missing message_id for 'read' action"}

        # Sanitize message_id (should be alphanumeric)
        message_id = re.sub(r"[^a-zA-Z0-9]", "", str(message_id))

        detail = api_get(
            f"{GMAIL_API}/messages/{message_id}",
            token=token,
            params={"format": "full"},
        )

        hdrs = get_headers(detail, "Subject", "From", "To", "Date", "Cc")
        body_text = decode_body(detail.get("payload", {}))

        # Recursively collect all attachments
        attachments = collect_attachments(detail.get("payload", {}))

        return {
            "id": message_id,
            "thread_id": detail.get("threadId", ""),
            "subject": hdrs["subject"],
            "from": hdrs["from"],
            "to": hdrs["to"],
            "cc": hdrs.get("cc", ""),
            "date": hdrs["date"],
            "snippet": detail.get("snippet", ""),
            "body": body_text[:5000],  # Truncate long emails
            "labels": detail.get("labelIds", []),
            "attachments": attachments,
        }
