#!/usr/bin/env python3
"""Calendar Update — Update an existing event on Google Calendar.

Supports partial updates: only fields that are provided are changed.
Feature-gated: requires allow_update_event.
Uses chitty-sdk for auth, config, and HTTP helpers.
"""
import sys
import os
import json
import urllib.request
import urllib.error

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from calendar_common import (
    sanitize_text, normalize_attendees, parse_event_window, validate_calendar_id,
)

from chitty_sdk import (
    tool_main, require_google_token, require_feature, check_feature, ChittyApiError,
)


CALENDAR_API = "https://www.googleapis.com/calendar/v3"

MAX_SUMMARY_LENGTH = 256
MAX_DESCRIPTION_LENGTH = 8000
MAX_LOCATION_LENGTH = 256
MAX_ATTENDEES = 50


def api_patch(url, token, json_data, params=None):
    """Send an HTTP PATCH request with Bearer authentication.

    The chitty-sdk does not expose api_patch, so we use urllib directly.
    """
    if params:
        import urllib.parse
        sep = "&" if "?" in url else "?"
        url = url + sep + urllib.parse.urlencode(params)

    body = json.dumps(json_data).encode("utf-8")
    req = urllib.request.Request(
        url,
        data=body,
        method="PATCH",
        headers={
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            raw = resp.read().decode("utf-8")
            if not raw:
                return {}
            return json.loads(raw)
    except urllib.error.HTTPError as exc:
        raise ChittyApiError(exc.code, exc.read().decode("utf-8", errors="replace")) from exc


@tool_main
def main(args):
    require_feature("allow_update_event")
    token = require_google_token()

    # Validate event_id
    event_id = str(args.get("event_id", "")).strip()
    if not event_id:
        return {"success": False, "error": "Missing required parameter 'event_id'"}

    calendar_id = validate_calendar_id(args.get("calendar_id"))

    # Build patch body with only provided fields
    patch = {}
    changes = []

    # Summary
    if "summary" in args and args["summary"] is not None:
        summary = sanitize_text(args["summary"], max_length=MAX_SUMMARY_LENGTH)
        if summary:
            patch["summary"] = summary
            changes.append("summary")

    # Description
    if "description" in args and args["description"] is not None:
        patch["description"] = sanitize_text(args["description"], max_length=MAX_DESCRIPTION_LENGTH)
        changes.append("description")

    # Location
    if "location" in args and args["location"] is not None:
        patch["location"] = sanitize_text(args["location"], max_length=MAX_LOCATION_LENGTH)
        changes.append("location")

    # Times (both must be provided together for a reschedule)
    has_start = "start_time" in args and args["start_time"]
    has_end = "end_time" in args and args["end_time"]
    if has_start or has_end:
        if not (has_start and has_end):
            return {"success": False, "error": "Both start_time and end_time must be provided to reschedule"}
        try:
            start_obj, end_obj, is_all_day = parse_event_window(
                args["start_time"],
                args["end_time"],
                args.get("timezone", "UTC"),
            )
        except ValueError as e:
            return {"success": False, "error": str(e)}
        patch["start"] = start_obj
        patch["end"] = end_obj
        changes.append("time")

    # Attendees (feature-gated)
    if "attendees" in args and args["attendees"] is not None:
        if not check_feature("allow_invite_attendees"):
            return {
                "success": False,
                "error": "Modifying attendees is disabled. Enable 'allow_invite_attendees' in package settings.",
            }
        attendee_emails = normalize_attendees(args["attendees"])
        if len(attendee_emails) > MAX_ATTENDEES:
            return {
                "success": False,
                "error": f"Too many attendees ({len(attendee_emails)}). Maximum is {MAX_ATTENDEES}.",
            }
        patch["attendees"] = [{"email": a} for a in attendee_emails]
        changes.append("attendees")

    if not patch:
        return {"success": False, "error": "No fields to update. Provide at least one field to change."}

    # Notification settings
    notify = args.get("notify_attendees")
    if isinstance(notify, str):
        notify = notify.lower().strip() in ("true", "1", "yes", "on")
    send_updates = "all" if notify else "none"

    # PATCH the event
    url = f"{CALENDAR_API}/calendars/{calendar_id}/events/{event_id}"
    result = api_patch(url, token, patch, params={"sendUpdates": send_updates})

    return {
        "success": True,
        "event_id": result.get("id", event_id),
        "title": result.get("summary", ""),
        "start": result.get("start", {}).get("dateTime") or result.get("start", {}).get("date", ""),
        "end": result.get("end", {}).get("dateTime") or result.get("end", {}).get("date", ""),
        "link": result.get("htmlLink", ""),
        "updated_fields": changes,
        "notifications_sent": bool(notify),
    }
