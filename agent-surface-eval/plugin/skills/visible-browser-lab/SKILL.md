---
name: visible-browser-lab
description: Use for lease-scoped visible browser navigation, semantic page interaction, diagnostics, emulation, performance, audits, memory analysis, screencasts, and browser artifacts.
---

# Visible Browser Lab

Start each browser task with `start_session` and retain its `agent_session_id`. Use only `tab_id` values owned by that session.

Inspect an unfamiliar page with `snapshot`, then act through its element references. Use `fill` to replace one ordinary field. Use `fill_form` for two or more controls, including one operation that combines select and checkbox updates. Use `type_text` for contenteditable controls and insertion at an established caret; it preserves application focus and does not require `focus_tab`. Use `press_key` only for named keys or shortcuts after `focus_tab`.

Use `wait_for` for asynchronous state and `screenshot` for visual appearance. Use `console` and `network` for runtime diagnosis. Use `help` to select an operation in a specialized domain.

`click`, `press_key`, and native pointer operations return `focus_required` until `focus_tab` makes the owned document visible and focused. CSS and `evaluate` are escape hatches for state the accessibility snapshot and named semantic tools cannot represent. Do not use them to verify a semantic action.

Use `performance`, `audit`, `memory`, `screencast`, and `artifacts` for their corresponding investigation outputs. Release a tab that another agent may continue, or close a tab whose browser work is complete.
