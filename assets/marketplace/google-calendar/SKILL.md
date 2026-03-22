---
name: google-calendar
description: >
  View upcoming events and create new events on Google Calendar.
  Use when the user asks about their schedule, upcoming meetings,
  creating events, booking time, or checking availability.
allowed-tools: calendar_list calendar_create
compatibility: Requires Google OAuth setup
license: MIT
metadata:
  author: Chitty
  version: "1.0"
---

# Google Calendar Integration

## Approach

Show event details clearly with times, locations, and attendees.
**Never create an event without explicit user confirmation.**
Always confirm the event summary, time, and attendees before creating.

## Listing Events

- Use `calendar_list` to show upcoming events
- Default is next 7 days, adjustable with `days_ahead` parameter (max 90)
- Events are returned in chronological order
- All-day events show date only, timed events show full datetime
- Check for attendees and meeting links to provide complete info

## Creating Events

- Use `calendar_create` with summary, start_time, and end_time
- Times must be in ISO 8601 format: `2024-03-20T14:00:00-07:00`
- For all-day events, use date format: `2024-03-20`
- Optional: description, location, attendees (list of email addresses)
- Always confirm all details with the user before creating
- Attendees will receive email invitations from Google Calendar

### Time Format Examples

- Timed event: `start_time: "2024-03-20T14:00:00-07:00"`, `end_time: "2024-03-20T15:00:00-07:00"`
- All-day event: `start_time: "2024-03-20"`, `end_time: "2024-03-21"` (end date is exclusive)
- UTC time: `start_time: "2024-03-20T21:00:00Z"`

## Common Errors

- `401 Unauthorized` — OAuth token expired. Re-run the Google Calendar setup wizard.
- `403 Forbidden` — Calendar API not enabled or scopes not authorized.
- `invalid_grant` — Token revoked. User needs to re-authorize.
- `409 Conflict` — Event ID collision (rare).
