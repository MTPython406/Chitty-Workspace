"""
Web Scraper tool for Chitty Workspace.
Extracts structured data from public web pages using BeautifulSoup.

Security: Validates URLs to block private/internal networks (SSRF protection).
Limits response size and validates content types before parsing.
"""

import json
import sys
import ipaddress
import socket
from urllib.parse import urljoin, urlparse

import requests
from bs4 import BeautifulSoup

HEADERS = {
    "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
}

# Maximum response size: 5 MB
MAX_RESPONSE_BYTES = 5 * 1024 * 1024

# Allowed content types for parsing
ALLOWED_CONTENT_TYPES = {"text/html", "application/xhtml+xml", "text/xml", "application/xml"}

# Blocked IP ranges (SSRF protection)
BLOCKED_NETWORKS = [
    ipaddress.ip_network("127.0.0.0/8"),       # Loopback
    ipaddress.ip_network("10.0.0.0/8"),         # Private Class A
    ipaddress.ip_network("172.16.0.0/12"),      # Private Class B
    ipaddress.ip_network("192.168.0.0/16"),     # Private Class C
    ipaddress.ip_network("169.254.0.0/16"),     # Link-local
    ipaddress.ip_network("0.0.0.0/8"),          # Current network
    ipaddress.ip_network("100.64.0.0/10"),      # Shared address space (CGN)
    ipaddress.ip_network("::1/128"),            # IPv6 loopback
    ipaddress.ip_network("fc00::/7"),           # IPv6 unique local
    ipaddress.ip_network("fe80::/10"),          # IPv6 link-local
]

# Blocked hostnames
BLOCKED_HOSTS = {
    "localhost", "metadata.google.internal", "169.254.169.254",
    "metadata", "metadata.google", "instance-data",
}


def validate_url(url: str) -> str | None:
    """Validate URL for safety. Returns error message or None if valid."""
    try:
        parsed = urlparse(url)
    except Exception:
        return "Invalid URL format"

    # Only allow http/https schemes
    if parsed.scheme not in ("http", "https"):
        return f"Unsupported scheme '{parsed.scheme}'. Only http and https are allowed."

    hostname = parsed.hostname
    if not hostname:
        return "URL has no hostname"

    # Check blocked hostnames
    hostname_lower = hostname.lower()
    if hostname_lower in BLOCKED_HOSTS:
        return f"Access to '{hostname}' is blocked for security."

    # Resolve DNS and check IP ranges
    try:
        resolved_ips = socket.getaddrinfo(hostname, parsed.port or 443, proto=socket.IPPROTO_TCP)
        for family, _, _, _, sockaddr in resolved_ips:
            ip = ipaddress.ip_address(sockaddr[0])
            for network in BLOCKED_NETWORKS:
                if ip in network:
                    return f"Access to internal/private network ({ip}) is blocked for security."
    except socket.gaierror:
        return f"Could not resolve hostname '{hostname}'"

    return None


def normalize_url(href: str, base_url: str) -> str:
    """Normalize a relative or absolute URL to an absolute URL."""
    if not href or href.startswith(("javascript:", "mailto:", "tel:", "#")):
        return href
    return urljoin(base_url, href)


def fetch_page(url: str):
    """Fetch a web page with size limits and content-type validation."""
    # Validate URL for SSRF
    err = validate_url(url)
    if err:
        return None, err

    try:
        # Stream response to enforce size limits
        resp = requests.get(url, headers=HEADERS, timeout=20, stream=True)
        resp.raise_for_status()

        # Validate content type
        content_type = resp.headers.get("Content-Type", "").split(";")[0].strip().lower()
        if content_type and content_type not in ALLOWED_CONTENT_TYPES:
            resp.close()
            return None, f"Unsupported content type: {content_type}. Expected HTML."

        # Read with size limit
        chunks = []
        total = 0
        for chunk in resp.iter_content(chunk_size=65536):
            total += len(chunk)
            if total > MAX_RESPONSE_BYTES:
                resp.close()
                return None, f"Response too large (>{MAX_RESPONSE_BYTES // 1024 // 1024} MB). Aborted."
            chunks.append(chunk)
        resp.close()

        html = b"".join(chunks).decode(resp.encoding or "utf-8", errors="replace")
        return BeautifulSoup(html, "lxml"), None

    except requests.Timeout:
        return None, f"Request timed out after 20 seconds"
    except requests.ConnectionError:
        return None, f"Could not connect to {urlparse(url).hostname}"
    except requests.HTTPError as e:
        return None, f"HTTP error: {e.response.status_code}"
    except requests.RequestException:
        return None, "Request failed"


def extract_text(soup, url):
    """Extract all visible text from the page."""
    for tag in soup(["script", "style", "nav", "footer", "header"]):
        tag.decompose()

    text = soup.get_text(separator="\n", strip=True)
    lines = [line for line in text.split("\n") if line.strip()]
    truncated = "\n".join(lines[:200])

    return {
        "success": True,
        "output": {
            "url": url,
            "title": soup.title.string if soup.title else "",
            "text": truncated,
            "line_count": len(lines),
        }
    }


def extract_links(soup, url):
    """Extract all links from the page."""
    links = []
    for a in soup.find_all("a", href=True):
        href = normalize_url(a["href"], url)
        text = a.get_text(strip=True)
        if not text and not href:
            continue
        links.append({"text": text, "url": href})

    return {
        "success": True,
        "output": {"url": url, "links": links[:100], "count": len(links)}
    }


def extract_tables(soup, url):
    """Extract HTML tables as JSON arrays."""
    tables = []
    for table in soup.find_all("table"):
        rows = []
        headers = []
        for th in table.find_all("th"):
            headers.append(th.get_text(strip=True))

        for tr in table.find_all("tr"):
            cells = [td.get_text(strip=True) for td in tr.find_all(["td", "th"])]
            if cells and any(c for c in cells):
                if headers and len(cells) == len(headers):
                    rows.append(dict(zip(headers, cells)))
                else:
                    rows.append(cells)

        if rows:
            tables.append({"headers": headers, "rows": rows[:50]})

    return {
        "success": True,
        "output": {"url": url, "tables": tables, "table_count": len(tables)}
    }


def extract_elements(soup, url, selector, extract_fields=None):
    """Extract elements matching a CSS selector."""
    if not selector:
        return {"success": False, "error": "selector is required for 'elements' action"}

    elements = soup.select(selector)
    results = []

    for el in elements[:50]:
        if extract_fields and isinstance(extract_fields, dict):
            item = {}
            for field_name, field_selector in extract_fields.items():
                found = el.select_one(field_selector)
                if found:
                    if found.name == "a" and found.get("href"):
                        item[field_name] = found.get_text(strip=True)
                        item[field_name + "_url"] = normalize_url(found["href"], url)
                    else:
                        item[field_name] = found.get_text(strip=True)
                else:
                    item[field_name] = ""
            results.append(item)
        else:
            text = el.get_text(strip=True)
            href = normalize_url(el.get("href", ""), url) if el.get("href") else ""
            results.append({"text": text, "href": href, "tag": el.name})

    return {
        "success": True,
        "output": {"url": url, "selector": selector, "elements": results, "count": len(results)}
    }


def extract_structured(soup, url):
    """Auto-detect structured data: job postings, products, articles."""
    items = []

    # Try common job listing patterns
    job_selectors = [
        "tr[class*='job']", "div[class*='job']", "li[class*='job']",
        "tr[class*='career']", "div[class*='career']", "li[class*='career']",
        "tr[class*='position']", "div[class*='position']", "li[class*='opening']",
        ".job-listing", ".job-row", ".career-item", ".position-item",
    ]
    for sel in job_selectors:
        found = soup.select(sel)
        if len(found) >= 2:
            for el in found[:30]:
                link = el.find("a")
                item = {
                    "type": "job_posting",
                    "text": el.get_text(" | ", strip=True)[:200],
                }
                if link:
                    item["title"] = link.get_text(strip=True)
                    item["url"] = normalize_url(link.get("href", ""), url)
                items.append(item)
            break

    # If no jobs found, try article/news patterns
    if not items:
        article_selectors = ["article", ".post", ".article", ".news-item", ".entry"]
        for sel in article_selectors:
            found = soup.select(sel)
            if len(found) >= 2:
                for el in found[:20]:
                    link = el.find("a")
                    heading = el.find(["h1", "h2", "h3", "h4"])
                    item = {
                        "type": "article",
                        "title": heading.get_text(strip=True) if heading else "",
                        "text": el.get_text(" ", strip=True)[:200],
                    }
                    if link:
                        item["url"] = normalize_url(link.get("href", ""), url)
                    items.append(item)
                break

    # Fallback: extract heading links
    if not items:
        for el in soup.find_all(["h2", "h3"]):
            link = el.find("a")
            if link:
                items.append({
                    "type": "heading_link",
                    "title": link.get_text(strip=True),
                    "url": normalize_url(link.get("href", ""), url),
                })

    return {
        "success": True,
        "output": {
            "url": url,
            "items": items[:50],
            "count": len(items),
            "detection": "auto",
        }
    }


# ── Main ──────────────────────────────────────────────

ACTIONS = {
    "text": lambda soup, url, p: extract_text(soup, url),
    "links": lambda soup, url, p: extract_links(soup, url),
    "tables": lambda soup, url, p: extract_tables(soup, url),
    "elements": lambda soup, url, p: extract_elements(soup, url, p.get("selector"), p.get("extract_fields")),
    "structured": lambda soup, url, p: extract_structured(soup, url),
}


def main():
    try:
        raw = sys.stdin.read()
        params = json.loads(raw) if raw.strip() else {}
    except json.JSONDecodeError:
        print(json.dumps({"success": False, "error": "Invalid JSON input"}))
        sys.exit(0)

    url = params.get("url", "")
    if not url:
        print(json.dumps({"success": False, "error": "url is required"}))
        sys.exit(0)

    if not url.startswith("http"):
        url = "https://" + url

    action = params.get("action", "text")
    if action not in ACTIONS:
        print(json.dumps({"success": False, "error": f"Unknown action '{action}'. Available: {', '.join(ACTIONS.keys())}"}))
        sys.exit(0)

    soup, err = fetch_page(url)
    if err:
        print(json.dumps({"success": False, "error": err}))
        sys.exit(0)

    result = ACTIONS[action](soup, url, params)
    print(json.dumps(result))


if __name__ == "__main__":
    main()
