//! Browser Engine — Real browser automation via Chrome DevTools Protocol
//!
//! Uses chromiumoxide to control the user's installed Chrome or Edge browser.
//! Gives the agent full DOM access: navigate, click, type, screenshot, execute JS
//! on ANY site including LinkedIn, X.com, Gmail, etc.
//!
//! The user's existing browser profile (cookies, saved passwords) can be used,
//! so they stay logged in to their accounts.

use anyhow::Result;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::page::ScreenshotParams;
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Result from a browser action, returned to the tool layer
#[derive(Debug, Clone)]
pub struct BrowserActionResult {
    pub success: bool,
    pub data: String,
    pub error: Option<String>,
}

impl BrowserActionResult {
    pub fn ok(data: impl Into<String>) -> Self {
        Self { success: true, data: data.into(), error: None }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        Self { success: false, data: String::new(), error: Some(msg) }
    }
}

/// Manages a CDP-controlled browser instance
pub struct BrowserEngine {
    browser: Option<Browser>,
    /// Handle for the browser process event loop
    _handle: Option<tokio::task::JoinHandle<()>>,
    /// Current page (tab)
    page: Option<Arc<chromiumoxide::Page>>,
    /// Path to Chrome/Edge executable (auto-detected or user-configured)
    chrome_path: Option<PathBuf>,
    /// User data directory for browser profile (cookies, passwords)
    user_data_dir: Option<PathBuf>,
    /// Whether to run headless (false = user can see the browser)
    headless: bool,
}

impl BrowserEngine {
    pub fn new() -> Self {
        Self {
            browser: None,
            _handle: None,
            page: None,
            chrome_path: None,
            user_data_dir: None,
            headless: false, // Default: visible so user can watch and intervene
        }
    }

    /// Configure the Chrome/Edge executable path
    pub fn with_chrome_path(mut self, path: PathBuf) -> Self {
        self.chrome_path = Some(path);
        self
    }

    /// Configure the user data directory (for persistent login sessions)
    pub fn with_user_data_dir(mut self, dir: PathBuf) -> Self {
        self.user_data_dir = Some(dir);
        self
    }

    /// Set headless mode
    pub fn with_headless(mut self, headless: bool) -> Self {
        self.headless = headless;
        self
    }

    /// Is the browser currently connected?
    pub fn is_connected(&self) -> bool {
        self.browser.is_some()
    }

    /// Launch or connect to a Chrome/Edge browser
    pub async fn launch(&mut self) -> Result<()> {
        if self.browser.is_some() {
            tracing::info!("Browser already launched");
            return Ok(());
        }

        let chrome_path = self.chrome_path.clone()
            .or_else(find_chrome)
            .ok_or_else(|| anyhow::anyhow!(
                "Chrome or Edge not found. Install Google Chrome or Microsoft Edge, \
                 or set browser.chrome_path in config."
            ))?;

        tracing::info!("Launching browser: {:?}", chrome_path);

        let mut builder = BrowserConfig::builder()
            .chrome_executable(chrome_path)
            .window_size(1280, 900)
            // Look like a normal browser, not an automation tool
            .arg("--disable-blink-features=AutomationControlled")
            // Open prominently — user should see the browser
            .arg("--start-maximized")
            .arg("--new-window")
            // Suppress "Chrome is being controlled by automated software" bar
            .arg("--disable-infobars")
            .arg("--no-first-run")
            .arg("--no-default-browser-check");

        if self.headless {
            builder = builder.arg("--headless=new");
        }

        // Use a persistent profile so the user stays logged in
        // (saved passwords, cookies, login sessions persist between uses)
        if let Some(ref data_dir) = self.user_data_dir {
            builder = builder.user_data_dir(data_dir.clone());
        }

        let config = builder.build()
            .map_err(|e| anyhow::anyhow!("Browser config error: {}", e))?;

        let (browser, mut handler) = Browser::launch(config).await?;

        // Spawn the CDP event handler
        let handle = tokio::spawn(async move {
            loop {
                let _ = handler.next().await;
            }
        });

        self.browser = Some(browser);
        self._handle = Some(handle);

        tracing::info!("Browser launched successfully via CDP");
        Ok(())
    }

    /// Ensure we have an active page (tab). Creates one if needed.
    async fn ensure_page(&mut self) -> Result<Arc<chromiumoxide::Page>> {
        if let Some(ref page) = self.page {
            return Ok(page.clone());
        }

        let browser = self.browser.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Browser not launched. Call launch() first."))?;

        let page = Arc::new(browser.new_page("about:blank").await?);
        self.page = Some(page.clone());
        Ok(page)
    }

    /// Navigate to a URL
    pub async fn navigate(&mut self, url: &str) -> BrowserActionResult {
        match self._navigate(url).await {
            Ok(info) => BrowserActionResult::ok(info),
            Err(e) => BrowserActionResult::err(format!("Navigation failed: {}", e)),
        }
    }

    async fn _navigate(&mut self, url: &str) -> Result<String> {
        let page = self.ensure_page().await?;
        page.goto(url).await?.wait_for_navigation().await?;

        // Bring the browser window to the front so the user sees it
        let _ = page.evaluate("window.focus()").await;

        let title = page.evaluate("document.title").await?
            .into_value::<String>().unwrap_or_default();

        Ok(serde_json::json!({
            "url": url,
            "title": title,
            "status": "loaded",
            "note": "Page is open in the Chitty Browser window. The user can see and interact with it."
        }).to_string())
    }

    /// Click an element by CSS selector
    pub async fn click(&mut self, selector: &str) -> BrowserActionResult {
        match self._click(selector).await {
            Ok(()) => BrowserActionResult::ok(format!("Clicked: {}", selector)),
            Err(e) => BrowserActionResult::err(format!("Click failed on '{}': {}", selector, e)),
        }
    }

    async fn _click(&mut self, selector: &str) -> Result<()> {
        let page = self.ensure_page().await?;
        let el = page.find_element(selector).await?;
        el.click().await?;
        // Small delay for any navigation or JS to settle
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        Ok(())
    }

    /// Type text into an element
    pub async fn type_text(&mut self, selector: &str, text: &str) -> BrowserActionResult {
        match self._type_text(selector, text).await {
            Ok(()) => BrowserActionResult::ok(format!("Typed {} chars into {}", text.len(), selector)),
            Err(e) => BrowserActionResult::err(format!("Type failed on '{}': {}", selector, e)),
        }
    }

    async fn _type_text(&mut self, selector: &str, text: &str) -> Result<()> {
        let page = self.ensure_page().await?;
        let el = page.find_element(selector).await?;
        el.click().await?;
        el.type_str(text).await?;
        Ok(())
    }

    /// Read text content from the page or a specific element
    pub async fn read_text(&mut self, selector: Option<&str>) -> BrowserActionResult {
        match self._read_text(selector).await {
            Ok(text) => BrowserActionResult::ok(text),
            Err(e) => BrowserActionResult::err(format!("Read failed: {}", e)),
        }
    }

    async fn _read_text(&mut self, selector: Option<&str>) -> Result<String> {
        let page = self.ensure_page().await?;

        let script = if let Some(sel) = selector {
            format!(
                "(() => {{ const el = document.querySelector('{}'); return el ? el.innerText : 'Element not found: {}'; }})()",
                sel.replace('\'', "\\'"), sel.replace('\'', "\\'")
            )
        } else {
            "document.body.innerText.substring(0, 8000)".to_string()
        };

        let result = page.evaluate(script).await?;
        let text = result.into_value::<String>().unwrap_or_default();

        Ok(serde_json::json!({
            "text": text,
            "selector": selector.unwrap_or("body"),
            "url": page.url().await.unwrap_or_default()
        }).to_string())
    }

    /// Take a screenshot and return as base64 PNG
    pub async fn screenshot(&mut self) -> BrowserActionResult {
        match self._screenshot().await {
            Ok(data) => BrowserActionResult::ok(data),
            Err(e) => BrowserActionResult::err(format!("Screenshot failed: {}", e)),
        }
    }

    async fn _screenshot(&mut self) -> Result<String> {
        let page = self.ensure_page().await?;
        let params = ScreenshotParams::builder()
            .format(CaptureScreenshotFormat::Png)
            .build();
        let png_data = page.screenshot(params).await?;
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png_data);

        let title = page.evaluate("document.title").await?
            .into_value::<String>().unwrap_or_default();
        let url = page.url().await.unwrap_or_default();

        Ok(serde_json::json!({
            "screenshot_base64": b64,
            "title": title,
            "url": url,
            "width": 1280,
            "height": 900
        }).to_string())
    }

    /// Execute JavaScript in the page context
    pub async fn execute_js(&mut self, script: &str) -> BrowserActionResult {
        match self._execute_js(script).await {
            Ok(result) => BrowserActionResult::ok(result),
            Err(e) => BrowserActionResult::err(format!("JS execution failed: {}", e)),
        }
    }

    async fn _execute_js(&mut self, script: &str) -> Result<String> {
        let page = self.ensure_page().await?;
        let result = page.evaluate(script.to_string()).await?;
        let value = result.into_value::<serde_json::Value>().unwrap_or(serde_json::Value::Null);
        Ok(value.to_string())
    }

    /// Wait for an element to appear on the page
    pub async fn wait_for(&mut self, selector: &str, timeout_ms: u64) -> BrowserActionResult {
        match self._wait_for(selector, timeout_ms).await {
            Ok(()) => BrowserActionResult::ok(format!("Element found: {}", selector)),
            Err(e) => BrowserActionResult::err(format!("Wait timed out for '{}': {}", selector, e)),
        }
    }

    async fn _wait_for(&mut self, selector: &str, timeout_ms: u64) -> Result<()> {
        let page = self.ensure_page().await?;
        let timeout = std::time::Duration::from_millis(timeout_ms);
        let start = std::time::Instant::now();

        loop {
            let script = format!(
                "document.querySelector('{}') !== null",
                selector.replace('\'', "\\'")
            );
            let found = page.evaluate(script).await?
                .into_value::<bool>().unwrap_or(false);
            if found {
                return Ok(());
            }
            if start.elapsed() > timeout {
                anyhow::bail!("Timeout after {}ms", timeout_ms);
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    /// Get current page info (URL, title, visible text snippet)
    pub async fn get_page_info(&mut self) -> BrowserActionResult {
        match self._get_page_info().await {
            Ok(info) => BrowserActionResult::ok(info),
            Err(e) => BrowserActionResult::err(format!("Failed to get page info: {}", e)),
        }
    }

    async fn _get_page_info(&mut self) -> Result<String> {
        let page = self.ensure_page().await?;
        let title = page.evaluate("document.title").await?
            .into_value::<String>().unwrap_or_default();
        let url = page.url().await.unwrap_or_default();
        let text_snippet = page.evaluate("document.body.innerText.substring(0, 2000)").await?
            .into_value::<String>().unwrap_or_default();

        Ok(serde_json::json!({
            "url": url,
            "title": title,
            "text_snippet": text_snippet
        }).to_string())
    }

    /// Close the current tab
    pub async fn close_tab(&mut self) -> BrowserActionResult {
        if let Some(page) = self.page.take() {
            drop(page);
            BrowserActionResult::ok("Tab closed")
        } else {
            BrowserActionResult::ok("No tab open")
        }
    }

    /// Shut down the browser completely
    pub async fn shutdown(&mut self) {
        self.page = None;
        if let Some(browser) = self.browser.take() {
            drop(browser);
        }
        if let Some(handle) = self._handle.take() {
            handle.abort();
        }
        tracing::info!("Browser engine shut down");
    }
}

impl Drop for BrowserEngine {
    fn drop(&mut self) {
        if let Some(handle) = self._handle.take() {
            handle.abort();
        }
    }
}

/// Auto-detect Chrome or Edge on the system
fn find_chrome() -> Option<PathBuf> {
    let candidates = if cfg!(target_os = "windows") {
        vec![
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
        ]
    } else if cfg!(target_os = "macos") {
        vec![
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ]
    } else {
        vec![
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium-browser",
            "/usr/bin/chromium",
            "/usr/bin/microsoft-edge",
        ]
    };

    for path in candidates {
        let p = PathBuf::from(path);
        if p.exists() {
            tracing::info!("Found browser: {:?}", p);
            return Some(p);
        }
    }

    // Try PATH
    if let Ok(output) = std::process::Command::new(if cfg!(windows) { "where" } else { "which" })
        .arg("chrome")
        .output()
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }

    None
}

/// Thread-safe wrapper for the browser engine
pub type SharedBrowserEngine = Arc<Mutex<BrowserEngine>>;

pub fn create_shared_engine() -> SharedBrowserEngine {
    let data_dir = crate::storage::default_data_dir().join("browser-profile");
    let _ = std::fs::create_dir_all(&data_dir);

    let engine = BrowserEngine::new()
        .with_user_data_dir(data_dir)
        .with_headless(false);

    Arc::new(Mutex::new(engine))
}
