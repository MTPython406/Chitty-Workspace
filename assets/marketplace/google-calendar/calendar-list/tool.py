#!/usr/bin/env python3
"""Calendar List — List upcoming events from Google Calendar.

Uses chitty-sdk for auth and HTTP helpers.
"""
from datetime import datetime, timedelta, timezone

from chitty_sdk import tool_main, require_google_token, api_get


CALENDAR_API = "https://www.googleapis.com/calendar/v3"


@tool_main
def main(args):
    token = require_google_token()
    max_results = min(int(args.get("max_results", 10)), 50)
    days_ahead = int(args.get("days_ahead", 7))

    now = datetime.now(timezone.utc)
    time_min = now.isoformat()
    time_max = (now + timedelta(days=days_ahead)).isoformat()

    data = api_get(
        f"{CALENDAR_API}/calendars/primary/events",
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
        attendees = [
            a.get("email", "") for a in e.get("attendees", [])
        ]
        results.append({
            "title": e.get("summary", "(no title)"),
            "start": start.get("dateTime") or start.get("date", ""),
            "end": end.get("dateTime") or end.get("date", ""),
            "all_day": "date" in start and "dateTime" not in start,
            "location": e.get("location", ""),
            "description": (e.get("description") or "")[:200],
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
