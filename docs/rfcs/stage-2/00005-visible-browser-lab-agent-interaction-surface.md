<!-- exo:5 ulid:01kw05h7avwyrejya6dw5fpx2n -->

# RFC 5: Visible Browser Lab Agent Interaction Surface

# Summary

Visible Browser Lab exposes one lease-scoped browser interface for agents that inspect, operate, and diagnose a shared visible Chrome profile. The interface combines explicit tools for the normal browser workflow with domain tools for specialized interaction, diagnostics, emulation, analysis, memory inspection, and capture.

Agents inspect unfamiliar pages through compact accessibility snapshots. Snapshots issue short element references that remain subordinate to `agent_session_id` and `tab_id`; the broker validates tab ownership before resolving a reference or invoking Chrome. CSS selectors remain available as an explicit fallback for pages whose useful targets are absent from the accessibility representation.

The v0.3 interface is one coherent contract for tab ownership, semantic page interaction, browser diagnostics, and generated artifacts.

# Motivation

A browser agent needs a reliable path from intent to action:

1. Establish an owned browser session and tab.
2. Observe the page in terms a user can perceive.
3. Identify the element that satisfies the task.
4. Perform an action after the target becomes actionable.
5. Observe the resulting page state.
6. Inspect browser diagnostics when the result differs from the intended behavior.

Tab ownership already gives each agent a stable working set. The page interface completes that model by giving agents semantic discovery, durable action vocabulary, explicit recovery paths, and bounded access to browser diagnostics.

CSS-only interaction makes page discovery depend on JavaScript evaluation or selector invention. A flat catalog that mirrors every browser protocol operation consumes context and makes tool selection harder. The interface defined here keeps the normal path explicit and groups specialized operations by the problem an agent is trying to solve.

# System Contract

## Ownership Boundary

The broker remains the authority for browser sessions, tab leases, Chrome targets, and element references.

Every page-scoped request carries `agent_session_id` and `tab_id`. The broker performs these checks in order:

1. The browser session exists.
2. The tab lease exists.
3. The lease belongs to the browser session.
4. The lease is active.
5. The Chrome target still exists.
6. Any element reference belongs to the same active lease and document revision.
7. The requested operation's actionability, target-preparation, and artifact-boundary conditions hold.

Chrome target IDs, frame IDs, backend node IDs, snapshot IDs, diagnostic IDs, and artifact IDs do not authorize Chrome actions. The broker resolves them only after validating the session and tab lease.

Release, close, takeover, missing-target recovery, broker restart, and top-level navigation clear element references associated with the affected lease or document.

## Compatibility Floor

The v0.3 surface provides a task-oriented mapping for the default capabilities exposed by these package versions:

- `@playwright/mcp` 0.0.76 core automation and tab management.
- `chrome-devtools-mcp` 1.3.0 input, navigation, emulation, performance, network, and debugging operations enabled by the plugin configuration, including heap capture.

The mapping preserves each browser outcome through a task-oriented operation. Shared operations appear once. Lease tools provide tab selection, and accessibility references provide element identity within an owned document.

The version-pinned [capability matrix](../evidence/00005-agent-interaction-surface/capability-matrix.md) maps all 23 default Playwright MCP operations and all 29 functional Chrome DevTools MCP operations available under the original configuration. Chrome DevTools MCP's feature-gated memory-analysis, screencast, and coordinate-input operations are identified separately; v0.3 includes them as deliberate product capabilities.

Extension management, experimental third-party developer tools, experimental WebMCP, and Playwright opt-in config, routing, storage, DevTools, PDF, vision, and verification packs remain separate capabilities.

## Stable Tool Catalog

The MCP server advertises one stable catalog throughout the connection.

Session and tab ownership tools:

- `start_session`
- `list_tabs`
- `new_tab`
- `claim_tab`
- `release_tab`
- `focus_tab`
- `close_tab`

Explicit page tools:

- `snapshot`
- `navigate`
- `wait_for`
- `click`
- `fill`
- `fill_form`
- `type_text`
- `press_key`
- `screenshot`
- `evaluate`

Domain tools:

- `interact`
- `console`
- `network`
- `emulation`
- `performance`
- `audit`
- `memory`
- `screencast`
- `artifacts`

Guidance tool:

- `help`

The server instructions and skill describe the normal workflow. Tool descriptions state when to use the tool, the preferred neighboring tool, and the recovery path for common failures. Tool annotations identify read-only, destructive, idempotent, and open-world behavior. Each tool publishes an output schema.

## Tool Descriptions and Annotations

The catalog uses these agent-facing descriptions and MCP annotations:

| Tool | Description | `readOnlyHint` | `destructiveHint` | `idempotentHint` | `openWorldHint` |
| --- | --- | --- | --- | --- | --- |
| `start_session` | Start one browser task and optionally create its first owned tab. Use this before every other browser tool. | false | false | false | true |
| `list_tabs` | List this session's tab leases, or inspect the shared read-only target inventory. | true | false | true | true |
| `new_tab` | Create a background tab owned by this session. Request focus only for user handoff to the visible browser. | false | false | false | true |
| `claim_tab` | Claim an unowned target, or perform a user-instructed takeover that returns a new tab lease. | false | true | false | true |
| `release_tab` | End ownership while leaving the Chrome target open and claimable. | false | true | true | true |
| `focus_tab` | Bring an owned tab to the foreground for user handoff or manual inspection. | false | false | true | true |
| `close_tab` | Close an owned Chrome target and its lease. | false | true | true | true |
| `snapshot` | Inspect user-perceivable page structure and obtain element references for later actions. | true | false | false | true |
| `navigate` | Change the owned tab's URL or session history and observe the resulting document. | false | false | false | true |
| `wait_for` | Wait for page text, element state, URL, load state, expression, or a bounded delay. | true | false | true | true |
| `click` | Invoke one referenced or explicitly selected element after target-session attachment and actionability checks. | false | true | false | true |
| `fill` | Replace the value of one editable control without activating Chrome. | false | true | true | true |
| `fill_form` | Apply two or more typed field updates and report partial completion on failure. | false | true | true | true |
| `type_text` | Insert text at an editable target's current selection without activating Chrome. | false | true | false | true |
| `press_key` | Dispatch one browser-protocol key sequence to a resolved element, or to the focused owned document after explicit handoff. | false | true | false | true |
| `screenshot` | Capture visual page state as MCP image content and an owned artifact. | false | false | false | true |
| `evaluate` | Read or modify page state with JavaScript when the semantic tools do not expose it. | false | true | false | true |
| `interact` | Perform a specialized user interaction such as hover, drag, upload, dialog handling, scrolling, or coordinate input. | false | true | false | true |
| `console` | List, inspect, or clear console diagnostics collected for an owned tab. | false | true | true | true |
| `network` | List, inspect, or clear network diagnostics collected for an owned tab. | false | true | true | true |
| `emulation` | Set or reset the owned target's viewport, network, CPU, location, media, user agent, or request headers. | false | false | true | true |
| `performance` | Record, summarize, or analyze performance data from an owned tab. | false | false | false | true |
| `audit` | Run accessibility, SEO, best-practices, and agentic-browsing checks against an owned tab. | false | false | false | true |
| `memory` | Capture and inspect an owned tab's JavaScript heap artifacts. | false | true | false | true |
| `screencast` | Start, stop, or inspect a recording bound to an owned tab. | false | false | false | true |
| `artifacts` | List, inspect, read, export, or delete outputs owned by the browser session. | false | true | false | false |
| `help` | Select a browser tool or domain operation from the task the agent needs to perform. | true | false | true | false |

MCP annotations apply to the complete top-level tool. A domain tool advertises
the conservative value required by any of its operations. Each domain result
also returns the selected operation so callers can interpret its effect.

## Shared Schema Types

```ts
type AgentSessionId = string; // session_<uuid>
type TabId = string;          // tab_<uuid>
type ArtifactId = string;     // artifact_<uuid>
type SnapshotId = string;     // snapshot_<uuid>
type DocumentRevision = string;
type ElementRef = string;     // e_<short opaque id>

type PageScope = {
  agent_session_id: AgentSessionId;
  tab_id: TabId;
};

type ObservationMode = "none" | "diff" | "snapshot";
type Modifier = "alt" | "control" | "meta" | "shift";
type JsonValue = null | boolean | number | string | JsonValue[] | {
  [key: string]: JsonValue;
};

type SnapshotDiff = {
  base_snapshot_id?: SnapshotId;
  snapshot_id: SnapshotId;
  document_revision: DocumentRevision;
  changes: string;
  changed_node_count: number;
  truncated: boolean;
};

type Observation =
  | { mode: "none" }
  | { mode: "diff"; diff: SnapshotDiff }
  | { mode: "snapshot"; snapshot: SnapshotResult };

type PageActionResult = {
  document_revision: DocumentRevision;
  observation: Observation;
};

type ArtifactSummary = {
  artifact_id: ArtifactId;
  kind: "screenshot" | "console" | "network" | "trace" | "audit" |
    "heap_snapshot" | "screencast" | "evaluation";
  media_type: string;
  size_bytes: number;
  sha256: string;
  created_at_ms: number;
  retention: "session";
};
```

## Session and Tab Tool Schemas

The seven ownership tools retain the RFC `00001` contract:

```ts
type OwnedTab = {
  tab_id: TabId;
  target_id: string;
  title: string;
  url: string;
  state: "active" | "missing";
  focused: boolean;
  created_at_ms: number;
  updated_at_ms: number;
};

type GlobalTab = {
  target_id: string;
  title: string;
  url: string;
  owner_display_id?: string;
  owner_label?: string;
  owned_by_caller: boolean;
  caller_tab_id?: TabId;
  claimable: boolean;
  focused: boolean;
};

start_session({
  label?: string,
  start_url?: string,
  focus?: boolean
}) -> {
  agent_session_id: AgentSessionId,
  tab?: OwnedTab
}

list_tabs({
  agent_session_id: AgentSessionId,
  scope?: "owned" | "global_readonly"
}) ->
  | { scope: "owned"; tabs: OwnedTab[] }
  | { scope: "global_readonly"; groups: Array<{
      owner_display_id?: string;
      owner_label?: string;
      tabs: GlobalTab[];
    }> }

new_tab({
  agent_session_id: AgentSessionId,
  url?: string,
  focus?: boolean
}) -> { tab: OwnedTab }

claim_tab({
  agent_session_id: AgentSessionId,
  target_id: string,
  takeover?: boolean,
  user_instruction?: string
}) -> { tab: OwnedTab }

release_tab(PageScope) -> { released: true }
focus_tab(PageScope) -> { tab: OwnedTab }
close_tab(PageScope) -> { closed: true }
```

`claim_tab` requires a non-empty `user_instruction` when `takeover` is true.
Foreign sessions never receive another session's `agent_session_id`, `tab_id`,
or element reference through the read-only inventory.

# Semantic Page Model

## Accessibility Snapshot

`snapshot` returns a compact representation of user-perceivable page structure. Chrome's Accessibility domain supplies computed roles, accessible names, state, hierarchy, and backend DOM node associations. DOM data supplies labels, placeholders, text, alternate text, titles, test IDs, geometry, frame boundaries, and shadow-host relationships where the accessibility data needs additional context.

The snapshot query order follows user-observable semantics:

1. Role and accessible name.
2. Associated label.
3. Placeholder or visible text.
4. Alternate text or title.
5. Test ID.
6. CSS fallback supplied explicitly by the caller.

Input:

```ts
snapshot({
  agent_session_id: string,
  tab_id: string,
  mode?: "interactive" | "meaningful" | "full",
  root?: ElementTarget,
  depth?: number,
  max_nodes?: number,
  include_hidden?: boolean,
  include_bounds?: boolean
}) -> SnapshotResult
```

Defaults are `mode: "meaningful"`, `depth: 8`, `max_nodes: 500`, `include_hidden: false`, and `include_bounds: false`.

Output:

```ts
type SnapshotResult = {
  snapshot_id: SnapshotId;
  document_revision: DocumentRevision;
  url: string;
  title: string;
  tree: string;
  node_count: number;
  truncated: boolean;
};
```

The compact tree labels actionable and meaningful nodes with references such as `e_1`, `e_2`, and `e_a`. The tree includes frame boundaries and preserves accessibility hierarchy without exposing the broker's internal reference map.

## Element References

An element reference records:

- the owning `agent_session_id` and `tab_id`;
- the Chrome target and frame session;
- the document loader or document revision;
- the backend DOM node ID;
- the snapshot generation;
- the semantic role and accessible name used for diagnostics.

References remain valid while the same DOM node belongs to the same document and active lease. A replaced or removed node returns `element_stale` with recovery `snapshot`. Navigation returns a new document revision and invalidates every reference from the prior document.

Frames, including out-of-process iframes, are collected through their CDP sessions and spliced into the snapshot tree. Element references carry frame identity internally, so page actions do not accept global frame handles. Open shadow roots participate through their associated DOM and accessibility nodes.

## Element Target

Element actions use one target union:

```ts
type ElementTarget =
  | { ref: string }
  | { css: string; frame_ref?: string };
```

A reference is the normal action handle. A CSS fallback uses strict matching. Zero matches return `element_not_found`; multiple matches return `element_ambiguous`. `frame_ref` identifies an iframe from the caller's current snapshot. CSS without `frame_ref` resolves in the main frame.

## Actionability

Element operations wait until their required conditions hold or `timeout_ms` expires.

Common conditions:

- attached to the active document;
- visible with a non-empty layout box;
- stable across two animation frames;
- enabled;
- editable for text entry;
- able to receive pointer events for pointer actions.

Pointer actions additionally hit-test the action point and reject an obscured target with `element_not_actionable`. Operations that imply one target use strict resolution.

Element actions preserve the user's active application by default. For pointer and keyboard operations with an element target, the broker attaches to the owned target, prepares the resolved element inside the document when the operation needs editable or keyboard state, and dispatches browser input through CDP after actionability succeeds. Target activation, including CDP `Target.activateTarget`, is reserved for `focus_tab` and `focus: true` tab creation when the user asks to bring managed Chrome forward for manual inspection or handoff.

Targetless raw input is a focused-document operation. `press_key` without `target` and `interact` `click_at` return `focus_required` until `focus_tab` has focused the owned document. They do not report success for background raw CDP input delivery.

The RFC `00004` installed runtime shipped with a focused-document recovery path for `click` and `press_key`. The non-intrusive interaction recon under this RFC supersedes that design interpretation: OS foreground application focus is not required for normal headed-browser page actions when the browser automation path uses target-session attachment, element preparation, actionability, and CDP input inside Chrome.

## Post-Action Observation

Mutating page actions accept:

```ts
observe?: "none" | "diff" | "snapshot"
```

The default is `diff`. The response includes a compact accessibility change set relative to the most recent snapshot for the lease. `snapshot` returns the full default snapshot. `none` returns operation results without page structure.

Navigation defaults to `observe: "snapshot"` because it establishes a new document.

# Explicit Page Tools

## Navigation

```ts
navigate({
  agent_session_id: string,
  tab_id: string,
  action: "url" | "back" | "forward" | "reload",
  url?: string,
  wait_until?: "none" | "dom_content_loaded" | "load" | "network_idle",
  timeout_ms?: number,
  ignore_cache?: boolean,
  before_unload?: "accept" | "dismiss",
  init_script?: string,
  observe?: ObservationMode
}) -> PageActionResult
```

`url` is required only for `action: "url"`. `ignore_cache` applies only to reload. `init_script` runs before page scripts in the next document and is removed after that navigation. Navigation never activates Chrome.

## Waits

```ts
wait_for({
  agent_session_id: string,
  tab_id: string,
  condition:
    | { kind: "delay"; duration_ms: number }
    | { kind: "text"; text: string; state?: "visible" | "hidden" }
    | { kind: "element"; target: ElementTarget; state: "attached" | "detached" | "visible" | "hidden" | "enabled" | "disabled" | "editable" | "checked" | "unchecked" }
    | { kind: "url"; value: string; match?: "exact" | "substring" | "regex" }
    | { kind: "load"; state: "dom_content_loaded" | "load" | "network_idle" }
    | { kind: "expression"; expression: string }
  timeout_ms?: number,
  observe?: ObservationMode
}) -> WaitResult
```

Waits poll or subscribe without activating Chrome. A timeout returns `operation_timeout` and names the unmet condition.

## Pointer and Form Actions

```ts
click({
  agent_session_id: string,
  tab_id: string,
  target: ElementTarget,
  button?: "left" | "middle" | "right",
  count?: 1 | 2,
  modifiers?: Modifier[],
  timeout_ms?: number,
  observe?: ObservationMode
}) -> PageActionResult
```

```ts
fill({
  agent_session_id: string,
  tab_id: string,
  target: ElementTarget,
  value: string,
  timeout_ms?: number,
  observe?: ObservationMode
}) -> PageActionResult
```

`fill` replaces the value of one editable control and dispatches the corresponding input and change events. It is the normal tool for one text field.

```ts
fill_form({
  agent_session_id: string,
  tab_id: string,
  fields: Array<
    | { target: ElementTarget; kind: "text"; value: string }
    | { target: ElementTarget; kind: "select"; values: string[] }
    | { target: ElementTarget; kind: "checked"; checked: boolean }
  >,
  timeout_ms?: number,
  observe?: ObservationMode
}) -> FormResult
```

`fill_form` is the normal tool for two or more controls. Fields run sequentially and stop at the first failure. The result records every completed field so callers can recover without repeating successful updates.

## Text and Keyboard

```ts
type_text({
  agent_session_id: string,
  tab_id: string,
  target: ElementTarget,
  text: string,
  delay_ms?: number,
  timeout_ms?: number,
  observe?: ObservationMode
}) -> PageActionResult
```

`type_text` focuses the target in its document and inserts text at its current selection. It preserves application focus. Use it for contenteditable controls and insertion semantics; use `fill` for ordinary field replacement.

```ts
press_key({
  agent_session_id: string,
  tab_id: string,
  key: string,
  target?: ElementTarget,
  modifiers?: Modifier[],
  timeout_ms?: number,
  observe?: ObservationMode
}) -> PageActionResult
```

`press_key` with `target` dispatches browser-protocol keyboard input after preparing the resolved element. Without `target`, it dispatches to the focused owned document and returns `focus_required` until the caller has used `focus_tab`.

## Screenshot

```ts
screenshot({
  agent_session_id: string,
  tab_id: string,
  target?: ElementTarget,
  full_page?: boolean,
  format?: "png" | "jpeg" | "webp",
  quality?: number
}) -> ScreenshotResult
```

The MCP response contains renderable image content plus artifact metadata. Element screenshots and full-page screenshots are mutually exclusive.

```ts
type ScreenshotResult = {
  artifact: ArtifactSummary;
  image: { media_type: "image/png" | "image/jpeg" | "image/webp" };
  width: number;
  height: number;
};
```

## Evaluation

```ts
evaluate({
  agent_session_id: string,
  tab_id: string,
  source: string,
  mode?: "expression" | "function",
  args?: JsonValue[],
  target?: ElementTarget,
  await_promise?: boolean
}) -> EvaluationResult
```

Evaluation is the escape hatch for page state unavailable through snapshots and diagnostics. Ordinary page interaction uses the named action tools.

```ts
type EvaluationResult = {
  value?: JsonValue;
  preview?: string;
  artifact?: ArtifactSummary;
};

type WaitResult = {
  matched: true;
  elapsed_ms: number;
  document_revision: DocumentRevision;
  observation: Observation;
};

type FormResult = {
  completed_fields: number;
  total_fields: number;
  document_revision: DocumentRevision;
  observation: Observation;
};
```

# Domain Tools

## Interact

`interact` contains specialized user operations selected by an `operation` discriminator:

- `select_options`
- `set_checked`
- `hover`
- `drag`
- `drop`
- `upload_files`
- `handle_dialog`
- `scroll`
- `click_at`

Element operations use `ElementTarget`. Upload paths resolve inside the active workspace supplied by the MCP host. Dialog handling accepts `accept` or `dismiss` and optional prompt text. Referenced pointer actions use target-session attachment, hit-test, and actionability. Coordinate `click_at` is targetless raw input and returns `focus_required` until `focus_tab` has focused the owned document.

## Console

`console` operations are `list`, `get`, and `clear`. List filters by sequence, level, and limit. Get returns one message with arguments, source location, and stack information. Diagnostic IDs are scoped to the active lease and reset at lease boundaries.

## Network

`network` operations are `list`, `get`, and `clear`. List filters by sequence, URL, resource type, status, and limit. Get returns bounded request headers, request body, response headers, response body, timing, initiator, and failure information. Oversized bodies become artifacts.

## Emulation

`emulation` operations are:

- `set_viewport`
- `set_network`
- `set_cpu`
- `set_geolocation`
- `set_media`
- `set_user_agent`
- `set_headers`
- `reset`

Emulation state belongs to the owned target. Responses state the effective values.

## Performance

`performance` operations are `start_trace`, `stop_trace`, `vitals`, and `analyze`. Trace capture occurs through the owned target's CDP session. Trace data becomes an artifact before analysis.

The performance analyzer runs inside `visible-browser-lab-mcp`. It reads a completed trace artifact and bounded analysis parameters without receiving the Chrome endpoint, target ID, session bearer, tab bearer, or browser profile path. It produces deterministic findings for long tasks, script execution, style and layout, paint, network activity, and dominant trace slices on all six release targets. The [analyzer feasibility record](../evidence/00005-agent-interaction-surface/analyzer-feasibility.md) defines the target evidence and output boundary.

## Audit

`audit` uses the `run` operation to execute named accessibility, best-practices, SEO, and agentic-browsing checks against an owned tab. It accepts desktop or mobile presentation and navigation or snapshot mode. Results contain category scores, findings, affected element references where available, and concrete remediation text.

## Memory

`memory` operations are `capture`, `summary`, `classes`, `node`, `dominators`, `retainers`, `retaining_paths`, `edges`, and `close`. Heap snapshots are artifacts bound to the owning browser session and tab. Analysis responses are paginated and bounded.

## Screencast

`screencast` operations are `start`, `stop`, and `status`. Recording remains bound to the owned target. Stop returns a silent AV1-in-WebM video artifact. The encoder runs inside `visible-browser-lab-mcp`; installed packages do not require a media runtime. Frame rate defaults to 10 and is capped at 30. Maximum duration defaults to 30 seconds and is capped at 5 minutes.

## Artifacts

`artifacts` operations are `list`, `metadata`, `read`, `export`, and `delete`. Artifacts carry owner session, originating tab, kind, media type, size, checksum, creation time, and `session` retention. A broker generation removes artifacts whose owning sessions can no longer be reached.

Inline reads are bounded. Export paths are relative to the active workspace root supplied by the MCP host. The broker rejects paths that escape that root.

## Domain Input Schemas

Each domain tool accepts `PageScope` plus one tagged operation. `artifacts` uses
`agent_session_id` and verifies artifact ownership directly.

```ts
type InteractInput = PageScope & (
  | { operation: "select_options"; target: ElementTarget; values: string[];
      timeout_ms?: number; observe?: ObservationMode }
  | { operation: "set_checked"; target: ElementTarget; checked: boolean;
      timeout_ms?: number; observe?: ObservationMode }
  | { operation: "hover"; target: ElementTarget; timeout_ms?: number;
      observe?: ObservationMode }
  | { operation: "drag"; source: ElementTarget; destination: ElementTarget;
      timeout_ms?: number; observe?: ObservationMode }
  | { operation: "drop"; target: ElementTarget; paths?: string[];
      data?: Record<string, string>; timeout_ms?: number;
      observe?: ObservationMode }
  | { operation: "upload_files"; target: ElementTarget; paths: string[];
      timeout_ms?: number; observe?: ObservationMode }
  | { operation: "handle_dialog"; action: "accept" | "dismiss";
      prompt_text?: string; observe?: ObservationMode }
  | { operation: "scroll"; target?: ElementTarget; delta_x?: number;
      delta_y: number; observe?: ObservationMode }
  | { operation: "click_at"; x: number; y: number;
      button?: "left" | "middle" | "right"; count?: 1 | 2;
      modifiers?: Modifier[]; observe?: ObservationMode }
);

type ConsoleInput = PageScope & (
  | { operation: "list"; since?: number;
      levels?: Array<"verbose" | "debug" | "info" | "warning" | "error">;
      limit?: number }
  | { operation: "get"; message_id: string }
  | { operation: "clear" }
);

type NetworkInput = PageScope & (
  | { operation: "list"; since?: number; url_pattern?: string;
      resource_types?: string[]; status_min?: number; status_max?: number;
      include_static?: boolean; limit?: number }
  | { operation: "get"; request_id: string;
      include_request_body?: boolean; include_response_body?: boolean;
      body_limit_bytes?: number }
  | { operation: "clear" }
);

type EmulationInput = PageScope & (
  | { operation: "set_viewport"; width: number; height: number;
      device_scale_factor?: number; mobile?: boolean; touch?: boolean;
      orientation?: "portrait" | "landscape" }
  | { operation: "set_network";
      preset?: "offline" | "slow_3g" | "fast_3g" | "slow_4g" | "none";
      offline?: boolean; latency_ms?: number;
      download_bytes_per_second?: number; upload_bytes_per_second?: number }
  | { operation: "set_cpu"; slowdown: number }
  | { operation: "set_geolocation"; latitude: number; longitude: number;
      accuracy_meters?: number }
  | { operation: "set_media"; media?: "screen" | "print";
      color_scheme?: "light" | "dark" | "no_preference";
      reduced_motion?: "reduce" | "no_preference" }
  | { operation: "set_user_agent"; user_agent: string;
      platform?: string; accept_language?: string }
  | { operation: "set_headers"; headers: Record<string, string> }
  | { operation: "reset" }
);

type PerformanceInput = PageScope & (
  | { operation: "start_trace"; reload?: boolean; screenshots?: boolean;
      categories?: string[] }
  | { operation: "stop_trace" }
  | { operation: "vitals"; since_navigation?: boolean }
  | { operation: "analyze"; artifact_id: ArtifactId;
      insight?: string; max_findings?: number }
);

type AuditInput = PageScope & {
  operation: "run";
  categories?: Array<"accessibility" | "seo" | "best_practices" |
    "agentic_browsing">;
  mode?: "navigation" | "snapshot";
  device?: "desktop" | "mobile";
};

type MemoryInput = PageScope & (
  | { operation: "capture" }
  | { operation: "summary"; artifact_id: ArtifactId }
  | { operation: "classes"; artifact_id: ArtifactId; class_name?: string;
      min_retained_bytes?: number; cursor?: string; limit?: number }
  | { operation: "node"; artifact_id: ArtifactId; node_id: string }
  | { operation: "dominators"; artifact_id: ArtifactId; node_id?: string;
      cursor?: string; limit?: number }
  | { operation: "retainers"; artifact_id: ArtifactId; node_id: string;
      cursor?: string; limit?: number }
  | { operation: "retaining_paths"; artifact_id: ArtifactId;
      node_id: string; max_depth?: number; limit?: number }
  | { operation: "edges"; artifact_id: ArtifactId; node_id: string;
      direction?: "incoming" | "outgoing"; cursor?: string; limit?: number }
  | { operation: "close"; artifact_id: ArtifactId }
);

type ScreencastInput = PageScope & (
  | { operation: "start"; fps?: number; quality?: number;
      max_duration_ms?: number }
  | { operation: "stop" }
  | { operation: "status" }
);

type ArtifactsInput = { agent_session_id: AgentSessionId } & (
  | { operation: "list"; tab_id?: TabId; kinds?: ArtifactSummary["kind"][];
      cursor?: string; limit?: number }
  | { operation: "metadata"; artifact_id: ArtifactId }
  | { operation: "read"; artifact_id: ArtifactId; offset?: number;
      length?: number }
  | { operation: "export"; artifact_id: ArtifactId; path: string;
      overwrite?: boolean }
  | { operation: "delete"; artifact_id: ArtifactId }
);
```

`drop` requires at least one non-empty `paths` or `data` member. `scroll`
requires a non-zero delta. `set_network` accepts either one preset or explicit
network values. Empty headers reset the request-header override. Pagination
limits default to 100 and are capped at 1,000; inline diagnostic and artifact
reads default to 256 KiB and are capped at 1 MiB.

## Domain Output Schemas

```ts
type InteractResult = PageActionResult & { operation: InteractInput["operation"] };

type ConsoleMessage = {
  message_id: string;
  sequence: number;
  level: "verbose" | "debug" | "info" | "warning" | "error";
  text: string;
  timestamp_ms?: number;
  source?: { url?: string; line?: number; column?: number };
  stack?: string[];
  arguments?: JsonValue[];
};

type ConsoleResult =
  | { operation: "list"; messages: ConsoleMessage[]; next_since: number;
      truncated: boolean; artifact?: ArtifactSummary }
  | { operation: "get"; message: ConsoleMessage }
  | { operation: "clear"; cleared: true };

type NetworkRequest = {
  request_id: string;
  sequence: number;
  url: string;
  method: string;
  resource_type?: string;
  status?: number;
  mime_type?: string;
  failed?: boolean;
  error_text?: string;
  started_at_ms?: number;
  duration_ms?: number;
};

type NetworkResult =
  | { operation: "list"; requests: NetworkRequest[]; next_since: number;
      truncated: boolean; artifact?: ArtifactSummary }
  | { operation: "get"; request: NetworkRequest;
      request_headers: Record<string, string>;
      response_headers?: Record<string, string>;
      request_body?: string; response_body?: string;
      body_artifact?: ArtifactSummary; timing?: Record<string, number>;
      initiator?: JsonValue }
  | { operation: "clear"; cleared: true };

type EmulationResult = {
  operation: EmulationInput["operation"];
  effective: JsonValue;
};

type PerformanceResult =
  | { operation: "start_trace"; recording: true }
  | { operation: "stop_trace"; recording: false; artifact: ArtifactSummary }
  | { operation: "vitals"; metrics: Record<string, number | null> }
  | { operation: "analyze"; artifact: ArtifactSummary;
      findings: Array<{ name: string; severity: "info" | "warning" | "error";
        summary: string; evidence?: JsonValue }> };

type AuditResult = {
  operation: "run";
  scores: Record<string, number | null>;
  findings: Array<{ id: string; category: string; title: string;
    description: string; refs?: ElementRef[] }>;
  reports: ArtifactSummary[];
};

type MemoryResult =
  | { operation: "capture"; artifact: ArtifactSummary }
  | { operation: "close"; closed: true }
  | { operation: Exclude<MemoryInput["operation"], "capture" | "close">;
      artifact: ArtifactSummary; data: JsonValue;
      next_cursor?: string; truncated: boolean };

type ScreencastResult =
  | { operation: "start"; recording: true; started_at_ms: number }
  | { operation: "stop"; recording: false; artifact: ArtifactSummary }
  | { operation: "status"; recording: boolean; started_at_ms?: number };

type ArtifactsResult =
  | { operation: "list"; artifacts: ArtifactSummary[]; next_cursor?: string }
  | { operation: "metadata"; artifact: ArtifactSummary }
  | { operation: "read"; artifact: ArtifactSummary; offset: number;
      data_base64: string; eof: boolean }
  | { operation: "export"; artifact: ArtifactSummary; path: string }
  | { operation: "delete"; deleted: true };
```

# Guidance Contract

`help` accepts a topic and optional operation:

```ts
help({
  topic: "workflow" | "tabs" | "snapshot" | "interaction" | "navigation" | "diagnostics" | "emulation" | "performance" | "audit" | "memory" | "screencast" | "artifacts" | "errors",
  operation?: string
}) -> HelpResult
```

Help results contain:

- the task the topic solves;
- the preferred tool and operation;
- neighboring tools and when they apply;
- a minimal valid call;
- the result schema;
- common error codes and recovery actions.

```ts
type HelpResult = {
  topic: string;
  operation?: string;
  task: string;
  preferred: { tool: string; operation?: string; reason: string };
  neighbors: Array<{ tool: string; operation?: string; use_when: string }>;
  example: { tool: string; arguments: JsonValue };
  result_schema: JsonValue;
  errors: Array<{ code: string; recovery: string }>;
};
```

## Server Instructions

The MCP server publishes these instructions with the catalog:

> Start each browser task with `start_session` and retain its
> `agent_session_id`. Use only `tab_id` values owned by that session. Inspect an
> unfamiliar page with `snapshot`, then act through its element references.
> Use `fill` for one field, `fill_form` for a form, `wait_for` for asynchronous
> state, and `screenshot` for visual appearance. Use `console` and `network`
> for runtime diagnosis. Use `help` to select an operation in a specialized
> domain. `click`, targeted `press_key`, and referenced pointer operations
> attach to the owned target, prepare the resolved element, run actionability
> checks, and preserve the user's active application during normal browser work.
> Targetless `press_key` and `interact` `click_at` require `focus_tab` first.
> Target activation, including CDP `Target.activateTarget`, is reserved for
> `focus_tab` and `focus: true` tab creation when the user asks to bring managed
> Chrome forward for handoff or manual inspection. CSS and `evaluate` are
> explicit escape hatches for page state the semantic tools do not expose.

These instructions establish the decision path without duplicating individual
input schemas.

## Skill Decision Path

The installed skill teaches one ordered workflow:

1. Call `start_session`; retain the session bearer for the complete browser task.
2. Reuse an owned tab from `list_tabs`, create one with `new_tab`, or claim an unowned target after inspecting `global_readonly` inventory.
3. Call `snapshot` before interacting with an unfamiliar document or after `element_stale`.
4. Use the shortest named semantic action that expresses the task: `click`, `fill`, `fill_form`, `type_text`, `press_key`, or `wait_for`.
5. Call `focus_tab` only when the user asks to bring Chrome forward for handoff or manual inspection.
6. Read the action's accessibility diff; request a full snapshot when the new document structure matters.
7. Use `console` and `network` when page behavior differs from the requested result.
8. Use `performance`, `audit`, `memory`, `screencast`, or `artifacts` for the corresponding investigation output.
9. Use `evaluate` or a CSS target when the semantic page model cannot represent the required state or element.
10. Release a tab that another agent may continue, or close a tab whose browser work is complete.

Domain tool descriptions enumerate their operations. An invalid operation returns `unsupported_operation` with recovery `help`.

# Errors and Recovery

The browser error contract adds:

- `element_not_found` -> `snapshot`
- `element_ambiguous` -> `snapshot`
- `element_stale` -> `snapshot`
- `element_not_actionable` -> `wait_for`
- `dialog_not_open` -> `help`
- `artifact_not_found` -> `artifacts`
- `unsupported_operation` -> `help`
- `analysis_unavailable` -> `help`

Every structured error includes a stable code, a concrete message, and one recovery operation. Ownership errors continue to return before element, diagnostic, artifact, or analyzer resolution.

# Implementation Shape

Chromiumoxide remains the broker's CDP client. The semantic page layer uses typed Accessibility, DOM, Runtime, Input, Page, Network, Log, Emulation, Tracing, HeapProfiler, and Target commands and events.

The broker owns:

- accessibility snapshot formatting;
- element reference registries;
- actionability polling;
- post-action snapshot diffs;
- diagnostic and artifact registries;
- workspace path validation;
- trace aggregation and deterministic performance findings;
- accessibility, SEO, best-practices, and agentic-browsing audits;
- V8 heap-snapshot parsing and graph analysis;
- AV1 encoding and WebM muxing;
- translation into stable MCP outputs.

The performance analyzer, heap parser, audit engine, and screencast encoder run
inside `visible-browser-lab-mcp`. They consume broker-owned data after session
and tab validation and return bounded structured results plus session-owned
artifacts. Each release package contains one executable and requires no
language, media, or trace-analysis runtime on the installed machine.

# Drawbacks

The stable catalog presents 27 tools. Explicit page tools and task-named domains keep the common path visible while specialized schemas remain behind operation discriminators and on-demand help.

Accessibility references depend on the live document. Dynamic pages can replace nodes between observation and action. The broker reports a stale reference and makes a fresh snapshot the recovery path.

Grouped domain tools place operation validation in the broker. Tagged operation schemas, output schemas, help results, and deterministic contract tests keep those operations discoverable.

Full accessibility snapshots and browser artifacts can be large. Snapshot modes, depth and node limits, compact diffs, bounded diagnostic results, pagination, and artifact handles control context growth.

# Alternatives

A one-tool-per-operation catalog makes every capability visible in the initial MCP schema. It also repeats session and tab parameters across a large tool list and increases the selection space for ordinary tasks.

A small set of generic tools minimizes tool count but moves too much meaning into untyped operation payloads. The hybrid catalog keeps normal actions explicit and gives each specialized problem a named domain.

A DOM-derived semantic tree can run as injected JavaScript. Chrome's Accessibility domain supplies computed browser semantics and backend node associations directly, so the broker uses it as the primary representation and enriches it with DOM data.

Bundled Playwright and Chrome DevTools MCP processes would preserve their internal behavior. A Chromiumoxide core keeps ownership validation, target selection, focus policy, runtime packaging, errors, outputs, analysis, and artifact generation inside one broker contract.

# Stage 2 Criteria

Stage 2 promotion requires:

- The version-pinned capability matrix maps every default operation from `@playwright/mcp` 0.0.76 and `chrome-devtools-mcp` 1.3.0 to an explicit tool, domain operation, lease operation, or named semantic equivalent.
- Exact MCP input and output schemas exist for the stable catalog.
- The server instructions, skill, tool descriptions, help topics, annotations, and output schemas describe one decision path.
- A production-path prototype returns compact AX snapshots, resolves lease-scoped references across main frames and iframes, performs ref-based click and fill, reports stale references, and returns post-action observations.
- At least 30 isolated Codex trials cover page discovery, forms, waits, history, frames, dialogs and files, console and network diagnosis, performance, emulation, ownership refusal, and specialized-domain discovery.
- At least 90 percent of trials complete the browser task.
- At least 85 percent select the correct first relevant explicit tool or domain.
- Semantic tasks do not use CSS or `evaluate` unless the task requests that path.
- No trial acts on an unowned tab.
- The hybrid catalog's serialized schema token count is at most 60 percent of a one-tool-per-operation catalog covering the same capability matrix.
- The in-process performance analyzer compiles for all six release targets and preserves one public output contract.
- Unit, fake-CDP, headless real-browser, visible macOS, Windows compile, package validation, and deterministic catalog checks pass.

The Stage 2 evidence consists of:

- a capability matrix covering 23 default Playwright MCP operations and 29 default Chrome DevTools MCP operations;
- one shared 27-tool contract and 63-tool comparison catalog consumed by the production MCP server and evaluation server;
- an `o200k_base` catalog measurement of 15,668 tokens for the hybrid catalog and 27,193 tokens for the comparison catalog, a ratio of 57.62 percent;
- 29 successful tasks and 29 correct first selections across 30 isolated GPT-5.5 medium-reasoning trials, with zero semantic fallback violations and zero foreign-tab actions;
- real-browser tests for the ownership boundary, accessibility references, actionability, all 45 domain operations, artifact containment, trace and heap analysis, and silent AV1-in-WebM capture;
- strict workspace Clippy, workspace tests, Windows ARM64 compilation, release-input validation, and deterministic catalog validation.
