---
name: web-app-builder
description: >
  Build modern, high-quality single-page web applications, dashboards,
  tools, calculators, and interactive prototypes. Use when the user asks
  to create a web app, dashboard, data visualization, landing page, or
  any interactive HTML-based output — even if they don't explicitly say
  "web app."
allowed-tools: file_writer file_reader terminal browser
---

# Web App Builder

## Core Approach

Build self-contained single HTML files with inline CSS and JavaScript. No build
tools, no npm, no Node.js required. This matches Chitty's local-first philosophy.

Use CDN imports for libraries when needed:
- React 18: `https://esm.sh/react@18` + `https://esm.sh/react-dom@18`
- Tailwind CSS: `https://cdn.tailwindcss.com`
- Chart.js: `https://cdn.jsdelivr.net/npm/chart.js`
- D3.js: `https://d3js.org/d3.v7.min.js`

## Design Rules — Avoid AI Slop

- Clean, asymmetric, content-first layouts
- Dark mode by default (prefers-color-scheme: dark)
- System fonts: `-apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif`
- No purple gradients, no excessive rounded corners, no centered-everything
- Inspired by Linear, Arc, Stripe — minimal, functional, premium
- Subtle shadows and borders, not heavy box-shadows
- Responsive: works on desktop and mobile

## Workflow

1. Clarify requirements with the user (what data, what interactions)
2. Create the HTML file using file_writer
3. Wrap the complete output in an artifact tag for preview:

```
<artifact type="html" title="App Name Here">
<!DOCTYPE html>
<html lang="en">
...complete self-contained HTML...
</html>
</artifact>
```

4. If the user requests changes, produce an updated artifact with the same title

## Gotchas

- Chitty runs primarily on Windows — avoid bash-only scripts in artifacts
- Keep everything in a single HTML file when possible (easier to iterate)
- CDN imports require internet access — mention this if relevant
- Use `<script type="module">` for modern JS features
- Test dark/light mode by toggling prefers-color-scheme
- For charts: Chart.js is simpler than D3 for standard charts; use D3 for custom visualizations

## Output Format

Always wrap significant web outputs in artifact tags:

```
<artifact type="html" title="Descriptive Name">
...complete HTML...
</artifact>
```

For code snippets that aren't full applications, use regular code blocks instead.
