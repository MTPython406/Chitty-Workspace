"""
Social Post Formatter for Chitty Workspace.
Generates platform-specific social media content.
"""

import json
import sys
from datetime import datetime, timedelta


PLATFORM_LIMITS = {
    "x": 280,
    "linkedin": 3000,
    "facebook": 5000,
    "instagram": 2200,
}


def format_post(params):
    """Generate platform-specific social media posts."""
    platform = params.get("platform", "all")
    topic = params.get("topic", "")
    job_data = params.get("job_data", {})
    industry_news = params.get("industry_news", "")
    tone = params.get("tone", "professional")
    include_hashtags = params.get("hashtags", True)

    if not topic and not job_data and not industry_news:
        return {"success": False, "error": "Provide at least one of: topic, job_data, or industry_news"}

    # Build content components
    company = job_data.get("company", "")
    job_title = job_data.get("title", "")
    location = job_data.get("location", "")
    job_url = job_data.get("url", "")

    posts = {}
    platforms = [platform] if platform != "all" else ["x", "linkedin", "facebook", "instagram"]

    for plat in platforms:
        limit = PLATFORM_LIMITS.get(plat, 5000)

        if plat == "x":
            # Twitter/X: Short, punchy, with hashtags
            if job_data and industry_news:
                text = f"{industry_news[:100]}\n\n"
                text += f"🔥 {company} is hiring: {job_title}"
                if location:
                    text += f" in {location}"
                if job_url:
                    text += f"\n\nApply: {job_url}"
                if include_hashtags:
                    tags = generate_hashtags(job_title, industry_news, 3)
                    text += f"\n\n{tags}"
            elif job_data:
                text = f"🚀 Now Hiring: {job_title}"
                if company:
                    text += f" at {company}"
                if location:
                    text += f" | {location}"
                if job_url:
                    text += f"\n\nApply today: {job_url}"
                if include_hashtags:
                    tags = generate_hashtags(job_title, "", 3)
                    text += f"\n\n{tags}"
            else:
                text = topic[:200] if topic else industry_news[:200]
                if include_hashtags:
                    tags = generate_hashtags(topic or industry_news, "", 3)
                    text += f"\n\n{tags}"
            posts[plat] = text[:limit]

        elif plat == "linkedin":
            # LinkedIn: Professional, longer form
            parts = []
            if industry_news:
                parts.append(f"📰 Industry Insight: {industry_news}")
                parts.append("")
            if job_data:
                parts.append(f"🔔 We're expanding our team!")
                parts.append("")
                parts.append(f"**{job_title}**")
                if company:
                    parts.append(f"🏢 {company}")
                if location:
                    parts.append(f"📍 {location}")
                parts.append("")
                if topic:
                    parts.append(topic)
                    parts.append("")
                if job_url:
                    parts.append(f"Learn more and apply: {job_url}")
            elif topic:
                parts.append(topic)
            if include_hashtags:
                tags = generate_hashtags(job_title or topic, industry_news, 5)
                parts.append("")
                parts.append(tags)
            posts[plat] = "\n".join(parts)[:limit]

        elif plat == "facebook":
            # Facebook: Conversational
            parts = []
            if industry_news:
                parts.append(f"📰 {industry_news}")
                parts.append("")
            if job_data:
                parts.append(f"🙌 Great opportunity alert!")
                parts.append("")
                parts.append(f"We're looking for a {job_title}")
                if location:
                    parts.append(f"📍 Location: {location}")
                if company:
                    parts.append(f"🏢 Company: {company}")
                parts.append("")
                if topic:
                    parts.append(topic)
                    parts.append("")
                parts.append("Know someone who'd be perfect? Tag them below! 👇")
                if job_url:
                    parts.append("")
                    parts.append(f"Apply here: {job_url}")
            elif topic:
                parts.append(topic)
            posts[plat] = "\n".join(parts)[:limit]

        elif plat == "instagram":
            # Instagram: Visual-focused caption
            parts = []
            if job_data:
                parts.append(f"🚀 JOIN OUR TEAM")
                parts.append("")
                parts.append(f"{job_title}")
                if location:
                    parts.append(f"📍 {location}")
                if company:
                    parts.append(f"🏢 {company}")
                parts.append("")
                if topic or industry_news:
                    parts.append(topic or industry_news)
                    parts.append("")
                parts.append("💼 Link in bio to apply!")
            elif topic:
                parts.append(topic)
            if include_hashtags:
                tags = generate_hashtags(job_title or topic, industry_news, 10)
                parts.append("")
                parts.append(tags)
            posts[plat] = "\n".join(parts)[:limit]

    return {
        "success": True,
        "output": {
            "posts": posts,
            "platform_count": len(posts),
            "char_counts": {p: len(t) for p, t in posts.items()},
        }
    }


def generate_hashtags(primary_topic, secondary_topic, count):
    """Generate relevant hashtags from topics."""
    words = set()
    for text in [primary_topic, secondary_topic]:
        if text:
            for word in text.split():
                clean = word.strip(".,!?;:").lower()
                if len(clean) > 3 and clean.isalpha():
                    words.add(clean)

    # Common industry hashtags
    industry_tags = {
        "hvac": "#HVAC",
        "maintenance": "#FacilitiesMaintenance",
        "facilities": "#FacilitiesManagement",
        "hospital": "#HealthcareFacilities",
        "fire": "#FireProtection",
        "protection": "#FireSafety",
        "engineer": "#Engineering",
        "technician": "#Technician",
        "hiring": "#NowHiring",
        "jobs": "#Jobs",
    }

    tags = ["#Hiring", "#Careers"]
    for word in words:
        if word in industry_tags and len(tags) < count + 2:
            tags.append(industry_tags[word])

    return " ".join(tags[:count + 2])


def generate_calendar(params):
    """Generate a week of content ideas."""
    job_data = params.get("job_data", {})
    topic = params.get("topic", "")

    today = datetime.now()
    calendar = []
    themes = [
        ("Monday", "Industry news + job highlight"),
        ("Tuesday", "Employee spotlight or company culture"),
        ("Wednesday", "Technical tip related to the role"),
        ("Thursday", "Job posting feature with benefits"),
        ("Friday", "Weekend motivation + career growth"),
    ]

    for i, (day, theme) in enumerate(themes):
        date = today + timedelta(days=i)
        calendar.append({
            "day": day,
            "date": date.strftime("%Y-%m-%d"),
            "theme": theme,
            "platforms": ["x", "linkedin", "facebook"],
            "suggested_content": f"{theme}: {job_data.get('title', topic)}" if job_data or topic else theme,
        })

    return {
        "success": True,
        "output": {"calendar": calendar, "days": len(calendar)}
    }


# ── Main ──────────────────────────────────────────

ACTIONS = {
    "format": format_post,
    "calendar": generate_calendar,
}


def main():
    try:
        raw = sys.stdin.read()
        params = json.loads(raw) if raw.strip() else {}
    except json.JSONDecodeError as e:
        print(json.dumps({"success": False, "error": f"Invalid JSON: {e}"}))
        sys.exit(0)

    action = params.get("action", "format")
    if action not in ACTIONS:
        print(json.dumps({"success": False, "error": f"Unknown action '{action}'. Available: {', '.join(ACTIONS.keys())}"}))
        sys.exit(0)

    result = ACTIONS[action](params)
    print(json.dumps(result))


if __name__ == "__main__":
    main()
