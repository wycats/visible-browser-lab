---
name: visible-browser-lab
description: Use for lease-scoped visible browser navigation, semantic page interaction, diagnostics, emulation, performance, audits, memory analysis, screencasts, and browser artifacts.
---

# Visible Browser Lab

Use this workflow when the user needs browser automation and visual confirmation in the same Chrome window.

Visible Browser Lab exposes one MCP server, `visible-browser-lab`. It owns tab isolation through explicit sessions and leased tab IDs.

## Browser Runtime

Start the browser workflow with an MCP tool call. With no CDP configuration, Visible Browser Lab starts or reuses its managed Chrome profile and creates tabs in a visible window without activating Chrome. The managed profile persists across broker and MCP server restarts so the user can inspect and interact with its tabs.

An explicit `VISIBLE_BROWSER_CDP_ENDPOINT`, `VISIBLE_BROWSER_CDP_PORT`, or `--cdp-endpoint` selects an existing Chrome instance. Use this external mode when the user or development environment already owns the browser lifecycle.

`VISIBLE_BROWSER_LAB_STATE_DIR` selects an isolated broker and managed profile. `VISIBLE_BROWSER_LAB_CHROME_PATH` selects a specific Chromium-family browser executable. Leave these unset for the installed default.

## Session Workflow

Call browser operations directly. Conversation-aware hosts select the session automatically. If a call returns `session_required`, call `start_session`, keep the returned `agent_session_id` for the full task, and pass it on every later tool call.

Use leased `tab_id` values as the browser-action handles. Conversation-aware hosts supply the ambient session out of band; include `agent_session_id` only when recovering through the explicit fallback workflow.

Use `list_tabs` with its default scope for normal work. The default list shows the caller's active and missing leases and hides released or closed leases. Use `global_readonly` only to understand the shared visible browser inventory; it groups foreign tabs by `owner_display_id` and withholds foreign `agent_session_id` and `tab_id` values.

Use `new_tab` to create an owned background tab. Set `focus: true` only when the user wants Chrome activated immediately. Use `claim_tab` to claim an unowned Chrome target by `target_id`. Use takeover only when the user explicitly asks to transfer ownership, with a non-empty `user_instruction`; takeover returns a new leased `tab_id` and invalidates the previous active lease.

Use every page-scoped tool only with an owned `tab_id`. `release_tab` makes the Chrome target visible and claimable, while VBL-created targets remain eligible for cleanup when the responsible session expires. Only when the user explicitly asks to keep a VBL-created target open after the session should you pass `leave_visible: true` together with their non-empty `user_instruction`. `close_tab` closes the Chrome target and marks the lease closed.

Inspect an unfamiliar page with `snapshot`. Its compact accessibility tree assigns short `ref` values to elements in the main document and frames. Pass those references to `click` and `fill`. A reference remains subordinate to its session, tab lease, frame, and document; after navigation or `element_stale`, call `snapshot` again. Use `{ "css": "..." }` only when the accessibility snapshot does not represent the target.

`fill` replaces one ordinary editable control. Use `fill_form` for two or more controls, including combined text, select, and checkbox updates. Use `type_text` for contenteditable controls and insertion at an established caret. Use `press_key` with a target for named keys or shortcuts against a resolved element. Use targetless `press_key` and `interact` `click_at` only after `focus_tab` has focused the owned document.

Mutating semantic actions return an accessibility diff by default; request `observe: "snapshot"` for the complete resulting tree or `observe: "none"` when no page observation is needed. `click` also returns action evidence with the delivery mode, resolved element, center hit-test, URL change, and relevant network events so quiet submits can be verified from the tool result. Navigation, waits, snapshots, screenshots, evaluation, form updates, text insertion, element-targeted click, targeted key, referenced pointer, diagnostics, and analysis preserve the user's active application. Routine actions attach to the owned target and prepare the resolved element without target activation. `focus_tab` and `focus: true` are the explicit operations for bringing managed Chrome forward when the user asks for manual inspection or handoff.

Use `wait_for` for asynchronous text, element, URL, load, or expression state. Use `screenshot` for visual appearance. Use `console` and `network` for runtime diagnosis. Use `help` to select an operation in `interact`, `emulation`, `performance`, `audit`, `memory`, `screencast`, or `artifacts`. Use `evaluate` or a strict CSS target only when the accessibility snapshot and named semantic tools cannot represent the required state. Do not use them to verify a semantic action.

If a leased target disappears, keep the missing lease visible in owned listings, create or claim another tab, and continue with the new `tab_id`.

## Auth and Permissions

Browser-visible startup, auth, permission, SSO, account, scope, or team-selection steps are normal handoff points.

When one appears:

1. State the user action needed.
2. Wait for the user to complete it in the visible browser.
3. Continue with the same browser profile and CDP endpoint.

## Interaction Rules

- Use the `visible-browser-lab` MCP server for tab lifecycle, semantic snapshots, navigation, page actions, diagnostics, emulation, analysis, capture, and artifacts.
- Treat leased `tab_id` values as bearer handles. In the explicit fallback workflow, treat `agent_session_id` as a bearer handle too and retain both for later calls; in ambient mode, never invent or request a session handle.
- Use `target_id`, title, URL, and owner display information for diagnosis and handoff, not as substitutes for an owned `tab_id`.
- Use `help` to inspect a domain operation before constructing specialized arguments.
- Use key-by-key entry for rich text editors when normal typing is unreliable.
- Keep the visible browser as the source of truth for what the user can see. Routine page actions preserve the user's active application. Target activation, including CDP `Target.activateTarget`, is reserved for `focus_tab` and `focus: true`.
- For v0 local perf work, keep the target URL at `http://localhost:3002/` unless the user chooses another local port.
