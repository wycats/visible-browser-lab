---
name: visible-browser-lab
description: Use when browser automation must run in a visible Chrome window the user can watch, especially for localhost v0 perf/debug loops, auth-sensitive flows, or owned-tab navigation, screenshots, page actions, and diagnostics.
---

# Visible Browser Lab

Use this workflow when the user needs browser automation and visual confirmation in the same Chrome window.

Visible Browser Lab exposes one MCP server, `visible-browser-lab`. It owns tab isolation through explicit sessions and leased tab IDs.

## Browser Runtime

Start the browser workflow with an MCP tool call. With no CDP configuration, Visible Browser Lab starts or reuses its managed Chrome profile and creates tabs in a visible window without activating Chrome. The managed profile persists across broker and MCP server restarts so the user can inspect and interact with its tabs.

An explicit `VISIBLE_BROWSER_CDP_ENDPOINT`, `VISIBLE_BROWSER_CDP_PORT`, or `--cdp-endpoint` selects an existing Chrome instance. Use this external mode when the user or development environment already owns the browser lifecycle.

`VISIBLE_BROWSER_LAB_STATE_DIR` selects an isolated broker and managed profile. `VISIBLE_BROWSER_LAB_CHROME_PATH` selects a specific Chromium-family browser executable. Leave these unset for the installed default.

## Session Workflow

Start every browser workflow with `start_session`. Keep the returned `agent_session_id` for the full task and pass it on every later tool call.

Use leased `tab_id` values as the only browser-action handles. Browser actions require both `agent_session_id` and a tab owned by that session.

Use `list_tabs` with its default scope for normal work. The default list shows the caller's active and missing leases and hides released or closed leases. Use `global_readonly` only to understand the shared visible browser inventory; it groups foreign tabs by `owner_display_id` and withholds foreign `agent_session_id` and `tab_id` values.

Use `new_tab` to create an owned background tab. Set `focus: true` only when the user wants Chrome activated immediately. Use `claim_tab` to claim an unowned Chrome target by `target_id`. Use takeover only when the user explicitly asks to transfer ownership, with a non-empty `user_instruction`; takeover returns a new leased `tab_id` and invalidates the previous active lease.

Use `focus_tab`, `navigate`, `screenshot`, `evaluate`, `click`, `type_text`, `press_key`, `console_messages`, `network_events`, `release_tab`, and `close_tab` only with an owned `tab_id`. `release_tab` leaves the Chrome target visible and claimable. `close_tab` closes the Chrome target and marks the lease closed.

Use `evaluate` for main-frame JavaScript expressions. Navigation, screenshots, evaluation, text insertion, and diagnostics preserve application focus. Use `click` with a main-frame CSS selector; it finds the first visible matching element, scrolls it into view, and dispatches a left-click at its center. `click` and `press_key` return `focus_required` while the owned tab lacks browser focus; invoke `focus_tab` and retry the action. Use `type_text` after focusing the intended DOM element. Use `console_messages` and `network_events` to inspect broker-owned diagnostics collected for the leased target.

If a leased target disappears, keep the missing lease visible in owned listings, create or claim another tab, and continue with the new `tab_id`.

## Auth and Permissions

Browser-visible startup, auth, permission, SSO, account, scope, or team-selection steps are normal handoff points.

When one appears:

1. State the user action needed.
2. Wait for the user to complete it in the visible browser.
3. Continue with the same browser profile and CDP endpoint.

## Interaction Rules

- Use the `visible-browser-lab` MCP server for visible-browser tab lifecycle, focus, navigation, screenshots, page actions, and diagnostics.
- Treat `agent_session_id` and `tab_id` as bearer handles. Record them in task notes when the workflow spans multiple steps.
- Use `target_id`, title, URL, and owner display information for diagnosis and handoff, not as substitutes for an owned `tab_id`.
- Record missing browser operations as plugin implementation gaps while preserving the current session and tab context.
- Use key-by-key entry for rich text editors when normal typing is unreliable.
- Keep the visible browser as the source of truth for what the user can see. Background-safe actions preserve the user's active application; `focus_tab` is the explicit transition that brings managed Chrome forward.
- For v0 local perf work, keep the target URL at `http://localhost:3002/` unless the user chooses another local port.
