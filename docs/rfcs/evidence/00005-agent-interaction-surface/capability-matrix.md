# Visible Browser Lab Agent Interaction Surface Capability Matrix

## Baseline

The original plugin configured two unmodified MCP servers in commit `1165f61`:

- `@playwright/mcp@latest`, connected to the shared Chrome endpoint;
- `chrome-devtools-mcp@latest`, connected to the same endpoint with usage
  statistics disabled.

The package versions resolved for this baseline are `@playwright/mcp` 0.0.76
and `chrome-devtools-mcp` 1.3.0. The inventories below come from those exact
npm package archives and their generated tool references.

The mapping classifications are:

- **Explicit**: one stable top-level Visible Browser Lab tool.
- **Domain**: one operation on a task-named domain tool.
- **Lease**: one or more session and tab ownership tools.
- **Semantic**: a constrained workflow provides the browser outcome through
  the facade's ownership and page-operation contracts.

Every page-scoped mapping carries `agent_session_id` and `tab_id`. Ownership
validation precedes element, diagnostic, artifact, or CDP resolution.

## Playwright MCP 0.0.76 Default Operations

Playwright MCP enables Core automation and Tab management without an opt-in
capability flag. Its generated reference contains 23 default operations.

| Upstream operation | v0.3 contract | Mapping | Notes |
| --- | --- | --- | --- |
| `browser_click` | `click` | Explicit | Uses a snapshot reference by default; CSS is an explicit fallback. |
| `browser_close` | `close_tab` | Lease | Closes the owned Chrome target and transitions its lease to closed. |
| `browser_console_messages` | `console(operation: "list")` | Domain | Filters by level and sequence; artifact export replaces filename output. |
| `browser_drag` | `interact(operation: "drag")` | Domain | Resolves source and destination through lease-scoped element targets. |
| `browser_drop` | `interact(operation: "drop")` | Domain | Supports broker-approved files and MIME-typed data. |
| `browser_evaluate` | `evaluate` | Explicit | Evaluates in the owned page or against one resolved element target. |
| `browser_file_upload` | `interact(operation: "upload_files")` | Domain | Resolves workspace-relative paths and an explicit file-input target. |
| `browser_fill_form` | `fill_form` | Explicit | Applies typed field operations sequentially and reports partial completion. |
| `browser_handle_dialog` | `interact(operation: "handle_dialog")` | Domain | Accepts or dismisses the owned tab's pending JavaScript dialog. |
| `browser_hover` | `interact(operation: "hover")` | Domain | Requires the owned tab to be the focused visible document. |
| `browser_navigate` | `navigate(action: "url")` | Explicit | Navigates the owned tab without selecting a global page. |
| `browser_navigate_back` | `navigate(action: "back")` | Explicit | Uses the owned tab's session history. |
| `browser_network_request` | `network(operation: "get")` | Domain | Returns bounded request and response detail; large bodies become artifacts. |
| `browser_network_requests` | `network(operation: "list")` | Domain | Supports URL, resource type, status, and sequence filters. |
| `browser_press_key` | `press_key` | Explicit | Dispatches native keyboard input after the focused-document check. |
| `browser_resize` | `emulation(operation: "set_viewport")` | Domain | Sets the owned target's viewport and device presentation. |
| `browser_run_code_unsafe` | named page tools plus `evaluate` | Semantic | Page JavaScript remains available; server-process JavaScript execution is not part of the browser contract. |
| `browser_select_option` | `interact(operation: "select_options")` | Domain | Selects one or more values on one resolved control. |
| `browser_snapshot` | `snapshot` | Explicit | Returns a compact accessibility tree and lease-scoped element references. |
| `browser_take_screenshot` | `screenshot` | Explicit | Captures viewport, full-page, or referenced-element images. |
| `browser_type` | `fill` or `type_text`, followed by `press_key` when submitting | Semantic | `fill` replaces a value; `type_text` preserves insertion semantics. |
| `browser_wait_for` | `wait_for` | Explicit | Covers delay, text, element state, URL, load state, and expression conditions. |
| `browser_tabs` | `list_tabs`, `new_tab`, `focus_tab`, and `close_tab` | Lease | Replaces global page indices with owned tab leases. |

## Chrome DevTools MCP 1.3.0 Default Operations

Chrome DevTools MCP 1.3.0 defines 40 documented operations. With the original
configuration, 29 are functional. The other 11 require experimental vision,
experimental screencast, or memory-debugging flags. Emulation, Performance,
and Network categories are enabled by default.

| Upstream operation | v0.3 contract | Mapping | Notes |
| --- | --- | --- | --- |
| `click` | `click` | Explicit | Uses strict reference or CSS resolution and actionability checks. |
| `drag` | `interact(operation: "drag")` | Domain | Preserves element-to-element drag behavior. |
| `fill` | `fill` | Explicit | Replaces the value of one editable control. |
| `fill_form` | `fill_form` | Explicit | Supports text, select, and checked field operations. |
| `handle_dialog` | `interact(operation: "handle_dialog")` | Domain | Handles the dialog attached to the owned target. |
| `hover` | `interact(operation: "hover")` | Domain | Uses native pointer input after focus validation. |
| `press_key` | `press_key` | Explicit | Supports printable keys, named browser keys, and modifiers. |
| `type_text` | `type_text` | Explicit | Inserts text into a resolved editable target. |
| `upload_file` | `interact(operation: "upload_files")` | Domain | Uses workspace-bounded paths and a resolved file input. |
| `close_page` | `close_tab` | Lease | Closes only an owned target. |
| `list_pages` | `list_tabs` | Lease | Owned scope is the default; `global_readonly` withholds foreign action handles. |
| `navigate_page` | `navigate` | Explicit | Supports URL, back, forward, reload, cache, before-unload, and next-document init script behavior. |
| `new_page` | `new_tab` | Lease | Creates a background owned tab unless focus is explicitly requested. |
| `select_page` | `focus_tab` | Lease | Makes the owned tab visible and focused through an explicit transition. |
| `wait_for` | `wait_for` | Explicit | Waits against the owned document without changing tab ownership. |
| `emulate` | `emulation` | Domain | Maps network, CPU, geolocation, user agent, media, viewport, and HTTP header overrides. |
| `resize_page` | `emulation(operation: "set_viewport")` | Domain | Uses the same viewport contract as browser resize. |
| `performance_analyze_insight` | `performance(operation: "analyze")` | Domain | Analyzes a broker-produced trace artifact. |
| `performance_start_trace` | `performance(operation: "start_trace")` | Domain | Starts trace capture for the owned target. |
| `performance_stop_trace` | `performance(operation: "stop_trace")` | Domain | Stops capture and returns a trace artifact. |
| `get_network_request` | `network(operation: "get")` | Domain | Returns bounded details for one lease-scoped diagnostic ID. |
| `list_network_requests` | `network(operation: "list")` | Domain | Lists the owned tab's request diagnostics. |
| `evaluate_script` | `evaluate` | Explicit | Runs page JavaScript after lease validation. |
| `get_console_message` | `console(operation: "get")` | Domain | Returns one lease-scoped console message with source and stack detail. |
| `lighthouse_audit` | `audit` | Domain | Runs accessibility, SEO, best-practices, and agentic-browsing audits. |
| `list_console_messages` | `console(operation: "list")` | Domain | Lists and filters console diagnostics for the owned tab. |
| `take_screenshot` | `screenshot` | Explicit | Returns MCP image content and artifact metadata. |
| `take_snapshot` | `snapshot` | Explicit | Returns the semantic page model used by element actions. |
| `take_heapsnapshot` | `memory(operation: "capture")` | Domain | Captures a heap artifact bound to the owning session and tab. |

## Deliberate v0.3 Capabilities Beyond the Default Floor

Visible Browser Lab includes complete memory inspection and screencast domains
in its stable catalog. Chrome DevTools MCP 1.3.0 documents these operations but
requires `--memory-debugging` or `--experimental-screencast` for the operations
listed below. They are v0.3 product capabilities, not evidence for the original
unconfigured compatibility floor.

| Chrome DevTools MCP operation | v0.3 contract |
| --- | --- |
| `close_heapsnapshot` | `memory(operation: "close")` |
| `get_heapsnapshot_class_nodes` | `memory(operation: "classes")` |
| `get_heapsnapshot_details` | `memory(operation: "node")` |
| `get_heapsnapshot_dominators` | `memory(operation: "dominators")` |
| `get_heapsnapshot_edges` | `memory(operation: "edges")` |
| `get_heapsnapshot_retainers` | `memory(operation: "retainers")` |
| `get_heapsnapshot_retaining_paths` | `memory(operation: "retaining_paths")` |
| `get_heapsnapshot_summary` | `memory(operation: "summary")` |
| `screencast_start` | `screencast(operation: "start")` |
| `screencast_stop` | `screencast(operation: "stop")` |

The `click_at` operation requires Chrome DevTools MCP's experimental vision
flag. Visible Browser Lab exposes the same operation under `interact` because
coordinate input is useful for canvas and other non-semantic surfaces. It
follows the focused-document contract and is not the documented path for DOM
elements.

Chrome DevTools MCP's extension, experimental third-party, WebMCP, tab-ID
interop, and Playwright's opt-in config, network interception, storage,
DevTools, PDF, vision, and verification packs are outside this v0.3 contract.

## Agent Workflow Capabilities

The v0.3 contract adds behavior that is not represented by one upstream tool:

| Capability | Contract |
| --- | --- |
| User-observable discovery | `snapshot` prioritizes role and accessible name, label, placeholder or visible text, alternate text or title, and test ID. |
| Strict element identity | Element references are bound to session, tab lease, frame, document revision, and broker generation. |
| Frame and shadow traversal | Snapshot references carry frame and open-shadow-root identity without global frame handles. |
| Action readiness | Element actions wait for operation-specific visibility, stability, enabled, editable, pointer-event, and hit-test conditions. |
| Bounded follow-up context | Mutating page actions return no observation, an accessibility diff, or a full snapshot; the default is a diff. |
| Task-oriented discovery | `help` maps a browser task to one explicit tool or domain operation and names recovery actions. |
| Artifact authority | Screenshots, traces, audits, heap snapshots, recordings, and oversized diagnostics use owner-scoped artifact handles. |
| Ownership refusal | Every page operation rejects a foreign `tab_id` before resolving page state. |

The semantic query order follows Testing Library's user-facing query priority.
The action readiness model follows Playwright's operation-specific actionability
checks. AgentChrome and chrome-agent provide design evidence for compact
accessibility snapshots and backend-node references; Visible Browser Lab keeps
those references subordinate to its tab leases.

## Contract Adjustments Required Before Stage 2

The operation inventory identifies these schema requirements for RFC `00005`:

- `wait_for` includes a bounded delay condition.
- `navigate` includes before-unload handling and a next-document init script.
- `type_text` includes per-character delay; submission remains an explicit
  `press_key` operation.
- `emulation` covers user-agent and extra-HTTP-header overrides in addition to
  viewport, network, CPU, geolocation, and media state.
- `interact` publishes exact tagged schemas for selection, checked state,
  hover, drag, drop, file upload, dialogs, scroll, and coordinate input.
- Diagnostic operations can materialize bounded results as artifacts, and
  `artifacts(operation: "export")` supplies workspace-relative file output.

## Sources

- [Playwright MCP tools](https://github.com/microsoft/playwright-mcp#tools)
- [Chrome DevTools MCP tool reference](https://github.com/ChromeDevTools/chrome-devtools-mcp/blob/main/docs/tool-reference.md)
- [Testing Library query priority](https://testing-library.com/docs/queries/about)
- [Playwright actionability](https://playwright.dev/docs/actionability)
- [AgentChrome](https://github.com/Nunley-Media-Group/AgentChrome)
- [chrome-agent](https://github.com/sderosiaux/chrome-agent)
