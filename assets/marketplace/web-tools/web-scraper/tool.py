"""
Web Scraper tool for Chitty Workspace.
Extracts structured data from any web page using BeautifulSoup.
"""

import json
import sys

import requests
from bs4 import BeautifulSoup

HEADERS = {
    "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
}


def fetch_page(url):
    """Fetch a web page and return BeautifulSoup object."""
    try:
        resp = requests.get(url, headers=HEADERS, timeout=20)
        resp.raise_for_status()
        return BeautifulSoup(resp.text, "lxml"), None
    except requests.RequestException as e:
        return None, f"Failed to fetch {url}: {str(e)}"


def extract_text(soup, url):
    """Extract all visible text from the page."""
    # Remove script and style elements
    for tag in soup(["script", "style", "nav", "footer", "header"]):
        tag.decompose()

    text = soup.get_text(separator="\n", strip=True)
    # Collapse multiple blank lines
    lines = [line for line in text.split("\n") if line.strip()]
    truncated = "\n".join(lines[:200])  # Limit to 200 lines

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
        href = a["href"]
        text = a.get_text(strip=True)
        if not text and not href:
            continue
        # Make relative URLs absolute
        if href.startswith("/"):
            from urllib.parse import urljoin
            href = urljoin(url, href)
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
                    # Get href if it's a link
                    if found.name == "a" and found.get("href"):
                        item[field_name] = found.get_text(strip=True)
                        item[field_name + "_url"] = found["href"]
                        if item[field_name + "_url"].startswith("/"):
                            from urllib.parse import urljoin
                            item[field_name + "_url"] = urljoin(url, item[field_name + "_url"])
                    else:
                        item[field_name] = found.get_text(strip=True)
                else:
                    item[field_name] = ""
            results.append(item)
        else:
            text = el.get_text(strip=True)
            href = el.get("href", "")
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
                    href = link.get("href", "")
                    if href.startswith("/"):
                        from urllib.parse import urljoin
                        href = urljoin(url, href)
                    item["url"] = href
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
                        item["url"] = link.get("href", "")
                    items.append(item)
                break

    # Fallback: extract all text blocks with links
    if not items:
        for el in soup.find_all(["h2", "h3"]):
            link = el.find("a")
            if link:
                href = link.get("href", "")
                if href.startswith("/"):
                    from urllib.parse import urljoin
                    href = urljoin(url, href)
                items.append({
                    "type": "heading_link",
                    "title": link.get_text(strip=True),
                    "url": href,
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
    except json.JSONDecodeError as e:
        print(json.dumps({"success": False, "error": f"Invalid JSON: {e}"}))
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
