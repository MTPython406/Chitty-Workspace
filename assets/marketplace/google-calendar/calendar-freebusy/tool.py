#!/usr/bin/env python3
"""Calendar FreeBusy — Find free/busy times across Google Calendars.

Queries the FreeBusy API and calculates available time slots.
Uses chitty-sdk for auth and HTTP helpers.
"""
import sys
import os
from datetime import datetime, timedelta, timezone

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from calendar_common import clamp_int, validate_calendar_id

from chitty_sdk import tool_main, require_google_token, api_post


CALENDAR_API = "https://www.googleapis.com/calendar/v3"

MAX_CALENDARS = 20
MAX_RANGE_DAYS = 60
MIN_SLOT_MINUTES = 5
MAX_SLOT_MINUTES = 480


def _parse_iso(s):
    """Parse an ISO 8601 datetime string to a datetime object."""
    s = str(s).strip()
    # Handle Z suffix
    if s.endswith("Z"):
        s = s[:-1] + "+00:00"
    return datetime.fromisoformat(s)


def _compute_free_slots(busy_blocks, range_start, range_end, min_duration):
    """Given sorted busy blocks, compute free slots of at least min_duration."""
    free = []
    cursor = range_start

    for block in busy_blocks:
        block_start = _parse_iso(block["start"])
        block_end = _parse_iso(block["end"])

        # Ensure block_start is timezone-aware
        if block_start.tzinfo is None:
            block_start = block_start.replace(tzinfo=timezone.utc)
        if block_end.tzinfo is None:
            block_end = block_end.replace(tzinfo=timezone.utc)

        if cursor < block_start:
            gap = block_start - cursor
            if gap >= min_duration:
                free.append({
                    "start": cursor.isoformat(),
                    "end": block_start.isoformat(),
                    "duration_minutes": int(gap.total_seconds() / 60),
                })
        cursor = max(cursor, block_end)

    # Trailing free slot
    if cursor < range_end:
        gap = range_end - cursor
        if gap >= min_duration:
            free.append({
                "start": cursor.isoformat(),
                "end": range_end.isoformat(),
                "duration_minutes": int(gap.total_seconds() / 60),
            })

    return free


@tool_main
def main(args):
    token = require_google_token()

    # Validate times
    start_time = args.get("start_time")
    end_time = args.get("end_time")
    if not start_time or not end_time:
        return {"success": False, "error": "Both start_time and end_time are required (ISO 8601 format)"}

    try:
        range_start = _parse_iso(start_time)
        range_end = _parse_iso(end_time)
    except (ValueError, TypeError) as e:
        return {"success": False, "error": f"Invalid time format: {e}. Use ISO 8601 (e.g. '2026-03-22T09:00:00-07:00')"}

    # Ensure timezone-aware
    if range_start.tzinfo is None:
        range_start = range_start.replace(tzinfo=timezone.utc)
    if range_end.tzinfo is None:
        range_end = range_end.replace(tzinfo=timezone.utc)

    if range_end <= range_start:
        return {"success": False, "error": "end_time must be after start_time"}

    if (range_end - range_start).days > MAX_RANGE_DAYS:
        return {"success": False, "error": f"Time range cannot exceed {MAX_RANGE_DAYS} days"}

    # Validate calendar IDs
    raw_ids = args.get("calendar_ids")
    if not raw_ids or not isinstance(raw_ids, list):
        calendar_ids = ["primary"]
    else:
        calendar_ids = [validate_calendar_id(cid) for cid in raw_ids[:MAX_CALENDARS]]

    # Validate min_duration
    min_minutes = clamp_int(
        args.get("min_duration_minutes"),
        name="min_duration_minutes",
        default=30,
        minimum=MIN_SLOT_MINUTES,
        maximum=MAX_SLOT_MINUTES,
    )
    min_duration = timedelta(minutes=min_minutes)

    # Build FreeBusy request
    freebusy_body = {
        "timeMin": range_start.isoformat(),
        "timeMax": range_end.isoformat(),
        "items": [{"id": cid} for cid in calendar_ids],
    }

    data = api_post(
        f"{CALENDAR_API}/freeBusy",
        token=token,
        json_data=freebusy_body,
    )

    # Process results per calendar
    calendars_result = {}
    all_busy = []

    for cal_id in calendar_ids:
        cal_data = data.get("calendars", {}).get(cal_id, {})
        errors = cal_data.get("errors", [])
        busy = cal_data.get("busy", [])
        all_busy.extend(busy)

        calendars_result[cal_id] = {
            "busy_count": len(busy),
            "busy": busy,
            "errors": [e.get("reason", "unknown") for e in errors] if errors else [],
        }

    # Sort all busy blocks and merge overlaps for free slot calculation
    all_busy.sort(key=lambda b: b.get("start", ""))
    merged = []
    for block in all_busy:
        if merged and block.get("start", "") <= merged[-1].get("end", ""):
            # Overlapping — extend
            if block.get("end", "") > merged[-1]["end"]:
                merged[-1]["end"] = block["end"]
        else:
            merged.append(dict(block))

    # Calculate free slots
    free_slots = _compute_free_slots(merged, range_start, range_end, min_duration)

    return {
        "success": True,
        "range": {
            "start": range_start.isoformat(),
            "end": range_end.isoformat(),
        },
        "calendars": calendars_result,
        "free_slots": free_slots,
        "free_slot_count": len(free_slots),
        "min_duration_minutes": min_minutes,
    }
