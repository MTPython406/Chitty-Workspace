---
name: slack
description: >
  Send messages, read channel history, and reply to threads in Slack.
  Use when the user asks about Slack messaging, channel management,
  posting updates, summarizing conversations, or monitoring Slack activity.
allowed-tools: slack_list_channels slack_send_message slack_read_history slack_reply_thread
compatibility: Requires Slack App OAuth setup
license: MIT
metadata:
  author: Chitty
  version: "1.0"
---

# Slack Integration

## Approach

Always start by listing channels to confirm bot access and get channel IDs.
Show the user message content before sending. **Never post without explicit user confirmation.**

## Listing Channels

- Use `slack_list_channels` to see available channels
- The bot must be invited to a channel before it can read or post
- Channel IDs (C01234...) are more reliable than channel names

## Reading Messages

- Use `slack_read_history` with a channel name or ID
- Default to 20 messages to keep context manageable
- Summarize long histories rather than dumping all messages
- Note thread replies separately — check `reply_count` and `thread_ts`
- Use `oldest` and `latest` params for time-filtered queries

## Sending Messages

- Use `slack_send_message` to post to a channel
- Always show the user the exact message text before sending
- Support Slack mrkdwn: *bold*, _italic_, `code`, ```code blocks```
- Links: `<https://example.com|display text>`
- Mentions: `<@USER_ID>`, `<!channel>`, `<!here>`

## Replying to Threads

- Use `slack_reply_thread` with the parent message's `ts` value
- Get the `ts` from `slack_read_history` results first
- Thread replies keep conversations organized

## Common Errors

- `channel_not_found` — Bot not invited. Tell user: /invite @ChittyWorkspace
- `not_in_channel` — Same as above
- `invalid_auth` — Token expired or revoked. Re-run OAuth setup.
- Empty channel list — Bot has no channel memberships yet

## Socket Mode (Real-Time Events)

When Socket Mode is enabled, the bot receives:
- **@mentions** — When someone @mentions the bot in a channel
- **DMs** — Direct messages to the bot

Events are routed to the assigned agent for automatic responses.
