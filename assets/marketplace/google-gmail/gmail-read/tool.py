#!/usr/bin/env python3
"""Gmail Read — List, search, and read emails via the Gmail API.

Uses chitty-sdk for auth, config, and HTTP helpers.
"""
import base64
import html as html_mod
import re

from chitty_sdk import tool_main, require_google_token, api_get


GMAIL_API = "https://gmail.googleapis.com/gmail/v1/users/me"


def decode_body(payload):
    """Extract plain text body from Gmail message payload (recursive)."""
    mime = payload.get("mimeType", "")

    if mime == "text/plain":
        data = payload.get("body", {}).get("data", "")
        if data:
            # Gmail uses URL-safe base64
            padded = data + "=" * (4 - len(data) % 4)
            try:
                return base64.urlsafe_b64decode(padded).decode("utf-8", errors="replace")
            except Exception:
                return ""

    if mime == "text/html" and not payload.get("parts"):
        data = payload.get("body", {}).get("data", "")
        if data:
            padded = data + "=" * (4 - len(data) % 4)
            try:
                raw_html = base64.urlsafe_b64decode(padded).decode("utf-8", errors="replace")
                # Strip HTML tags for plain text
                text = re.sub(r"<[^>]+>", " ", raw_html)
                text = html_mod.unescape(text)
                return re.sub(r"\s+", " ", text).strip()
            except Exception:
                return ""

    # Recurse into multipart
    for part in payload.get("parts", []):
        body = decode_body(part)
        if body:
            return body

    return ""


def get_headers(detail, *names):
    """Extract specific headers from a Gmail message detail response."""
    result = {n.lower(): "" for n in names}
    headers = detail.get("payload", {}).get("headers", [])
    for h in headers:
        name = h.get("name", "")
        if name.lower() in result:
            result[name.lower()] = h.get("value", "")
    return result


@tool_main
def main(args):
    token = require_google_token()
    action = args.get("action", "list")

    if action in ("list", "search"):
        query = args.get("query", "in:inbox") if action == "search" else "in:inbox"
        max_results = min(int(args.get("max_results", 10)), 50)

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

        detail = api_get(
            f"{GMAIL_API}/messages/{message_id}",
            token=token,
            params={"format": "full"},
        )

        hdrs = get_headers(detail, "Subject", "From", "To", "Date", "Cc")
        body_text = decode_body(detail.get("payload", {}))

        # Get attachment info
        attachments = []
        for part in detail.get("payload", {}).get("parts", []):
            filename = part.get("filename", "")
            if filename:
                attachments.append({
                    "filename": filename,
                    "mimeType": part.get("mimeType", ""),
                    "size": part.get("body", {}).get("size", 0),
                })

        return {
            "id": message_id,
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

    else:
        return {"success": False, "error": f"Unknown action: {action}. Use: list, search, read"}
