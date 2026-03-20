/**
 * Chitty Browser Extension — Content Script
 *
 * Injected into every page. Provides DOM access for the background service worker.
 * Listens for messages from background.js and executes DOM operations.
 */

// Signal that content script is loaded
chrome.runtime.sendMessage({ type: 'content_ready', url: window.location.href });
