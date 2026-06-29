<!-- exo:1 ulid:01kvrab2dbv0yn0pbwcsqmthrj -->

# RFC 00001: Visible Browser Lab Tab Isolation

## Summary

Visible Browser Lab gives agents a shared Chrome profile in a visible browser window the user can watch. This RFC defines the MCP facade that keeps that shared browser usable while giving each agent a broker-enforced tab ownership boundary.

The `visible-browser-lab` MCP facade issues opaque `agent_session_id` and `tab_id` bearer identifiers. The broker validates ownership before Chrome actions. The default tab list returns the caller's owned tab leases. The `global_readonly` scope returns a read-only target inventory grouped by `owner_display_id`; it includes caller-owned action handles and represents other sessions' tabs as target metadata.

The boundary is tab-level isolation inside one visible Chrome profile. Cookies, local storage, extension state, browser settings, service workers, and the Chrome process remain shared.

## System Contract

### Chrome Target Boundary

Visible Chrome exposes one target namespace through the Chrome DevTools Protocol. Direct CDP-facing tools operate over that namespace: a caller that can address a page target can focus, navigate, inspect, or close it. The facade defined here places a broker-controlled ownership check between MCP callers and Chrome page targets.

### Facade Contract

The plugin-facing MCP configuration exposes one server, `visible-browser-lab`. The facade starts or reuses one local broker process. The broker owns the in-memory session and tab lease registry, translates owned browser actions to Chrome DevTools Protocol calls, and validates tab ownership before each supported Chrome action.

Broker-backed MCP tools:

- `start_session`
- `list_tabs`
- `new_tab`
- `claim_tab`
- `release_tab`
- `focus_tab`
- `navigate`
- `screenshot`
- `close_tab`
- `evaluate`
- `click`
- `type_text`
- `press_key`
- `console_messages`
- `network_events`

The `cargo xtask live-smoke --cdp-endpoint <url>` smoke test launches `visible-browser-lab-mcp` over stdio and drives it against a supplied visible Chrome CDP endpoint. It checks session creation, owned default listings, read-only target inventory, new tab creation, navigation, PNG screenshot capture, page evaluation, CSS selector click, text input, key press, console diagnostics, network diagnostics, ownership errors, `target_owned` for owned-target claim attempts, release and claim transfer, close, and missing-target recovery after an external Chrome tab close. RFC 00004 defines the managed browser lifecycle and installed-package validation path.

### Stage 3 Criteria

Stage 3 promotion requires the RFC, plugin configuration, broker implementation, skill text, `cargo xtask live-smoke`, and `cargo test --test headless_mcp` checks to describe the same tool contract.

The Stage 3 tool contract includes the session and tab lifecycle tools, owned-tab navigation and screenshot tools, owned-tab page actions, and owned-tab diagnostics. The Stage 3 validation set checks session creation, owned default listings, read-only target inventory, new tab creation, navigation, screenshots, `evaluate`, `click`, `type_text`, `press_key`, `console_messages`, `network_events`, ownership refusal for every owned-tab action class, release and claim transfer, explicit takeover, close, and missing-target recovery.

## Isolation Model

The isolation model is broker-enforced tab ownership for same-user agents that use the facade MCP contract. It prevents accidental cross-tab actions through the exposed tools. The boundary is a local capability boundary: `agent_session_id` and `tab_id` are bearer values, so callers keep them private and use the facade for Chrome actions.

`agent_session_id` and `tab_id` are unguessable bearer capabilities issued by the broker. They are generated independently of MCP transport identity from at least 128 bits of cryptographically secure randomness and encoded as opaque strings. The broker accepts a valid bearer capability from any MCP facade process connected to the same local broker.

Bearer values appear in session responses and caller-owned action handles. Read-only target inventory, logs intended for normal agent consumption, and owner labels use non-authorizing display identifiers so another agent can identify a tab for explicit user-directed transfer while receiving only display metadata.

Chrome `targetId` is target metadata and claim input. The action capability is the broker-issued `tab_id` paired with the caller's `agent_session_id`.

## Terminology

A **browser session** is a broker record for one agent workflow. It has an opaque `agent_session_id`, a non-authorizing `owner_display_id`, an optional label, and timestamps.

A **tab lease** is the broker's ownership record for one Chrome page target. It binds an opaque `tab_id` to a Chrome `targetId`, an owning browser session, the last observed title and URL, timestamps, and a lease state.

An **owned-tab action tool** is an MCP tool that requires both `agent_session_id` and `tab_id`, then asks the broker to verify that the tab is active and owned by that session before touching Chrome.

The **read-only target inventory** is the `global_readonly` tab listing that shows visible Chrome page targets grouped by owner display ID. It includes action handles only for caller-owned tabs and represents other sessions' tabs as target metadata.

An **owner display ID** is non-authorizing display metadata used for owner grouping in read-only target inventory. Owned listing and owned-tab action tools accept `agent_session_id` and `tab_id`.

## MCP Surface

Expose one MCP server named `visible-browser-lab` from every plugin-facing MCP surface:

```json
{
  "mcpServers": {
    "visible-browser-lab": {
      "command": "./bin/visible-browser-lab-mcp",
      "args": [],
      "cwd": ".",
      "env_vars": [
        "VISIBLE_BROWSER_LAB_STATE_DIR",
        "VISIBLE_BROWSER_LAB_CHROME_PATH",
        "VISIBLE_BROWSER_CDP_ENDPOINT",
        "VISIBLE_BROWSER_CDP_PORT"
      ]
    }
  }
}
```

Installed packages contain one target-specific binary and resolve it from the plugin root. The source checkout uses `scripts/visible-browser-lab-mcp.sh` with the same facade entry and runtime environment overrides.

The MCP server is a Rust facade over the visible Chrome CDP endpoint. It provides a session-scoped tab API. The facade owns the mapping between browser sessions, leased tabs, and Chrome `targetId`s. Browser tools validate ownership before touching a Chrome target.

The broker and tab actions use direct CDP. The facade exposes broker-checked tool calls so every Chrome action receives ownership validation.

## Build Shape

The plugin repo contains a root Rust package:

```text
Cargo.toml
src/main.rs
src/mcp.rs
src/broker.rs
src/cdp.rs
src/leases.rs
scripts/visible-browser-lab-mcp.sh
```

The Rust package builds a binary named `visible-browser-lab-mcp`.

Use these build choices:

- MCP server: `rmcp` over stdio.
- Async runtime: `tokio`.
- CDP client: `chromiumoxide` with a broker-owned browser connection and driven handler.
- ID generation: `rand` or `uuid` with at least 128 bits of CSPRNG entropy.
- Internal broker protocol: newline-delimited JSON request/response over local IPC using a small cross-platform abstraction. Unix platforms use local sockets. Windows uses the corresponding Windows local IPC transport. The wire format remains the same across platforms.

Use direct CDP for broker and tab actions. Browser automation dependencies that select global pages belong to separate tool surfaces.

## Runtime Shape

The binary has two modes:

```text
visible-browser-lab-mcp [--cdp-endpoint URL] [--state-dir PATH]
visible-browser-lab-mcp broker --socket PATH [--cdp-endpoint URL] --state-dir PATH
```

The default mode is the MCP stdio facade. It ensures a broker is running, connects to the broker socket, and translates MCP tool calls into broker requests.

The broker mode owns browser selection, CDP connections, and lease state. It is a detached child process started by the first MCP facade process when no broker is listening.

State paths:

```text
state_dir = $VISIBLE_BROWSER_LAB_STATE_DIR or the platform cache directory for visible-browser-lab
endpoint = a local IPC endpoint derived from $state_dir
lock = $state_dir/broker.lock
pid = $state_dir/broker.pid
```

Broker startup rules:

1. The facade tries to connect to the derived broker IPC endpoint.
2. If connect succeeds, it uses the existing broker.
3. If connect fails, it takes `broker.lock`.
4. After taking the lock, it retries the broker IPC endpoint in case another process won the race.
5. If the endpoint is still unavailable, it removes stale local IPC state only when no live `broker.pid` process exists.
6. It starts `visible-browser-lab-mcp broker ...` as a detached child with stdio closed or redirected to broker log files under `state_dir/logs/`.
7. It waits for the broker IPC endpoint to accept connections before serving MCP calls.

The broker keeps lease state in memory. After broker restart, agents call `start_session` again and claim or create tabs again. Chrome tabs and browser profile state survive because Chrome owns them.

## Browser Runtime Selection

An explicit `--cdp-endpoint`, `VISIBLE_BROWSER_CDP_ENDPOINT`, or `VISIBLE_BROWSER_CDP_PORT` selects external CDP mode. The broker validates and uses the supplied endpoint while the caller retains browser lifecycle ownership.

When external CDP configuration is absent, the broker selects managed mode. It discovers an installed Chromium-family browser, starts or reuses the persistent Visible Browser Lab profile with a dynamically assigned debugging port, reads `DevToolsActivePort`, and connects through the resolved endpoint. RFC 00004 defines browser discovery, platform launch behavior, runtime directories, focus behavior, and installed-package validation.

Both modes validate `/json/version` and establish a Chromiumoxide browser connection before serving owned-tab operations. Broker status reports the selected runtime mode and resolved endpoint.

## State Model

The broker maintains browser sessions and tab leases.

```ts
type BrowserSession = {
  agent_session_id: string;
  label?: string;
  created_at: string;
  updated_at: string;
};

type TabLease = {
  tab_id: string;
  target_id: string;
  owner_session_id: string;
  created_at: string;
  updated_at: string;
  state: "active" | "missing" | "released" | "closed";
};
```

State transitions:

- `new_tab` and successful `claim_tab` create an `active` lease.
- External Chrome tab close changes an `active` lease to `missing` when the broker next observes the missing target.
- `release_tab` changes an `active` or `missing` lease to `released` and leaves the Chrome target open if it still exists. A released target is unowned and claimable.
- `close_tab` closes the Chrome target when it exists and changes the lease to `closed`.
- Successful takeover atomically removes the prior active lease and creates a new active lease with a new `tab_id` for the takeover session. The prior owner's old `tab_id` becomes invalid and must fail later actions with `unknown_tab`.
- `released` and `closed` leases are omitted from default owned listings.
- `missing` leases remain in owned listings until released, so the agent can see why its tab handle stopped working.

Browser sessions are in-memory records with the same lifetime as the broker process.

## Response Types

Owned tab summaries include an action handle:

```ts
type OwnedTabSummary = {
  tab_id: string;
  target_id: string;
  title: string;
  url: string;
  state: "active" | "missing";
  focused: boolean;
  created_at: string;
  updated_at: string;
};
```

Global readonly summaries include action handles only for caller-owned tabs:

```ts
type GlobalTabSummary = {
  target_id: string;
  title: string;
  url: string;
  owner_display_id?: string;
  owner_label?: string;
  owned_by_caller: boolean;
  caller_tab_id?: string;
  claimable: boolean;
  focused: boolean;
};

type GlobalTabGroup = {
  owner_display_id?: string;
  owner_label?: string;
  tabs: GlobalTabSummary[];
};
```

For caller-owned tabs, `caller_tab_id` may be present in read-only target inventory results. For tabs owned by another session, `caller_tab_id` must be absent.

`owner_display_id` is stable, non-authorizing display metadata generated for owner grouping. Owned listing and owned-tab action tools accept `agent_session_id` and `tab_id`.

Structured tool errors use this payload shape, either as MCP tool errors or as an error object in the broker protocol:

```ts
type BrowserToolError = {
  code:
    | "chrome_unavailable"
    | "unknown_session"
    | "unknown_tab"
    | "tab_not_owned"
    | "tab_not_active"
    | "target_missing"
    | "target_owned"
    | "invalid_input"
    | "operation_timeout";
  message: string;
  recovery?: "start_session" | "list_tabs" | "new_tab" | "claim_tab" | "release_tab" | "start_chrome";
};
```

Error messages name the failed condition and the next safe action.

## Tool Contract

Every mutating or inspecting browser action requires both `agent_session_id` and an owned `tab_id`. Active tab, tab index, title, URL, and direct `target_id` inputs are lookup metadata. Action handles are `agent_session_id` and owned `tab_id`.

### `start_session`

```ts
start_session({
  label?: string,
  start_url?: string,
  focus?: boolean
}) -> {
  agent_session_id: string,
  tab?: OwnedTabSummary
}
```

Creates a browser session. If `start_url` is present, the broker creates a new Chrome page target, leases it to the session, and returns its `tab_id`.

### `list_tabs`

```ts
list_tabs({
  agent_session_id: string,
  scope?: "owned"
}) -> {
  scope: "owned",
  tabs: OwnedTabSummary[]
}

list_tabs({
  agent_session_id: string,
  scope: "global_readonly"
}) -> {
  scope: "global_readonly",
  groups: GlobalTabGroup[]
}
```

The default scope is `owned`.

Owned listing returns only tabs leased by the caller. The `global_readonly` scope returns all visible Chrome page targets grouped by owner. The result is read-only target inventory with action handles only for caller-owned tabs.

### `new_tab`

```ts
new_tab({
  agent_session_id: string,
  url?: string,
  focus?: boolean
}) -> {
  tab: OwnedTabSummary
}
```

Creates a new Chrome page target and leases it to the session. If `focus` is true, the broker activates the target after creating it.

### `claim_tab`

```ts
claim_tab({
  agent_session_id: string,
  target_id: string,
  takeover?: boolean,
  user_instruction?: string
}) -> {
  tab: OwnedTabSummary
}
```

Claims an existing unowned Chrome page target.

If the target is owned by another session, the broker accepts the claim only when `takeover` is true and `user_instruction` is non-empty. The tool description states that callers set takeover after explicit user instruction. The `user_instruction` field records the authorization text and keeps takeover out of the ordinary claim path.

Successful takeover transfers ownership. The broker invalidates the former owner's lease before returning the new lease. The former owner's `tab_id` becomes invalid and fails later actions with `unknown_tab`, and the takeover session receives a new `tab_id`.

### Owned-Tab Action Tools

These tools require an owned `tab_id`:

```ts
focus_tab({ agent_session_id, tab_id }) -> { tab: OwnedTabSummary }
navigate({ agent_session_id, tab_id, url, wait_until?, timeout_ms? }) -> { tab: OwnedTabSummary }
screenshot({ agent_session_id, tab_id, full_page? }) -> { mime_type: "image/png", data_base64: string }
release_tab({ agent_session_id, tab_id }) -> { released: true }
close_tab({ agent_session_id, tab_id }) -> { closed: true }
```

Action semantics:

- `navigate` defaults to `wait_until: "load"` and `timeout_ms: 15000`. It returns `operation_timeout` when the timeout elapses before the load event.
- `screenshot` returns PNG bytes as base64. `full_page: false` captures the viewport. `full_page: true` uses CDP layout metrics and screenshot clipping for the main frame page bounds.
- `release_tab` releases the broker lease and leaves the Chrome target open when it still exists.
- `close_tab` closes the owned Chrome target when it exists and marks the lease `closed`.
- `focus_tab`, `navigate`, `screenshot`, `release_tab`, and `close_tab` all validate ownership before touching Chrome or changing lease state.

### Owned-Tab Page Actions and Diagnostics

These tools require an owned `tab_id`:

```ts
evaluate({ agent_session_id, tab_id, expression }) -> { value?: unknown, preview?: string }
click({ agent_session_id, tab_id, selector, timeout_ms? }) -> { clicked: true }
type_text({ agent_session_id, tab_id, text }) -> { typed: true }
press_key({ agent_session_id, tab_id, key, modifiers? }) -> { pressed: true }
console_messages({ agent_session_id, tab_id, since? }) -> { messages: ConsoleMessage[] }
network_events({ agent_session_id, tab_id, since? }) -> { events: NetworkEvent[] }
```

Action semantics:

- `evaluate` runs in the main frame execution context. If the result is JSON-serializable, return it in `value`; otherwise return a string preview.
- `click` accepts a CSS selector in the main frame. It finds the first visible matching element, scrolls it into view, and dispatches a left-click at its center.
- `type_text` sends text to the currently focused element after activating the owned tab.
- `press_key` supports one key at a time. `modifiers` may include `Alt`, `Control`, `Meta`, and `Shift`.
- Input actions activate the owned tab before dispatching input.
- `console_messages` and `network_events` return broker-owned ring buffers collected while the broker is attached to the target. They provide bounded per-target diagnostics.

## Enforcement Rules

The broker enforces these checks before every Chrome action:

- The `agent_session_id` identifies a known browser session.
- The `tab_id` identifies a known tab lease.
- The tab lease belongs to the session.
- The lease state is `active`.
- The Chrome target still exists and is a page target.

If any check fails, the broker returns `BrowserToolError` with the failed condition and a recovery action.

A missing Chrome target changes the lease state to `missing`. The broker keeps the missing lease visible in owned listings until the caller releases it or the broker restarts.

## Broker Protocol

The MCP facade and broker communicate over newline-delimited JSON. Requests have this shape:

```ts
type BrokerRequest = {
  id: string;
  method: string;
  params: unknown;
};
```

Responses have this shape:

```ts
type BrokerResponse =
  | { id: string; ok: true; result: unknown }
  | { id: string; ok: false; error: BrowserToolError };
```

The internal method names match the MCP tool names. The broker protocol is an internal interface for sharing lease state across MCP facade processes.

## Tab Organization

The primary organization mechanism is the broker's scoped view of tabs:

- Owned tabs are the caller's working set.
- Read-only target inventory groups visible Chrome targets by owner session.
- Other sessions' tabs are represented by target metadata and owner display ID; action handles and bearer session identifiers are caller-owned only.

The broker-enforced tab ownership boundary is independent of Chrome UI tab groups and per-agent browser windows. Those presentation features have separate designs.

## Plugin and Skill Contract

Plugin-facing MCP exposure surfaces:

- `.mcp.json` must expose only `visible-browser-lab`.
- `.codex-plugin/plugin.json` points at `.mcp.json`, and that file contains the facade server entry.
- `.codex/config.toml` is development configuration and must also use only the facade while testing this plugin.

This RFC governs the MCP surfaces owned by this plugin. Other locally installed browser MCP servers have separate contracts.

The visible-browser-lab skill uses the broker workflow:

1. Call `start_session` to start or reuse the selected browser runtime and create the browser session.
2. Reuse the returned `agent_session_id` for the whole workflow.
3. Use only owned `tab_id`s for browser actions.
4. Use `global_readonly` listing only for target identification or explicit user-directed tab transfer.
5. Use `focus_tab` as the explicit transition before native click or key input when the broker returns `focus_required` in the shipped focus-handoff path. RFC 00005 defines the successor interaction contract for normal page actions: target-session attachment, element preparation, and browser-protocol input that preserve the user's active application.
6. Use the facade MCP server for this workflow.

## Failure Modes

When managed Chrome is unavailable, browser tools fail with `chrome_unavailable` and diagnostics that identify browser discovery or startup failure. When an external endpoint is unavailable, the error identifies the configured endpoint and connection failure.

If the broker restarts, existing Chrome tabs remain open but leases are lost. The agent must start a new session and claim or create tabs.

If a tab closes through Chrome UI or another client, the next action marks the lease `missing` and returns `target_missing`.

If two sessions try to claim the same unowned target at the same time, the broker serializes the operation. One claim succeeds and the other receives `target_owned`.

If the caller releases a tab, the Chrome target remains open and becomes claimable by another session.

If the caller closes a tab, the broker closes only the owned target and marks the lease `closed`.

## Contract Boundaries

The enforced boundary is tab ownership. Browser storage, login state, cookies, extensions, service workers, and Chrome process state remain shared.

Bearer custody is part of the local contract: callers keep `agent_session_id` and `tab_id` private and use the facade for Chrome actions.

The facade exposes named owned-tab tools. Arbitrary Playwright or DevTools operations require a separate tool surface.

The page interaction tools use the semantics named in this RFC. The `click` selector is a main-frame CSS selector.

The Chrome profile is shared by all browser sessions in this RFC.

## Build Map

Available components:

1. Root Rust package, `visible-browser-lab-mcp` binary, and source-checkout wrapper script.
2. Managed and external runtime selection, browser discovery, platform launch adapters, runtime directories, and CDP availability checks.
3. Detached broker startup, local IPC locking, stale endpoint cleanup, pid file handling, and newline-delimited broker RPC.
4. Cross-platform broker IPC using local IPC transport so release binaries can be built for macOS, Linux, and Windows.
5. Session and lease registries with opaque bearer IDs and ownership checks shared by owned-tab action tools.
6. Broker-owned Chromiumoxide connections and handlers for target discovery, target creation, explicit target activation, navigation, screenshot capture, target close, page evaluation, CSS selector click, text input, key press, console buffering, and network buffering.
7. MCP tools: `start_session`, `list_tabs`, `new_tab`, `claim_tab`, `release_tab`, `focus_tab`, `navigate`, `screenshot`, `close_tab`, `evaluate`, `click`, `type_text`, `press_key`, `console_messages`, and `network_events`.
8. Plugin-facing MCP surfaces for the `visible-browser-lab` facade.
9. Skill text for the explicit session, owned-tab action, and diagnostics workflow.
10. Live MCP smoke test through `cargo xtask live-smoke`.
11. Real-browser MCP regression tests through `cargo test --test headless_mcp` and managed-runtime lifecycle tests through `cargo test --test managed_runtime`.
12. Installed Codex package validation through `cargo xtask install-smoke` and the release dry-run workflow.

## Test Plan

Use three test layers.

Unit tests:

- Lease registry returns errors for unknown sessions, unknown tabs, unowned tabs, non-active leases, and target-only actions.
- State transitions cover active, missing, released, closed, external close, release, close, and reclaim.
- Takeover requires `takeover: true` and non-empty `user_instruction`.
- Read-only target inventory includes action handles only for caller-owned tabs.

Broker contract tests with a fake CDP transport:

- Detached facade startup connects to an existing broker.
- Stale endpoint cleanup only removes local IPC state when the pid file is absent or dead.
- Concurrent claims for the same target serialize to one success and one `target_owned` error.
- Broker restart clears leases while preserving Chrome tab state.
- Navigation timeout, missing target, and Chrome unavailable errors return the documented `BrowserToolError` codes and recovery actions.
- Page actions route through ownership validation before CDP evaluation, CSS selector click, text input, or key press.
- Diagnostic buffers preserve per-target console and network events, support `since` filtering, and reset at lease boundaries.

Live Chrome smoke tests:

- Drive a supplied visible Chrome endpoint through `cargo xtask live-smoke --cdp-endpoint <url>` and verify explicit endpoint selection.
- Start, reuse, close, and relaunch managed Chrome through the production runtime manager.
- Verify macOS managed launch and background-safe actions preserve the frontmost application until an explicit focus operation.
- Start two sessions, create one tab in each, and verify default `list_tabs` returns only the caller's tab.
- Verify `global_readonly` lists both tabs, groups them by owner, and returns usable `tab_id`s only for caller-owned tabs.
- Verify ownership errors for focus, navigate, screenshot, evaluate, click, type_text, press_key, console_messages, network_events, release, and close against another session's tab.
- Verify `claim_tab` succeeds for an unowned target and returns `target_owned` for an owned target unless takeover is explicit.
- Verify closing a Chrome tab through Chrome UI or another client marks the lease `missing` and produces `target_missing` on the next action.
- Verify the plugin exposes exactly one MCP server: `visible-browser-lab`.
- Verify `evaluate`, `click`, `type_text`, `press_key`, `console_messages`, and `network_events` against a local HTTP fixture.
- Install a packaged Codex archive in a disposable Codex home and verify MCP discovery, managed browser startup, the owned-tab lifecycle, and cleanup.

## Validation Contract

- The plugin exposes the facade MCP server as `visible-browser-lab`.
- The broker validates ownership for every owned-tab action tool.
- Default tab listing returns the caller's owned working set.
- Read-only target inventory shows visible Chrome targets grouped by owner with action handles only for caller-owned tabs.
- Live smoke verifies session creation, owned/default listing, read-only target inventory, navigation, screenshots, page actions, diagnostics, ownership errors, release and claim transfer, close, and missing-target recovery.

A takeover call with `user_instruction` is the transfer path for mutating a tab owned by another session.

The plugin-facing MCP surfaces and visible-browser-lab skill use the facade workflow.
