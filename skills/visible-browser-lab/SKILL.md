---
name: visible-browser-lab
description: Use when browser automation must run in a visible Chrome window the user can watch, especially for localhost v0 perf/debug loops, auth-sensitive flows, or leased-tab navigation and screenshots.
---

# Visible Browser Lab

Use this workflow when the user needs browser automation and visual confirmation in the same Chrome window.

Visible Browser Lab exposes one MCP server, `visible-browser-lab`. It owns tab isolation through explicit sessions and leased tab IDs.

## Start or Verify Chrome

When working from this source checkout on macOS, run the Chrome startup script before browser work:

```bash
~/plugins/visible-browser-lab/scripts/start-visible-browser.sh http://localhost:3002/
```

For packaged installations or other hosts, ensure Chrome is listening on the CDP endpoint configured for `visible-browser-lab`.

Verify the CDP endpoint:

```bash
curl -fsS http://127.0.0.1:9222/json/version
```

The browser profile is persistent at `/Users/wycats/.cache/v0-visible-browser-profile`.

## Session Workflow

Start every browser workflow with `start_session`. Keep the returned `agent_session_id` for the full task and pass it on every later tool call.

Use leased `tab_id` values as the only browser-action handles. Browser actions require both `agent_session_id` and a tab owned by that session.

Use `list_tabs` with its default scope for normal work. The default list shows the caller's active and missing leases and hides released or closed leases. Use `global_readonly` only to understand the shared visible browser inventory; it groups foreign tabs by `owner_display_id` and withholds foreign `agent_session_id` and `tab_id` values.

Use `new_tab` to create an owned visible tab. Use `claim_tab` to claim an unowned Chrome target by `target_id`. Use takeover only when the user explicitly asks to transfer ownership, with a non-empty `user_instruction`; takeover returns a new leased `tab_id` and invalidates the previous active lease.

Use `focus_tab`, `navigate`, `screenshot`, `release_tab`, and `close_tab` only with an owned `tab_id`. `release_tab` leaves the Chrome target visible and claimable. `close_tab` closes the Chrome target and marks the lease closed.

If a leased target disappears, keep the missing lease visible in owned listings, create or claim another tab, and continue with the new `tab_id`.

## Auth and Permissions

Browser-visible startup, auth, permission, SSO, account, scope, or team-selection steps are normal handoff points.

When one appears:

1. State the user action needed.
2. Wait for the user to complete it in the visible browser.
3. Continue with the same browser profile and CDP endpoint.

## Interaction Rules

- Use the `visible-browser-lab` MCP server for visible-browser tab lifecycle, focus, navigation, and screenshots.
- Treat `agent_session_id` and `tab_id` as bearer handles. Record them in task notes when the workflow spans multiple steps.
- Use `target_id`, title, URL, and owner display information for diagnosis and handoff, not as substitutes for an owned `tab_id`.
- Record missing browser operations as plugin implementation gaps while preserving the current session and tab context.
- Use key-by-key entry for rich text editors when normal typing is unreliable.
- Keep the visible browser as the source of truth for what the user can see.
- For v0 local perf work, keep the target URL at `http://localhost:3002/` unless the user chooses another local port.
