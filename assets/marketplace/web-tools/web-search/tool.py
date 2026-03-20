"""
Web Search tool for Chitty Workspace.
Uses DuckDuckGo (free, no API key) via the ddgs package.
"""

import json
import sys


def search_duckduckgo(query, max_results=5):
    """Search using DuckDuckGo (free, no API key needed)."""
    # Try the new 'ddgs' package first, fall back to 'duckduckgo_search'
    try:
        try:
            from ddgs import DDGS
        except ImportError:
            from duckduckgo_search import DDGS

        with DDGS() as ddgs:
            results = list(ddgs.text(query, max_results=max_results))

        formatted = []
        for r in results:
            formatted.append({
                "title": r.get("title", ""),
                "url": r.get("href", r.get("link", "")),
                "snippet": r.get("body", r.get("snippet", "")),
            })

        return {
            "success": True,
            "output": {
                "query": query,
                "results": formatted,
                "count": len(formatted),
                "provider": "duckduckgo",
            }
        }
    except ImportError:
        return {
            "success": False,
            "error": "Neither 'ddgs' nor 'duckduckgo-search' is installed. Run: pip install ddgs"
        }
    except Exception as e:
        return {"success": False, "error": f"Search failed: {str(e)}"}


def main():
    try:
        raw = sys.stdin.read()
        params = json.loads(raw) if raw.strip() else {}
    except json.JSONDecodeError as e:
        print(json.dumps({"success": False, "error": f"Invalid JSON: {e}"}))
        sys.exit(0)

    query = params.get("query", "")
    if not query:
        print(json.dumps({"success": False, "error": "query is required"}))
        sys.exit(0)

    max_results = min(int(params.get("max_results", 5)), 20)
    result = search_duckduckgo(query, max_results)
    print(json.dumps(result))


if __name__ == "__main__":
    main()
