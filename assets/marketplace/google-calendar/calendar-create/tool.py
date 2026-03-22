#!/usr/bin/env python3
"""Calendar Create — Create events on Google Calendar.

Uses chitty-sdk for auth, config, and HTTP helpers.
"""
from chitty_sdk import tool_main, require_google_token, require_feature, api_post


CALENDAR_API = "https://www.googleapis.com/calendar/v3"


@tool_main
def main(args):
    require_feature("allow_create_event")
    token = require_google_token()

    summary = args.get("summary")
    if not summary:
        return {"success": False, "error": "Missing 'summary' (event title)"}

    start_time = args.get("start_time")
    end_time = args.get("end_time")
    if not start_time or not end_time:
        return {"success": False, "error": "Missing 'start_time' and/or 'end_time' (ISO 8601 format)"}

    # Build event body
    event = {
        "summary": summary,
        "description": args.get("description", ""),
        "location": args.get("location", ""),
    }

    # Detect all-day vs timed event
    if "T" in start_time:
        event["start"] = {"dateTime": start_time, "timeZone": args.get("timezone", "UTC")}
        event["end"] = {"dateTime": end_time, "timeZone": args.get("timezone", "UTC")}
    else:
        event["start"] = {"date": start_time}
        event["end"] = {"date": end_time}

    # Add attendees if provided
    attendees = args.get("attendees", [])
    if attendees:
        if isinstance(attendees, str):
            attendees = [a.strip() for a in attendees.split(",")]
        event["attendees"] = [{"email": a} for a in attendees if a]

    # Create the event
    result = api_post(
        f"{CALENDAR_API}/calendars/primary/events",
        token=token,
        json_data=event,
        params={"sendUpdates": "all"} if attendees else None,
    )

    return {
        "success": True,
        "event_id": result.get("id", ""),
        "title": result.get("summary", summary),
        "start": result.get("start", {}).get("dateTime") or result.get("start", {}).get("date", ""),
        "end": result.get("end", {}).get("dateTime") or result.get("end", {}).get("date", ""),
        "link": result.get("htmlLink", ""),
        "attendees_count": len(attendees),
    }
