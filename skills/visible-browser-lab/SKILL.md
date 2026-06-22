---
name: visible-browser-lab
description: Use when browser automation must run in a visible Chrome window the user can watch, especially for localhost v0 perf/debug loops, auth-sensitive flows, or CDP-backed page/network/frame inspection.
---

# Visible Browser Lab

Use this workflow when the user needs browser automation and visual confirmation in the same Chrome window.

## Start or Verify Chrome

Run the plugin script before browser work:

```bash
~/plugins/visible-browser-lab/scripts/start-visible-browser.sh http://localhost:3002/
```

Verify the CDP endpoint:

```bash
curl -fsS http://127.0.0.1:9222/json/version
```

The browser profile is persistent at `/Users/wycats/.cache/v0-visible-browser-profile`.

## Auth and Permissions

Browser-visible startup, auth, permission, SSO, account, scope, or team-selection steps are normal handoff points.

When one appears:

1. State the user action needed.
2. Wait for the user to complete it in the visible browser.
3. Continue with the same browser profile and CDP endpoint.

## Interaction Rules

- Use the `visible-playwright` MCP server for page interaction when it is available.
- Treat visible-playwright tabs as agent-owned resources: create or explicitly claim one tab at the start of the workflow, record the returned tab/page ID, and use that ID for every later tab selection, navigation, and page action.
- Do not act on the active tab, tab index, title, or URL alone. If the owned tab ID disappears, create or claim a new tab and record its ID before continuing.
- Do not switch to or mutate another agent's tab unless the user explicitly asks you to take over that tab.
- Use the `visible-devtools` MCP server for frame, network, console, and performance inspection when it is available.
- Use key-by-key entry for rich text editors when normal typing is unreliable.
- Keep the visible browser as the source of truth for what the user can see.
- For v0 local perf work, keep the target URL at `http://localhost:3002/` unless the user chooses another local port.
