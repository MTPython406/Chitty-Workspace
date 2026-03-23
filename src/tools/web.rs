//! Web tools — native web search and web scraper
//!
//! Critical system tools implemented in pure Rust.
//! - `web_search`: Google Custom Search (with DuckDuckGo fallback)
//! - `web_scraper`: Fetch and extract structured data from web pages

use async_trait::async_trait;
use scraper::{Html, Selector};
use std::net::ToSocketAddrs;
use tracing;
use url::Url;

use super::{NativeTool, ToolCategory, ToolContext, ToolDefinition, ToolResult};

// ─── SSRF Protection ─────────────────────────────────────────────────────────

const BLOCKED_HOSTS: &[&str] = &[
    "localhost",
    "metadata.google.internal",
    "169.254.169.254",
    "metadata",
    "metadata.google",
    "instance-data",
];

/// Check if an IP address is in a private/blocked range
fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.octets()[0] == 100 && v4.octets()[1] >= 64 && v4.octets()[1] <= 127 // CGN
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // fc00::/7 — unique local
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // fe80::/10 — link-local
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Validate a URL for SSRF protection. Returns error message or None if safe.
fn validate_url(url_str: &str) -> Result<Url, String> {
    let parsed = Url::parse(url_str).map_err(|_| "Invalid URL format".to_string())?;

    match parsed.scheme() {
        "http" | "https" => {}
        scheme => return Err(format!("Unsupported scheme '{}'. Only http/https allowed.", scheme)),
    }

    let host = parsed
        .host_str()
        .ok_or("URL has no hostname")?;

    if BLOCKED_HOSTS.contains(&host.to_lowercase().as_str()) {
        return Err(format!("Access to '{}' is blocked for security.", host));
    }

    // Resolve DNS and check IP ranges
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addr_str = format!("{}:{}", host, port);
    if let Ok(addrs) = addr_str.to_socket_addrs() {
        for addr in addrs {
            if is_private_ip(&addr.ip()) {
                return Err(format!(
                    "Access to internal/private network ({}) is blocked for security.",
                    addr.ip()
                ));
            }
        }
    }

    Ok(parsed)
}

// ─── HTTP Client ─────────────────────────────────────────────────────────────

fn build_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))
}

const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024; // 5 MB

// ─── Web Search ──────────────────────────────────────────────────────────────

// ─── Keyring keys for BYOK Google Custom Search ─────────────────────────────
// Users provide their own API key + Search Engine ID from their own GCP project.
// Stored in OS keyring — never hardcoded, never billed to DataVisions.
const KEYRING_GOOGLE_CSE_API_KEY: &str = "google_cse_api_key";
const KEYRING_GOOGLE_CSE_CX: &str = "google_cse_cx";

pub struct WebSearchTool;

#[async_trait]
impl NativeTool for WebSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_search".to_string(),
            display_name: "Web Search".to_string(),
            description: "Search the web. Uses Google Custom Search if configured (BYOK), otherwise falls back to DuckDuckGo. Returns titles, URLs, and snippets.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query"
                    },
                    "max_results": {
                        "type": "number",
                        "description": "Number of results to return (default: 5, max: 10 for Google, 20 for DuckDuckGo)"
                    }
                },
                "required": ["query"]
            }),
            instructions: Some(
                "Search the web for current information. Returns results with titles, URLs, and snippets.\n\
                 - Uses **Google Custom Search** when the user has configured their own API key (BYOK).\n\
                 - Falls back to **DuckDuckGo** (free, no setup) if Google is not configured.\n\
                 - The response includes a `provider` field showing which engine was used.\n\
                 - If the user wants Google-quality results, guide them: Settings → Integrations → Google Search → enter API key + Search Engine ID.\n\
                 - Use for current events, weather, prices, news, or anything needing up-to-date info.\n\
                 - After searching, use `web_scraper` to read the full content of promising results."
                    .to_string(),
            ),
            category: ToolCategory::Native,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let query = match args.get("query").and_then(|v| v.as_str()) {
            Some(q) if !q.is_empty() => q,
            _ => return ToolResult::err("Missing required parameter: query"),
        };

        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .min(20) as usize;

        // Try Google Custom Search first (BYOK — user's own API key)
        match search_google(query, max_results).await {
            Ok(results) => {
                return ToolResult::ok(serde_json::json!({
                    "query": query,
                    "results": results,
                    "count": results.len(),
                    "provider": "google"
                }));
            }
            Err(google_err) => {
                tracing::info!("Google Search unavailable ({}), falling back to DuckDuckGo", google_err);
            }
        }

        // Fallback: DuckDuckGo (free, no auth required)
        match search_duckduckgo(query, max_results).await {
            Ok(results) => ToolResult::ok(serde_json::json!({
                "query": query,
                "results": results,
                "count": results.len(),
                "provider": "duckduckgo"
            })),
            Err(e) => ToolResult::err(format!("Search failed: {}", e)),
        }
    }
}

/// Search using Google Custom Search API (BYOK — user's own API key + CX)
///
/// Requires two keyring entries:
/// - `google_cse_api_key`: API key from user's Google Cloud Console
/// - `google_cse_cx`: Search Engine ID from Programmable Search Engine
async fn search_google(
    query: &str,
    max_results: usize,
) -> Result<Vec<serde_json::Value>, String> {
    // Get user's own API key from keyring
    let api_key = crate::config::get_api_key(KEYRING_GOOGLE_CSE_API_KEY)
        .ok()
        .flatten()
        .ok_or_else(|| "Google Search API key not configured".to_string())?;

    let cx = crate::config::get_api_key(KEYRING_GOOGLE_CSE_CX)
        .ok()
        .flatten()
        .ok_or_else(|| "Google Search Engine ID (cx) not configured".to_string())?;

    // Google CSE returns max 10 results per request
    let num = max_results.min(10);

    let client = build_client()?;
    let resp = client
        .get("https://www.googleapis.com/customsearch/v1")
        .query(&[
            ("key", api_key.as_str()),
            ("cx", cx.as_str()),
            ("q", query),
            ("num", &num.to_string()),
        ])
        .send()
        .await
        .map_err(|e| format!("Google Search request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        // Parse error for better messaging
        if status.as_u16() == 403 {
            return Err("Google Search API key invalid or quota exceeded. Check your API key in Settings → Integrations.".to_string());
        }
        return Err(format!("Google Search HTTP {}: {}", status, body));
    }

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse Google Search response: {}", e))?;

    let mut results = Vec::new();
    if let Some(items) = data.get("items").and_then(|v| v.as_array()) {
        for item in items {
            results.push(serde_json::json!({
                "title": item.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                "url": item.get("link").and_then(|v| v.as_str()).unwrap_or(""),
                "snippet": item.get("snippet").and_then(|v| v.as_str()).unwrap_or("")
            }));
        }
    }

    Ok(results)
}

/// Search DuckDuckGo HTML endpoint and parse results
async fn search_duckduckgo(
    query: &str,
    max_results: usize,
) -> Result<Vec<serde_json::Value>, String> {
    let client = build_client()?;

    // Use DuckDuckGo HTML search (same as the ddgs Python library)
    let resp = client
        .post("https://html.duckduckgo.com/html/")
        .form(&[("q", query), ("kl", "")])
        .send()
        .await
        .map_err(|e| format!("Search request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("DuckDuckGo returned HTTP {}", resp.status()));
    }

    let html = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read response: {}", e))?;

    let document = Html::parse_document(&html);

    // DuckDuckGo HTML results are in <div class="result"> or similar
    let result_sel =
        Selector::parse(".result").unwrap_or_else(|_| Selector::parse("div").unwrap());
    let link_sel = Selector::parse("a.result__a").unwrap_or_else(|_| Selector::parse("a").unwrap());
    let snippet_sel = Selector::parse("a.result__snippet, .result__snippet")
        .unwrap_or_else(|_| Selector::parse(".snippet").unwrap());

    let mut results = Vec::new();

    for element in document.select(&result_sel) {
        if results.len() >= max_results {
            break;
        }

        // Extract link
        let (title, url) = if let Some(link) = element.select(&link_sel).next() {
            let title: String = link.text().collect::<Vec<_>>().join("").trim().to_string();
            let href = link.value().attr("href").unwrap_or("").to_string();

            // DDG wraps URLs in a redirect — extract the actual URL
            let actual_url = extract_ddg_url(&href);
            if title.is_empty() || actual_url.is_empty() {
                continue;
            }
            (title, actual_url)
        } else {
            continue;
        };

        // Extract snippet
        let snippet: String = if let Some(snip) = element.select(&snippet_sel).next() {
            snip.text().collect::<Vec<_>>().join("").trim().to_string()
        } else {
            String::new()
        };

        results.push(serde_json::json!({
            "title": title,
            "url": url,
            "snippet": snippet
        }));
    }

    Ok(results)
}

/// Extract the actual URL from DDG's redirect wrapper
fn extract_ddg_url(href: &str) -> String {
    // DDG uses //duckduckgo.com/l/?uddg=ENCODED_URL&rut=...
    if let Some(pos) = href.find("uddg=") {
        let start = pos + 5;
        let end = href[start..]
            .find('&')
            .map(|i| start + i)
            .unwrap_or(href.len());
        let encoded = &href[start..end];
        urlencoding::decode(encoded)
            .map(|s| s.into_owned())
            .unwrap_or_else(|_| encoded.to_string())
    } else if href.starts_with("http") {
        href.to_string()
    } else if href.starts_with("//") {
        format!("https:{}", href)
    } else {
        href.to_string()
    }
}

// ─── Web Scraper ─────────────────────────────────────────────────────────────

pub struct WebScraperTool;

#[async_trait]
impl NativeTool for WebScraperTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_scraper".to_string(),
            display_name: "Web Scraper".to_string(),
            description: "Scrape and extract structured data from any public web page. Get text, links, tables, or specific elements by CSS selector.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to scrape"
                    },
                    "action": {
                        "type": "string",
                        "description": "What to extract: 'text' (visible page text), 'links' (all links), 'tables' (HTML tables as JSON), 'elements' (by CSS selector)",
                        "enum": ["text", "links", "tables", "elements"]
                    },
                    "selector": {
                        "type": "string",
                        "description": "CSS selector for 'elements' action (e.g., '.job-listing', 'table tr', 'h2.title')"
                    }
                },
                "required": ["url"]
            }),
            instructions: Some(
                "Scrape data from public web pages. Use after `web_search` to read full page content.\n\n\
                 **Actions:**\n\
                 - `text` (default) — Get all visible text from the page (strips scripts, styles, nav, footer)\n\
                 - `links` — Get all links with text and URLs\n\
                 - `tables` — Extract HTML tables as JSON arrays\n\
                 - `elements` — Extract specific elements using a CSS selector (requires `selector` parameter)\n\n\
                 **Tips:**\n\
                 - Use `text` first to understand the page structure, then `elements` for targeted extraction.\n\
                 - Text output is limited to 200 lines to keep responses manageable."
                    .to_string(),
            ),
            category: ToolCategory::Native,
            vendor: None,
        }
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let url_str = match args.get("url").and_then(|v| v.as_str()) {
            Some(u) if !u.is_empty() => {
                if u.starts_with("http") {
                    u.to_string()
                } else {
                    format!("https://{}", u)
                }
            }
            _ => return ToolResult::err("Missing required parameter: url"),
        };

        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("text");

        if !["text", "links", "tables", "elements"].contains(&action) {
            return ToolResult::err(format!(
                "Unknown action '{}'. Available: text, links, tables, elements",
                action
            ));
        }

        // Validate URL (SSRF protection)
        let parsed_url = match validate_url(&url_str) {
            Ok(u) => u,
            Err(e) => return ToolResult::err(e),
        };

        // Fetch the page
        let html = match fetch_page(&parsed_url).await {
            Ok(h) => h,
            Err(e) => return ToolResult::err(e),
        };

        let document = Html::parse_document(&html);
        let url_string = parsed_url.to_string();

        match action {
            "text" => extract_text(&document, &url_string),
            "links" => extract_links(&document, &url_string),
            "tables" => extract_tables(&document, &url_string),
            "elements" => {
                let selector = args.get("selector").and_then(|v| v.as_str()).unwrap_or("");
                if selector.is_empty() {
                    return ToolResult::err("'selector' parameter is required for 'elements' action");
                }
                extract_elements(&document, &url_string, selector)
            }
            _ => ToolResult::err("Unknown action"),
        }
    }
}

/// Fetch a page with size limits and content-type validation
async fn fetch_page(url: &Url) -> Result<String, String> {
    let client = build_client()?;

    let resp = client
        .get(url.as_str())
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                "Request timed out after 20 seconds".to_string()
            } else if e.is_connect() {
                format!("Could not connect to {}", url.host_str().unwrap_or("host"))
            } else {
                format!("Request failed: {}", e)
            }
        })?;

    if !resp.status().is_success() {
        return Err(format!("HTTP error: {}", resp.status()));
    }

    // Check content type
    if let Some(ct) = resp.headers().get("content-type") {
        let ct_str = ct.to_str().unwrap_or("");
        let mime = ct_str.split(';').next().unwrap_or("").trim().to_lowercase();
        if !mime.is_empty()
            && !matches!(
                mime.as_str(),
                "text/html" | "application/xhtml+xml" | "text/xml" | "application/xml"
            )
        {
            return Err(format!("Unsupported content type: {}. Expected HTML.", mime));
        }
    }

    // Read with size limit
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("Failed to read response: {}", e))?;

    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(format!(
            "Response too large (>{} MB). Aborted.",
            MAX_RESPONSE_BYTES / 1024 / 1024
        ));
    }

    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Extract visible text from the page
fn extract_text(document: &Html, url: &str) -> ToolResult {
    // Get title
    let title = Selector::parse("title")
        .ok()
        .and_then(|sel| document.select(&sel).next())
        .map(|el| el.text().collect::<String>())
        .unwrap_or_default();

    // Get the body HTML, strip unwanted tags via regex, then extract text
    let body_html = Selector::parse("body")
        .ok()
        .and_then(|sel| document.select(&sel).next())
        .map(|el| el.html())
        .unwrap_or_else(|| document.html());

    // Remove script/style/nav/footer/header/noscript blocks
    // Note: Rust regex doesn't support backreferences (\1), so we strip each tag separately
    let mut cleaned_html = body_html;
    for tag in &["script", "style", "nav", "footer", "header", "noscript"] {
        let pattern = format!(r"(?si)<{}\b[^>]*>.*?</{}>", tag, tag);
        if let Ok(re) = regex::Regex::new(&pattern) {
            cleaned_html = re.replace_all(&cleaned_html, "").to_string();
        }
    }

    // Re-parse the cleaned HTML and extract text
    let cleaned = Html::parse_fragment(&cleaned_html);
    let body_sel = Selector::parse("body").unwrap();
    let text: String = cleaned
        .select(&body_sel)
        .next()
        .map(|el| el.text().collect::<Vec<_>>().join("\n"))
        .unwrap_or_else(|| cleaned.root_element().text().collect::<Vec<_>>().join("\n"));

    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    let line_count = lines.len();
    let truncated: String = lines.into_iter().take(200).collect::<Vec<_>>().join("\n");

    // Detect error/thin pages so the LLM knows to try a different URL
    let lower_title = title.to_lowercase();
    let lower_text = truncated.to_lowercase();
    let is_error_page = lower_title.contains("404")
        || lower_title.contains("not found")
        || lower_title.contains("error")
        || lower_title.contains("access denied")
        || lower_title.contains("captcha")
        || lower_text.contains("page not found")
        || lower_text.contains("404 not found")
        || lower_text.contains("access denied")
        || lower_text.contains("enable javascript")
        || lower_text.contains("please verify you are a human");

    let is_thin_page = line_count < 5 && !truncated.is_empty();

    let mut result = serde_json::json!({
        "url": url,
        "title": title,
        "text": truncated,
        "line_count": line_count
    });

    if is_error_page {
        result["warning"] = serde_json::json!(
            "This page appears to be an error page (404/access denied/captcha). Try a different URL from the search results."
        );
    } else if is_thin_page {
        result["warning"] = serde_json::json!(
            "Very little content extracted. The page may require JavaScript or may have blocked the request. Try a different URL."
        );
    }

    ToolResult::ok(result)
}

/// Extract all links from the page
fn extract_links(document: &Html, url: &str) -> ToolResult {
    let sel = match Selector::parse("a[href]") {
        Ok(s) => s,
        Err(_) => return ToolResult::err("Failed to parse link selector"),
    };

    let base_url = Url::parse(url).ok();
    let mut links = Vec::new();

    for el in document.select(&sel) {
        if links.len() >= 100 {
            break;
        }

        let href = el.value().attr("href").unwrap_or("");
        let text: String = el.text().collect::<Vec<_>>().join("").trim().to_string();

        if href.is_empty() && text.is_empty() {
            continue;
        }

        // Resolve relative URLs
        let resolved = if let Some(ref base) = base_url {
            base.join(href)
                .map(|u| u.to_string())
                .unwrap_or_else(|_| href.to_string())
        } else {
            href.to_string()
        };

        links.push(serde_json::json!({
            "text": text,
            "url": resolved
        }));
    }

    let count = links.len();
    ToolResult::ok(serde_json::json!({
        "url": url,
        "links": links,
        "count": count
    }))
}

/// Extract HTML tables as JSON arrays
fn extract_tables(document: &Html, url: &str) -> ToolResult {
    let table_sel = match Selector::parse("table") {
        Ok(s) => s,
        Err(_) => return ToolResult::err("Failed to parse table selector"),
    };
    let th_sel = Selector::parse("th").unwrap();
    let tr_sel = Selector::parse("tr").unwrap();
    let td_sel = Selector::parse("td, th").unwrap();

    let mut tables = Vec::new();

    for table in document.select(&table_sel) {
        let mut headers: Vec<String> = Vec::new();
        for th in table.select(&th_sel) {
            let text: String = th.text().collect::<Vec<_>>().join("").trim().to_string();
            headers.push(text);
        }

        let mut rows: Vec<serde_json::Value> = Vec::new();
        for tr in table.select(&tr_sel) {
            if rows.len() >= 50 {
                break;
            }
            let cells: Vec<String> = tr
                .select(&td_sel)
                .map(|td| td.text().collect::<Vec<_>>().join("").trim().to_string())
                .collect();
            if cells.is_empty() || cells.iter().all(|c| c.is_empty()) {
                continue;
            }
            if !headers.is_empty() && cells.len() == headers.len() {
                let row: serde_json::Map<String, serde_json::Value> = headers
                    .iter()
                    .zip(cells.iter())
                    .map(|(h, c)| (h.clone(), serde_json::Value::String(c.clone())))
                    .collect();
                rows.push(serde_json::Value::Object(row));
            } else {
                rows.push(serde_json::json!(cells));
            }
        }

        if !rows.is_empty() {
            tables.push(serde_json::json!({
                "headers": headers,
                "rows": rows
            }));
        }
    }

    let table_count = tables.len();
    ToolResult::ok(serde_json::json!({
        "url": url,
        "tables": tables,
        "table_count": table_count
    }))
}

/// Extract elements matching a CSS selector
fn extract_elements(document: &Html, url: &str, selector: &str) -> ToolResult {
    let sel = match Selector::parse(selector) {
        Ok(s) => s,
        Err(e) => return ToolResult::err(format!("Invalid CSS selector '{}': {:?}", selector, e)),
    };

    let base_url = Url::parse(url).ok();
    let mut results = Vec::new();

    for el in document.select(&sel) {
        if results.len() >= 50 {
            break;
        }

        let text: String = el.text().collect::<Vec<_>>().join(" ").trim().to_string();
        let href = el.value().attr("href").unwrap_or("").to_string();
        let tag = el.value().name().to_string();

        // Resolve relative URLs
        let resolved_href = if !href.is_empty() {
            if let Some(ref base) = base_url {
                base.join(&href)
                    .map(|u| u.to_string())
                    .unwrap_or(href)
            } else {
                href
            }
        } else {
            String::new()
        };

        results.push(serde_json::json!({
            "text": text,
            "href": resolved_href,
            "tag": tag
        }));
    }

    let count = results.len();
    ToolResult::ok(serde_json::json!({
        "url": url,
        "selector": selector,
        "elements": results,
        "count": count
    }))
}
