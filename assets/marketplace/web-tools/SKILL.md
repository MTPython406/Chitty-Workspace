---
name: web-tools
description: >
  Search the web and scrape web pages for information. Use when
  the user asks to find information online, research a topic,
  extract content from a URL, or gather data from websites.
allowed-tools: web_search web_scraper terminal
---

# Web Tools

## Web Search

- Use web_search for finding current information, documentation, or answers
- Summarize results concisely — don't dump raw search results
- Provide source URLs so the user can verify

## Web Scraping

- Use web_scraper to extract content from specific URLs
- Respect rate limits — don't make rapid repeated requests
- Extract the relevant content, not the entire page HTML
- For JavaScript-heavy sites, mention that scraping may not capture dynamic content

## Gotchas

- Some sites block automated requests — mention this if scraping fails
- Search results may be region-specific
- Always attribute sources when presenting scraped information
