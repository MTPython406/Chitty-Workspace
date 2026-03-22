#!/usr/bin/env python3
"""Calendar List — List upcoming events from Google Calendar.

Validates inputs, bounds results, truncates descriptions safely.
Uses chitty-sdk for auth and HTTP helpers.
"""
import sys
import os
from datetime import datetime, timedelta, timezone

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from calendar_common import clamp_int, validate_calendar_id

from chitty_sdk import tool_main, require_google_token, api_get


CALENDAR_API = "https://www.googleapis.com/calendar/v3"

MAX_RESULTS_LIMIT = 50
MAX_DAYS_AHEAD = 365
DESCRIPTION_TRUNCATE = 200


@tool_main
def main(args):
    token = require_google_token()

    max_results = clamp_int(args.get("max_results"), name="max_results", default=10, minimum=1, maximum=MAX_RESULTS_LIMIT)
    days_ahead = clamp_int(args.get("days_ahead"), name="days_ahead", default=7, minimum=1, maximum=MAX_DAYS_AHEAD)
    calendar_id = validate_calendar_id(args.get("calendar_id"))

    now = datetime.now(timezone.utc)
    time_min = now.isoformat()
    time_max = (now + timedelta(days=days_ahead)).isoformat()

    data = api_get(
        f"{CALENDAR_API}/calendars/{calendar_id}/events",
        token=token,
        params={
            "timeMin": time_min,
            "timeMax": time_max,
            "maxResults": max_results,
            "singleEvents": "true",
            "orderBy": "startTime",
        },
    )

    events = data.get("items", [])
    results = []
    for e in events:
        start = e.get("start", {})
        end = e.get("end", {})
        attendees = [a.get("email", "") for a in e.get("attendees", [])]

        desc = e.get("description") or ""
        description_truncated = len(desc) > DESCRIPTION_TRUNCATE

        results.append({
            "title": e.get("summary", "(no title)"),
            "start": start.get("dateTime") or start.get("date", ""),
            "end": end.get("dateTime") or end.get("date", ""),
            "all_day": "date" in start and "dateTime" not in start,
            "location": e.get("location", ""),
            "description": desc[:DESCRIPTION_TRUNCATE],
            "description_truncated": description_truncated,
            "attendees": attendees,
            "meeting_link": e.get("hangoutLink", ""),
            "status": e.get("status", ""),
            "id": e.get("id", ""),
        })

    return {
        "events": results,
        "count": len(results),
        "period": f"next {days_ahead} days",
        "calendar": data.get("summary", "primary"),
    }
