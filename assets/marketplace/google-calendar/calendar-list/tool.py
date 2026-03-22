#!/usr/bin/env python3
"""Calendar List & Search — List, search, and filter Google Calendar events.

Supports upcoming events, text search, date range queries, and past event lookup.
Uses chitty-sdk for auth and HTTP helpers.
"""
import sys
import os
from datetime import datetime, timedelta, timezone

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from calendar_common import clamp_int, validate_calendar_id, sanitize_text

from chitty_sdk import tool_main, require_google_token, api_get


CALENDAR_API = "https://www.googleapis.com/calendar/v3"

MAX_RESULTS_LIMIT = 50
MAX_DAYS = 365
DESCRIPTION_TRUNCATE = 200


def format_event(e):
    """Format a single event for output."""
    start = e.get("start", {})
    end = e.get("end", {})

    # Collect attendees with response status
    attendees = []
    for a in e.get("attendees", []):
        entry = {"email": a.get("email", "")}
        name = a.get("displayName", "")
        if name:
            entry["name"] = name
        status = a.get("responseStatus", "")
        if status:
            entry["status"] = status
        attendees.append(entry)

    desc = e.get("description") or ""
    organizer = e.get("organizer", {})

    return {
        "title": e.get("summary", "(no title)"),
        "start": start.get("dateTime") or start.get("date", ""),
        "end": end.get("dateTime") or end.get("date", ""),
        "all_day": "date" in start and "dateTime" not in start,
        "location": e.get("location", ""),
        "description": desc[:DESCRIPTION_TRUNCATE],
        "description_truncated": len(desc) > DESCRIPTION_TRUNCATE,
        "attendees": attendees,
        "organizer": organizer.get("email", ""),
        "meeting_link": e.get("hangoutLink", ""),
        "status": e.get("status", ""),
        "recurring": bool(e.get("recurringEventId")),
        "id": e.get("id", ""),
    }


@tool_main
def main(args):
    token = require_google_token()

    max_results = clamp_int(args.get("max_results"), name="max_results", default=10, minimum=1, maximum=MAX_RESULTS_LIMIT)
    calendar_id = validate_calendar_id(args.get("calendar_id"))

    # Search query (free-text search across title, description, location, attendees)
    query = sanitize_text(args.get("query", ""), max_length=200)

    # Time range: support both "days_ahead" (future) and "days_back" (past) + explicit start/end
    start_time = args.get("start_time")
    end_time = args.get("end_time")

    now = datetime.now(timezone.utc)

    if start_time and end_time:
        # Explicit time range (for searching past events)
        time_min = str(start_time).strip()
        time_max = str(end_time).strip()
        period_desc = f"{time_min} to {time_max}"
    elif args.get("days_back"):
        # Look backward (past events)
        days_back = clamp_int(args.get("days_back"), name="days_back", default=30, minimum=1, maximum=MAX_DAYS)
        time_min = (now - timedelta(days=days_back)).isoformat()
        time_max = now.isoformat()
        period_desc = f"past {days_back} days"
    else:
        # Default: look forward (upcoming events)
        days_ahead = clamp_int(args.get("days_ahead"), name="days_ahead", default=7, minimum=1, maximum=MAX_DAYS)
        time_min = now.isoformat()
        time_max = (now + timedelta(days=days_ahead)).isoformat()
        period_desc = f"next {days_ahead} days"

    # Build API params
    params = {
        "timeMin": time_min,
        "timeMax": time_max,
        "maxResults": max_results,
        "singleEvents": "true",
        "orderBy": "startTime",
    }

    # Add search query if provided
    if query:
        params["q"] = query

    data = api_get(
        f"{CALENDAR_API}/calendars/{calendar_id}/events",
        token=token,
        params=params,
    )

    events = data.get("items", [])
    results = [format_event(e) for e in events]

    return {
        "events": results,
        "count": len(results),
        "period": period_desc,
        "query": query if query else None,
        "calendar": data.get("summary", "primary"),
    }
