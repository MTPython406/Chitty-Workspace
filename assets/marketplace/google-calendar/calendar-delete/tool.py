#!/usr/bin/env python3
"""Calendar Delete — Delete/cancel an event on Google Calendar.

Feature-gated: requires allow_delete_event (default disabled).
Uses chitty-sdk for auth, config, and HTTP helpers.
"""
import sys
import os
import json
import urllib.request
import urllib.error
import urllib.parse

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from calendar_common import validate_calendar_id

from chitty_sdk import tool_main, require_google_token, require_feature, ChittyApiError


CALENDAR_API = "https://www.googleapis.com/calendar/v3"


def api_delete_with_params(url, token, params=None):
    """Send an HTTP DELETE request with query params.

    The chitty-sdk api_delete does not accept params, so we build the URL manually.
    """
    if params:
        sep = "&" if "?" in url else "?"
        url = url + sep + urllib.parse.urlencode(params)

    req = urllib.request.Request(
        url,
        method="DELETE",
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
    require_feature("allow_delete_event")
    token = require_google_token()

    # Validate event_id
    event_id = str(args.get("event_id", "")).strip()
    if not event_id:
        return {"success": False, "error": "Missing required parameter 'event_id'"}

    calendar_id = validate_calendar_id(args.get("calendar_id"))

    # Notification settings
    notify = args.get("notify_attendees")
    if isinstance(notify, str):
        notify = notify.lower().strip() in ("true", "1", "yes", "on")
    elif not isinstance(notify, bool):
        notify = False
    send_updates = "all" if notify else "none"

    # DELETE the event
    url = f"{CALENDAR_API}/calendars/{calendar_id}/events/{event_id}"
    api_delete_with_params(url, token, params={"sendUpdates": send_updates})

    return {
        "success": True,
        "event_id": event_id,
        "calendar_id": calendar_id,
        "deleted": True,
        "notifications_sent": notify,
    }
