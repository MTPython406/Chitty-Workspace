"""Shared validation helpers for Google Calendar package.

Provides input validation, sanitization, and parsing for calendar tools.
"""
import re
from datetime import datetime, timezone

# Email validation (basic RFC 5322)
EMAIL_REGEX = re.compile(r"^[a-zA-Z0-9._%+\-]+@[a-zA-Z0-9.\-]+\.[a-zA-Z]{2,}$")

# ISO 8601 date patterns
ISO_DATETIME_REGEX = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}")
ISO_DATE_REGEX = re.compile(r"^\d{4}-\d{2}-\d{2}$")


def clamp_int(value, *, name: str, default: int, minimum: int, maximum: int) -> int:
    """Parse and clamp an integer value within bounds."""
    try:
        n = int(value) if value is not None else default
    except (TypeError, ValueError):
        n = default
    return max(minimum, min(n, maximum))


def parse_bool(value, *, name: str, default: bool = False) -> bool:
    """Parse a boolean value from various input formats."""
    if value is None:
        return default
    if isinstance(value, bool):
        return value
    if isinstance(value, str):
        return value.lower().strip() in ("true", "1", "yes", "on")
    return bool(value)


def sanitize_text(value: str, *, max_length: int = 1000) -> str:
    """Sanitize a text field: strip, truncate."""
    if not value:
        return ""
    return str(value).strip()[:max_length]


def normalize_attendees(attendees) -> list[str]:
    """Parse, validate, normalize, and deduplicate attendee email addresses."""
    if not attendees:
        return []

    # Handle comma-separated string
    if isinstance(attendees, str):
        attendees = [a.strip() for a in attendees.split(",")]

    # Validate, normalize, deduplicate
    seen = set()
    valid = []
    for addr in attendees:
        if not isinstance(addr, str):
            continue
        addr = addr.strip().lower()
        if not addr or not EMAIL_REGEX.match(addr):
            continue
        if addr not in seen:
            seen.add(addr)
            valid.append(addr)

    return valid


def validate_calendar_id(calendar_id) -> str:
    """Validate and sanitize a calendar ID.

    Accepts 'primary', email addresses, or Google resource calendar IDs.
    Returns 'primary' if the input is empty or invalid.
    """
    if not calendar_id or not isinstance(calendar_id, str):
        return "primary"
    calendar_id = calendar_id.strip()
    if not calendar_id:
        return "primary"
    if calendar_id.lower() == "primary":
        return "primary"
    # Allow email-style IDs and resource calendar IDs
    if EMAIL_REGEX.match(calendar_id):
        return calendar_id
    # Allow resource calendar IDs (contain @group.calendar.google.com etc.)
    if re.match(r"^[a-zA-Z0-9._\-]+@[a-zA-Z0-9.\-]+\.[a-zA-Z]{2,}$", calendar_id):
        return calendar_id
    # Fallback: if it looks like a bare ID (alphanumeric), allow it
    if re.match(r"^[a-zA-Z0-9._\-@]+$", calendar_id):
        return calendar_id
    return "primary"


def parse_event_window(start_time: str, end_time: str, timezone_str: str = "UTC"):
    """Parse and validate event start/end times.

    Returns (start_obj, end_obj, is_all_day) or raises ValueError.
    """
    if not start_time or not end_time:
        raise ValueError("Both start_time and end_time are required (ISO 8601 format)")

    start_time = str(start_time).strip()
    end_time = str(end_time).strip()

    # Detect all-day vs timed
    start_is_date = ISO_DATE_REGEX.match(start_time)
    end_is_date = ISO_DATE_REGEX.match(end_time)
    start_is_datetime = ISO_DATETIME_REGEX.match(start_time)
    end_is_datetime = ISO_DATETIME_REGEX.match(end_time)

    if start_is_date and end_is_date:
        # All-day event
        if start_time >= end_time:
            raise ValueError(f"End date ({end_time}) must be after start date ({start_time})")
        return (
            {"date": start_time},
            {"date": end_time},
            True,
        )
    elif start_is_datetime and end_is_datetime:
        # Timed event
        tz = sanitize_text(timezone_str, max_length=64) or "UTC"
        return (
            {"dateTime": start_time, "timeZone": tz},
            {"dateTime": end_time, "timeZone": tz},
            False,
        )
    else:
        raise ValueError(
            "Invalid time format. Use ISO 8601: "
            "'2026-03-22T10:00:00-07:00' for timed events or "
            "'2026-03-22' for all-day events"
        )
