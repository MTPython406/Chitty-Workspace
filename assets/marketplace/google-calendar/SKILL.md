---
name: google-calendar
description: >
  Full Google Calendar management — list events, create, update, delete,
  and find free/busy times across multiple calendars. Use when the user
  asks about their schedule, availability, scheduling meetings, rescheduling,
  cancelling events, or checking free time slots.
allowed-tools: calendar_list calendar_create calendar_freebusy calendar_update calendar_delete
compatibility: Requires Google OAuth setup
license: MIT
metadata:
  author: Chitty
  version: "1.2"
---

# Google Calendar Integration

## Approach

Show event details clearly with times, locations, and attendees.
**Never create, update, or delete an event without explicit user confirmation.**
When scheduling, use `calendar_freebusy` first to find available time slots.
Always confirm the event summary, time, and attendees before making changes.

## Listing Events

- Use `calendar_list` to show upcoming events
- Default is next 7 days, adjustable with `days_ahead` (max 365)
- Optional `calendar_id` to query a specific calendar (default: "primary")
- Events returned in chronological order with title, time, location, attendees

## Finding Free Time

- Use `calendar_freebusy` to find available time slots
- Provide `start_time` and `end_time` to define the search window
- Optional `min_duration_minutes` to filter slots (default: 30)
- Optional `calendar_ids` to check multiple calendars at once
- Returns busy blocks and calculated free slots
- **Use this BEFORE creating events** to avoid conflicts

## Creating Events

- Use `calendar_create` with summary, start_time, and end_time
- Times must be in ISO 8601 format: `2026-03-22T14:00:00-07:00`
- For all-day events, use date format: `2026-03-22`
- Optional: description, location, attendees, calendar_id, notify_attendees
- Attendees require `allow_invite_attendees` feature flag
- Notifications only sent when `notify_attendees=true`

## Updating Events

- Use `calendar_update` with event_id and only the fields to change
- Requires `allow_update_event` feature flag
- Only provided fields are updated (partial update via PATCH)
- Attendee changes require `allow_invite_attendees` flag

## Deleting Events

- Use `calendar_delete` with event_id
- Requires `allow_delete_event` feature flag (disabled by default)
- Optional `notify_attendees` to inform attendees of cancellation
- **This action is irreversible — always confirm with user**

## Multi-Calendar Support

All tools accept an optional `calendar_id` parameter:
- `"primary"` — User's main calendar (default)
- `"user@example.com"` — Shared calendar by email
- `"resource@group.calendar.google.com"` — Room/resource calendar

## Time Format Examples

- Timed: `"2026-03-22T14:00:00-07:00"` (with timezone offset)
- UTC: `"2026-03-22T21:00:00Z"`
- All-day: `"2026-03-22"` (end date is exclusive: use next day)

## Common Errors

- `401 Unauthorized` — OAuth token expired. Re-run Google setup.
- `403 Forbidden` — Calendar API not enabled or scopes missing.
- `invalid_grant` — Token revoked. User needs to re-authorize.
- Feature flag disabled — Enable the required flag in package settings.
