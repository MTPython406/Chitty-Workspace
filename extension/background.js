/**
 * Chitty Browser Extension — Background Service Worker
 *
 * Connects to Chitty Workspace server via HTTP polling.
 * Polls for pending browser commands, executes them, sends results back.
 *
 * Uses HTTP instead of WebSocket because Manifest V3 service workers
 * can't maintain persistent WebSocket connections (they go inactive).
 */

const CHITTY_BASE = 'http://127.0.0.1:8770';
let polling = false;
let activeTabId = null;

// ── Polling Loop ──────────────────────────────────────

async function startPolling() {
  if (polling) return;
  polling = true;
  console.log('[Chitty] Starting command polling');
  updateBadge('ON', '#10b981');

  while (polling) {
    try {
      // Long-poll: server holds the request until a command is available (up to 25s)
      const resp = await fetch(`${CHITTY_BASE}/api/browser/poll`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ timeout: 25000 }),
      });

      if (!resp.ok) {
        if (resp.status === 204) continue; // No command pending, poll again
        throw new Error(`Server returned ${resp.status}`);
      }

      const cmd = await resp.json();
      if (!cmd || !cmd.action) continue;

      console.log('[Chitty] Command:', cmd.action, cmd.id);
      const result = await executeCommand(cmd);

      // Send result back
      await fetch(`${CHITTY_BASE}/api/browser/result`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(result),
      });
    } catch (e) {
      // Server not running or network error — wait and retry
      console.log('[Chitty] Poll error:', e.message);
      updateBadge('OFF', '#ef4444');
      await sleep(3000);
      // Check if server is back
      try {
        const health = await fetch(`${CHITTY_BASE}/health`);
        if (health.ok) {
          updateBadge('ON', '#10b981');
          console.log('[Chitty] Reconnected to Chitty Workspace');
        }
      } catch (_) { /* still down */ }
    }
  }
}

function sleep(ms) {
  return new Promise(r => setTimeout(r, ms));
}

function updateBadge(text, color) {
  chrome.action.setBadgeText({ text });
  chrome.action.setBadgeBackgroundColor({ color });
}

// ── Command Execution ──────────────────────────────────────

async function executeCommand(cmd) {
  const { id, action, params } = cmd;

  try {
    switch (action) {
      case 'open':
        return await cmdOpen(id, params);
      case 'click':
        return await cmdClick(id, params);
      case 'type':
        return await cmdType(id, params);
      case 'read_text':
        return await cmdReadText(id, params);
      case 'screenshot':
        return await cmdScreenshot(id);
      case 'execute_js':
        return await cmdExecuteJs(id, params);
      case 'wait_for':
        return await cmdWaitFor(id, params);
      case 'page_info':
        return await cmdPageInfo(id);
      case 'close':
        return await cmdClose(id);
      default:
        return { id, success: false, data: null, error: `Unknown action: ${action}` };
    }
  } catch (e) {
    return { id, success: false, data: null, error: e.message || String(e) };
  }
}

// ── Open URL ──────────────────────────────────────

async function cmdOpen(id, params) {
  const url = params.url;
  if (!url) return { id, success: false, data: null, error: 'Missing url' };

  if (activeTabId) {
    try {
      await chrome.tabs.update(activeTabId, { url, active: true });
    } catch (e) {
      const tab = await chrome.tabs.create({ url, active: true });
      activeTabId = tab.id;
    }
  } else {
    const tab = await chrome.tabs.create({ url, active: true });
    activeTabId = tab.id;
  }

  await waitForTabLoad(activeTabId, 15000);
  const tab = await chrome.tabs.get(activeTabId);

  return {
    id, success: true,
    data: JSON.stringify({ url: tab.url, title: tab.title, status: 'loaded' }),
    error: null
  };
}

// ── Click Element ──────────────────────────────────────

async function cmdClick(id, params) {
  const selector = params.selector;
  if (!selector) return { id, success: false, data: null, error: 'Missing selector' };
  if (!activeTabId) return { id, success: false, data: null, error: 'No active tab' };

  const results = await chrome.scripting.executeScript({
    target: { tabId: activeTabId },
    func: (sel) => {
      const el = document.querySelector(sel);
      if (!el) return { ok: false, error: `Element not found: ${sel}` };
      el.scrollIntoView({ block: 'center' });
      el.click();
      return { ok: true };
    },
    args: [selector]
  });

  const result = results[0]?.result;
  if (result?.ok) return { id, success: true, data: `Clicked: ${selector}`, error: null };
  return { id, success: false, data: null, error: result?.error || 'Click failed' };
}

// ── Type Text ──────────────────────────────────────

async function cmdType(id, params) {
  const selector = params.selector;
  const text = params.text;
  if (!selector) return { id, success: false, data: null, error: 'Missing selector' };
  if (text === undefined) return { id, success: false, data: null, error: 'Missing text' };
  if (!activeTabId) return { id, success: false, data: null, error: 'No active tab' };

  const results = await chrome.scripting.executeScript({
    target: { tabId: activeTabId },
    func: (sel, txt) => {
      const el = document.querySelector(sel);
      if (!el) return { ok: false, error: `Element not found: ${sel}` };
      el.scrollIntoView({ block: 'center' });
      el.focus();
      el.click();
      if (el.contentEditable === 'true' || el.getAttribute('role') === 'textbox') {
        el.innerHTML = '';
        document.execCommand('insertText', false, txt);
      } else {
        el.value = txt;
        el.dispatchEvent(new Event('input', { bubbles: true }));
        el.dispatchEvent(new Event('change', { bubbles: true }));
      }
      return { ok: true, chars: txt.length };
    },
    args: [selector, text]
  });

  const result = results[0]?.result;
  if (result?.ok) return { id, success: true, data: `Typed ${result.chars} chars into ${selector}`, error: null };
  return { id, success: false, data: null, error: result?.error || 'Type failed' };
}

// ── Read Text ──────────────────────────────────────

async function cmdReadText(id, params) {
  if (!activeTabId) return { id, success: false, data: null, error: 'No active tab' };
  const selector = params?.selector;

  const results = await chrome.scripting.executeScript({
    target: { tabId: activeTabId },
    func: (sel) => {
      if (sel) {
        const el = document.querySelector(sel);
        if (!el) return { ok: false, error: `Element not found: ${sel}` };
        return { ok: true, text: el.innerText.substring(0, 8000) };
      }
      return { ok: true, text: document.body.innerText.substring(0, 8000) };
    },
    args: [selector || null]
  });

  const result = results[0]?.result;
  if (result?.ok) {
    return { id, success: true, data: JSON.stringify({ text: result.text, selector: selector || 'body' }), error: null };
  }
  return { id, success: false, data: null, error: result?.error || 'Read failed' };
}

// ── Screenshot ──────────────────────────────────────

async function cmdScreenshot(id) {
  if (!activeTabId) return { id, success: false, data: null, error: 'No active tab' };
  const dataUrl = await chrome.tabs.captureVisibleTab(null, { format: 'png' });
  const tab = await chrome.tabs.get(activeTabId);
  return {
    id, success: true,
    data: JSON.stringify({ screenshot_base64: dataUrl.replace('data:image/png;base64,', ''), title: tab.title, url: tab.url }),
    error: null
  };
}

// ── Execute JavaScript ──────────────────────────────────────

async function cmdExecuteJs(id, params) {
  if (!activeTabId) return { id, success: false, data: null, error: 'No active tab' };
  const script = params.script;
  if (!script) return { id, success: false, data: null, error: 'Missing script' };

  // Use chrome.scripting.executeScript with world: 'MAIN' to run in page context
  // without eval() which triggers CSP violations on sites like LinkedIn
  try {
    const results = await chrome.scripting.executeScript({
      target: { tabId: activeTabId },
      world: 'MAIN',
      func: new Function('return (' + script + ')'),
      args: []
    });
    const result = results[0]?.result;
    return { id, success: true, data: JSON.stringify(result), error: null };
  } catch (e) {
    // Fallback: run in isolated world (can't access page JS vars but no CSP issues)
    try {
      const results = await chrome.scripting.executeScript({
        target: { tabId: activeTabId },
        func: (code) => {
          try {
            const fn = new Function(code);
            return { ok: true, result: fn() };
          } catch (e2) {
            return { ok: false, error: e2.message };
          }
        },
        args: [script]
      });
      const result = results[0]?.result;
      if (result?.ok) return { id, success: true, data: JSON.stringify(result.result), error: null };
      return { id, success: false, data: null, error: result?.error || 'JS execution failed' };
    } catch (e2) {
      return { id, success: false, data: null, error: e2.message || 'JS execution failed' };
    }
  }
}

// ── Wait For Element ──────────────────────────────────────

async function cmdWaitFor(id, params) {
  if (!activeTabId) return { id, success: false, data: null, error: 'No active tab' };
  const selector = params.selector;
  const timeout = params.timeout || 10000;
  if (!selector) return { id, success: false, data: null, error: 'Missing selector' };

  const start = Date.now();
  while (Date.now() - start < timeout) {
    const results = await chrome.scripting.executeScript({
      target: { tabId: activeTabId },
      func: (sel) => !!document.querySelector(sel),
      args: [selector]
    });
    if (results[0]?.result) return { id, success: true, data: `Element found: ${selector}`, error: null };
    await sleep(500);
  }
  return { id, success: false, data: null, error: `Timeout waiting for: ${selector}` };
}

// ── Page Info ──────────────────────────────────────

async function cmdPageInfo(id) {
  if (!activeTabId) return { id, success: false, data: null, error: 'No active tab' };
  const tab = await chrome.tabs.get(activeTabId);
  const results = await chrome.scripting.executeScript({
    target: { tabId: activeTabId },
    func: () => document.body.innerText.substring(0, 2000),
    args: []
  });
  return {
    id, success: true,
    data: JSON.stringify({ url: tab.url, title: tab.title, text_snippet: results[0]?.result || '' }),
    error: null
  };
}

// ── Close Tab ──────────────────────────────────────

async function cmdClose(id) {
  if (activeTabId) {
    try { await chrome.tabs.remove(activeTabId); } catch (e) { /* already closed */ }
    activeTabId = null;
  }
  return { id, success: true, data: 'Tab closed', error: null };
}

// ── Helpers ──────────────────────────────────────

function waitForTabLoad(tabId, timeout) {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, timeout);
    function listener(id, info) {
      if (id === tabId && info.status === 'complete') {
        chrome.tabs.onUpdated.removeListener(listener);
        clearTimeout(timer);
        setTimeout(resolve, 500);
      }
    }
    chrome.tabs.onUpdated.addListener(listener);
  });
}

// ── Startup ──────────────────────────────────────

startPolling();
chrome.runtime.onStartup.addListener(startPolling);
chrome.runtime.onInstalled.addListener(startPolling);
