#!/usr/bin/env python3
"""Calendar Create — Create events on Google Calendar.

Validates all inputs, separates attendee invitation from notification,
and feature-gates attendee invitations independently.
Uses chitty-sdk for auth, config, and HTTP helpers.
"""
import sys
import os

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from calendar_common import (
    clamp_int, parse_bool, sanitize_text,
    normalize_attendees, parse_event_window, validate_calendar_id,
)

from chitty_sdk import tool_main, require_google_token, require_feature, check_feature, api_post


CALENDAR_API = "https://www.googleapis.com/calendar/v3"

# Limits
MAX_SUMMARY_LENGTH = 256
MAX_DESCRIPTION_LENGTH = 8000
MAX_LOCATION_LENGTH = 256
MAX_ATTENDEES = 50


@tool_main
def main(args):
    require_feature("allow_create_event")
    token = require_google_token()
    calendar_id = validate_calendar_id(args.get("calendar_id"))

    # Validate summary
    summary = sanitize_text(args.get("summary", ""), max_length=MAX_SUMMARY_LENGTH)
    if not summary:
        return {"success": False, "error": "Missing or empty 'summary' (event title)"}

    # Validate times
    try:
        start_obj, end_obj, is_all_day = parse_event_window(
            args.get("start_time"),
            args.get("end_time"),
            args.get("timezone", "UTC"),
        )
    except ValueError as e:
        return {"success": False, "error": str(e)}

    # Build event body
    description = sanitize_text(args.get("description", ""), max_length=MAX_DESCRIPTION_LENGTH)
    location = sanitize_text(args.get("location", ""), max_length=MAX_LOCATION_LENGTH)

    event = {
        "summary": summary,
        "start": start_obj,
        "end": end_obj,
    }
    if description:
        event["description"] = description
    if location:
        event["location"] = location

    # Handle attendees (feature-gated)
    attendee_emails = normalize_attendees(args.get("attendees"))
    notify = parse_bool(args.get("notify_attendees"), name="notify_attendees", default=False)

    if attendee_emails:
        # Check if attendee invitations are allowed
        if not check_feature("allow_invite_attendees"):
            return {
                "success": False,
                "error": "Adding attendees is disabled. Enable 'allow_invite_attendees' in package settings.",
            }

        if len(attendee_emails) > MAX_ATTENDEES:
            return {
                "success": False,
                "error": f"Too many attendees ({len(attendee_emails)}). Maximum is {MAX_ATTENDEES}.",
            }

        event["attendees"] = [{"email": a} for a in attendee_emails]

    # Determine notification behavior:
    # Only send Google notifications if explicitly requested AND attendees exist
    send_updates = "none"
    if attendee_emails and notify:
        send_updates = "all"

    # Create the event
    result = api_post(
        f"{CALENDAR_API}/calendars/{calendar_id}/events",
        token=token,
        json_data=event,
        params={"sendUpdates": send_updates},
    )

    return {
        "success": True,
        "event_id": result.get("id", ""),
        "title": result.get("summary", summary),
        "start": result.get("start", {}).get("dateTime") or result.get("start", {}).get("date", ""),
        "end": result.get("end", {}).get("dateTime") or result.get("end", {}).get("date", ""),
        "link": result.get("htmlLink", ""),
        "all_day": is_all_day,
        "attendees_count": len(attendee_emails),
        "notifications_sent": notify and bool(attendee_emails),
    }
