---
name: google-gmail
description: >
  Read, search, and send emails from Gmail.
  Use when the user asks about email, inbox management,
  sending messages, searching mail, or reading email threads.
allowed-tools: gmail_read gmail_send
compatibility: Requires Google OAuth setup
license: MIT
metadata:
  author: Chitty
  version: "1.0"
---

# Google Gmail Integration

## Approach

Always show the user email content before sending. **Never send an email without explicit user confirmation.**
When reading emails, summarize long threads rather than dumping full content.

## Reading Emails

- Use `gmail_read` with action `list` to show recent inbox emails
- Use `gmail_read` with action `search` for Gmail query syntax searches
- Use `gmail_read` with action `read` to get the full email body by message_id

### Gmail Search Query Examples

- `from:boss@company.com` — Emails from a specific sender
- `subject:meeting` — Emails with "meeting" in the subject
- `is:unread` — Unread emails only
- `has:attachment` — Emails with attachments
- `after:2024/01/01 before:2024/12/31` — Date range
- `from:alice OR from:bob` — Multiple senders
- `label:important` — Emails with a specific label
- Combine queries: `is:unread from:boss has:attachment`

## Sending Emails

- Use `gmail_send` with to, subject, and body
- Always show the exact email content to the user before sending
- For replies, pass `reply_to_id` with the original message's ID
- Reply threading is automatic — the tool handles In-Reply-To and References headers

## Common Errors

- `401 Unauthorized` — OAuth token expired. Re-run the Google Gmail setup wizard.
- `403 Forbidden` — Gmail API not enabled or scopes not authorized.
- `invalid_grant` — Token revoked. User needs to re-authorize.
- Empty results — Try broadening the search query.
