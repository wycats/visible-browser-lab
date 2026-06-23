<!-- exo:1 ulid:01kvrab2dbv0yn0pbwcsqmthrj -->

# RFC 00001: Visible Browser Lab Tab Isolation

## Summary

Before this RFC work, Visible Browser Lab exposed two raw MCP wrappers, `visible-playwright` and `visible-devtools`, that both connected to the same visible Chrome DevTools Protocol endpoint. That gave agents a shared browser the user can watch, but it also gave every agent access to the same global tab set. The safety rule lived in the skill text: an agent should remember the tab or page id it was using. That rule helped, but it was advisory. The tools still allowed an agent to select or mutate the wrong tab.

This RFC replaces the raw wrapper surface with one `visible-browser-lab` MCP facade. The facade keeps the shared visible Chrome profile, but it makes tab ownership explicit. An agent starts a browser session, receives an opaque `agent_session_id`, and then acts only through server-issued `tab_id`s that belong to that session. Listing tabs is scoped by default. A global tab inventory remains available for diagnosis and handoff, but it does not grant control over another agent's tabs.

The boundary is tab-level isolation inside one visible Chrome profile. Cookies, local storage, extension state, browser settings, service workers, and the Chrome process remain shared.

## Current State

### Pre-RFC Plugin Behavior

Before this RFC work, the plugin started Chrome with `scripts/start-visible-browser.sh` and exposed raw MCP servers in `.mcp.json`:

```json
{
  "mcpServers": {
    "visible-playwright": {
      "command": "npx",
      "args": [
        "-y",
        "@playwright/mcp@latest",
        "--cdp-endpoint=http://127.0.0.1:9222",
        "--browser=chrome"
      ]
    },
    "visible-devtools": {
      "command": "npx",
      "args": [
        "-y",
        "chrome-devtools-mcp@latest",
        "--browserUrl=http://127.0.0.1:9222",
        "--no-usage-statistics"
      ]
    }
  }
}
```

The plugin manifest points at that `.mcp.json`, and the development `.codex/config.toml` also exposes the same raw servers. Both wrappers can see the same Chrome targets. Any agent with access to the raw tools can enumerate, focus, navigate, inspect, or close another agent's tab.

### Current Branch Implementation

The current implementation exposes one MCP server, `visible-browser-lab`, from the plugin-facing MCP configuration. The facade starts or reuses one local broker process. The broker owns the in-memory session and tab lease registry, translates owned browser actions to Chrome DevTools Protocol calls, and validates tab ownership before each implemented Chrome action.

Implemented broker-backed MCP tools:

- `start_session`
- `list_tabs`
- `new_tab`
- `claim_tab`
- `release_tab`
- `focus_tab`
- `navigate`
- `screenshot`
- `close_tab`

The implemented live smoke test, `cargo xtask live-smoke`, drives the actual stdio MCP server against a visible Chrome CDP endpoint. It verifies session creation, owned default listings, global readonly inventory, new tab creation, navigation, PNG screenshot capture, ownership refusal, owned-target claim refusal, release and claim transfer, close, and missing-target recovery after an external Chrome tab close.

### Remaining Stage 3 Work

Stage 3 requires the RFC to match the implemented behavior and the validation evidence. Before promotion, owned-tab page actions and diagnostics must either be implemented and tested, or explicitly moved into a follow-up RFC or Exo task.

Owned-tab page actions and diagnostics are:

- `evaluate`
- `click`
- `type_text`
- `press_key`
- `console_messages`
- `network_events`

## Isolation Model

The v1 isolation model is non-adversarial lease isolation for same-user agents. It prevents accidental cross-tab actions by agents that follow the exposed MCP tool contract. It does not protect against a malicious local process, a caller that intentionally copies another session's bearer tokens, or a separate manually configured raw CDP client.

`agent_session_id` and `tab_id` are unguessable bearer capabilities issued by the broker. They are not derived from MCP transport identity. They must be generated from at least 128 bits of cryptographically secure randomness and encoded as opaque strings. The broker accepts a valid bearer capability from any MCP facade process connected to the same local broker.

Bearer values must never appear in global readonly tab listings, logs intended for normal agent consumption, or owner labels. Global listings use non-authorizing display identifiers so another agent can identify a tab for handoff without receiving the bearer values needed to act as the owner.

Chrome `targetId` is not an action capability. It may appear in readonly global inventory so an agent can ask to claim a tab, but owned-tab action tools must reject raw `target_id` inputs.

## Terminology

A **browser session** is a broker record for one agent workflow. It has an opaque `agent_session_id`, a non-authorizing `owner_display_id`, an optional label, and timestamps.

A **tab lease** is the broker's ownership record for one Chrome page target. It binds an opaque `tab_id` to a Chrome `targetId`, an owning browser session, the last observed title and URL, timestamps, and a lease state.

An **owned-tab action tool** is an MCP tool that requires both `agent_session_id` and `tab_id`, then asks the broker to verify that the tab is active and owned by that session before touching Chrome.

The **global readonly inventory** is the diagnostic tab listing that shows visible Chrome page targets grouped by owner display ID. It includes caller-owned action handles, withholds action handles for other sessions' tabs, and never exposes another session's `agent_session_id`.

An **owner display ID** is a non-authorizing identifier used for grouping and coordination in global readonly inventory. It is not accepted by owned listing or owned-tab action tools.

## Proposal

Expose one MCP server named `visible-browser-lab` and remove the raw wrappers from every plugin-facing MCP surface:

```json
{
  "mcpServers": {
    "visible-browser-lab": {
      "command": "/Users/wycats/plugins/visible-browser-lab/scripts/visible-browser-lab-mcp.sh",
      "args": ["--cdp-endpoint=http://127.0.0.1:9222"]
    }
  }
}
```

The command is a repo-local wrapper script. In development it runs the Rust binary with `cargo run --manifest-path /Users/wycats/plugins/visible-browser-lab/Cargo.toml --bin visible-browser-lab-mcp -- "$@"`. A later packaged build may replace the script internals with an exec of a release binary, but `.mcp.json` should keep one facade entry.

The MCP server is a Rust facade over the visible Chrome CDP endpoint. It provides a browser-workspace API instead of a global browser API. The facade owns the mapping between browser sessions, leased tabs, and Chrome `targetId`s. Browser tools validate ownership before touching a Chrome target.

The first implementation uses direct CDP for the broker and tab actions. The facade exposes broker-mediated tools so every Chrome action carries ownership validation.

## Build Shape

Add a root Rust package to this plugin repo:

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

Use these implementation choices for v1:

- MCP server: `rmcp` over stdio.
- Async runtime: `tokio`.
- CDP transport: direct websocket JSON using `tokio-tungstenite` and `serde_json`.
- ID generation: `rand` or `uuid` with at least 128 bits of CSPRNG entropy.
- Internal broker protocol: newline-delimited JSON request/response over local IPC using a small cross-platform abstraction. Unix platforms use local sockets. Windows uses the corresponding Windows local IPC transport. The wire format remains the same across platforms.

Do not add a browser automation dependency that reintroduces global page selection as the primary abstraction.

## Runtime Shape

The binary has two modes:

```text
visible-browser-lab-mcp [--cdp-endpoint URL]
visible-browser-lab-mcp broker --socket PATH --cdp-endpoint URL --state-dir PATH
```

The default mode is the MCP stdio facade. It ensures a broker is running, connects to the broker socket, and translates MCP tool calls into broker requests.

The broker mode owns CDP connections and lease state. It is a detached child process started by the first MCP facade process when no broker is listening.

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

The broker keeps lease state in memory for v1. It does not persist leases across broker restarts. If the broker restarts, agents must call `start_session` again and claim or create tabs again. Chrome tabs and browser profile state survive because they are owned by Chrome, not by the broker.

## CDP Endpoint Selection

The broker resolves the Chrome endpoint in this order:

1. `--cdp-endpoint` CLI argument.
2. `VISIBLE_BROWSER_CDP_ENDPOINT` environment variable.
3. `http://127.0.0.1:${VISIBLE_BROWSER_CDP_PORT:-9222}`.

The startup script should pass the same endpoint policy to the MCP facade. The script may keep accepting a port through `VISIBLE_BROWSER_CDP_PORT`, but the facade and broker must not hard-code `9222` after a different endpoint was provided.

Before leasing tabs, the broker validates that the endpoint responds to `/json/version`, exposes a browser websocket URL, and reports a Chrome-compatible browser. CDP does not reliably expose the profile directory, so v1 treats the selected endpoint as authoritative. If another Chrome is already listening on the chosen endpoint, the broker will use it. The startup script should make endpoint reuse visible in its output so the user can notice a wrong endpoint before automation begins.

Chrome startup remains outside the broker in v1. Agents continue to run `scripts/start-visible-browser.sh` before browser work. If the broker cannot reach the CDP endpoint, every browser tool fails with the endpoint it tried and the startup command to run.

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
- `missing` leases remain in owned listings until released, so the agent can see why its previous handle stopped working.

The broker does not automatically expire sessions in v1.

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

Global readonly summaries do not include action handles for tabs owned by another session:

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

For caller-owned tabs, `caller_tab_id` may be present in global readonly results. For tabs owned by another session, `caller_tab_id` must be absent.

`owner_display_id` is not an `agent_session_id`. It is a stable, non-authorizing display handle generated for grouping and coordination only. It cannot be passed to owned listing or owned-tab action tools.

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

Error messages should name the failed condition and the next safe action.

## Tool Contract

Every mutating or inspecting browser action requires both `agent_session_id` and an owned `tab_id`. Tools reject attempts to act by active tab, tab index, title, URL, or raw `target_id`.

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

Owned listing returns only tabs leased by the caller. Global listing returns all visible Chrome page targets grouped by owner. Global listing is read-only and does not grant action handles for tabs owned by another session.

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

If the target is owned by another session, the broker refuses the claim unless `takeover` is true and `user_instruction` is non-empty. The tool description must say that callers may set takeover only after explicit user instruction. This does not make user intent cryptographically enforceable; it makes takeover visible in the call contract and prevents accidental takeover from ordinary tab listing.

Successful takeover is a transfer, not a shared lease. The broker must invalidate the previous owner's lease before returning the new lease. The previous `tab_id` must not remain active, and the takeover session receives a new `tab_id`.

### Implemented Owned-Tab Action Tools

These tools are implemented on the current branch and require an owned `tab_id`:

```ts
focus_tab({ agent_session_id, tab_id }) -> { tab: OwnedTabSummary }
navigate({ agent_session_id, tab_id, url, wait_until?, timeout_ms? }) -> { tab: OwnedTabSummary }
screenshot({ agent_session_id, tab_id, full_page? }) -> { mime_type: "image/png", data_base64: string }
release_tab({ agent_session_id, tab_id }) -> { released: true }
close_tab({ agent_session_id, tab_id }) -> { closed: true }
```

Implemented action semantics:

- `navigate` defaults to `wait_until: "load"` and `timeout_ms: 15000`. It returns `operation_timeout` if the load event does not arrive before the timeout.
- `screenshot` returns PNG bytes as base64. `full_page: false` captures the viewport. `full_page: true` uses CDP layout metrics and screenshot clipping for the main frame page bounds.
- `release_tab` releases the broker lease and leaves the Chrome target open when it still exists.
- `close_tab` closes the owned Chrome target when it exists and marks the lease `closed`.
- `focus_tab`, `navigate`, `screenshot`, `release_tab`, and `close_tab` all validate ownership before touching Chrome or changing lease state.

### Remaining Owned-Tab Page Actions and Diagnostics

These tools are part of the RFC tool contract and remain Stage 3 work unless moved into a follow-up RFC or Exo task before promotion:

```ts
evaluate({ agent_session_id, tab_id, expression }) -> { value?: unknown, preview?: string }
click({ agent_session_id, tab_id, selector, timeout_ms? }) -> { clicked: true }
type_text({ agent_session_id, tab_id, text }) -> { typed: true }
press_key({ agent_session_id, tab_id, key, modifiers? }) -> { pressed: true }
console_messages({ agent_session_id, tab_id, since? }) -> { messages: ConsoleMessage[] }
network_events({ agent_session_id, tab_id, since? }) -> { events: NetworkEvent[] }
```

Remaining action semantics:

- `evaluate` runs in the main frame execution context. If the result is JSON-serializable, return it in `value`; otherwise return a string preview.
- `click` accepts only a CSS selector in the main frame. It clicks the center of the first matching visible element. Iframe traversal, role selectors, text selectors, and Playwright locator semantics are out of scope for v1.
- `type_text` sends text to the currently focused element after activating the owned tab.
- `press_key` supports one key at a time. `modifiers` may include `Alt`, `Control`, `Meta`, and `Shift`.
- Input actions activate the owned tab before dispatching input.
- `console_messages` and `network_events` return broker-owned ring buffers collected while the broker is attached to the target. They are diagnostic tools, not complete tracing infrastructure.

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

The internal method names match the MCP tool names. The broker protocol is not public API; it exists to let multiple MCP facade processes share lease state through one broker.

## Tab Organization

The primary organization mechanism is the broker's scoped view of tabs:

- Owned tabs are the caller's working set.
- Global readonly tabs are grouped by owner session.
- Other sessions' tabs are visible for coordination, but they do not include action handles or bearer session identifiers.

V1 does not require Chrome UI tab groups or one browser window per agent. Those may be added later if they improve the human-visible browser view, but they are not required for tab isolation.

## Plugin and Skill Changes

The implementation must update every plugin-facing MCP exposure surface:

- `.mcp.json` must expose only `visible-browser-lab`.
- `.codex-plugin/plugin.json` can continue pointing at `.mcp.json`, but that file must no longer contain raw wrappers.
- `.codex/config.toml` is development configuration and must also use only the facade while testing this plugin.

Raw `visible-playwright` and `visible-devtools` may still be installed elsewhere for unrelated work, but they must not be exposed by this plugin or invoked by the visible-browser-lab skill.

The visible-browser-lab skill should change from advisory tab-id language to the broker workflow:

1. Start or verify visible Chrome with `scripts/start-visible-browser.sh`.
2. Call `start_session` before browser work.
3. Reuse the returned `agent_session_id` for the whole workflow.
4. Use only owned `tab_id`s for browser actions.
5. Use `global_readonly` listing only for diagnosis or explicit user-directed handoff.
6. Do not call raw Playwright or DevTools MCP servers for this workflow.

## Failure Modes

If Chrome is not running, every browser tool fails with `chrome_unavailable`, the CDP endpoint it tried, and the startup command to run.

If the broker restarts, existing Chrome tabs remain open but leases are lost. The agent must start a new session and claim or create tabs.

If a tab closes outside the broker, the next action marks the lease `missing` and returns `target_missing`.

If two sessions try to claim the same unowned target at the same time, the broker serializes the operation. One claim succeeds and the other receives `target_owned`.

If the caller releases a tab, the Chrome target remains open and becomes claimable by another session.

If the caller closes a tab, the broker closes only the owned target and marks the lease `closed`.

## Non-Goals

This RFC does not isolate browser storage, login state, cookies, extensions, service workers, or Chrome process state.

This RFC does not protect against malicious same-user clients that intentionally steal bearer capabilities or connect directly to CDP.

This RFC does not expose arbitrary Playwright or DevTools operations through a pass-through escape hatch.

This RFC does not reproduce the full Playwright locator model in v1.

This RFC does not require a new Chrome profile per agent.

## Implementation Plan

Completed implementation groups:

1. Root Rust package, `visible-browser-lab-mcp` binary, and repo wrapper script.
2. Endpoint resolution and CDP availability checks.
3. Detached broker startup, local IPC locking, stale endpoint cleanup, pid file handling, and newline-delimited broker RPC.
4. Cross-platform broker IPC using local IPC transport so release binaries can be built for macOS, Linux, and Windows.
5. Session and lease registries with opaque bearer IDs and ownership checks shared by implemented owned-tab action tools.
6. CDP target discovery, target creation, target activation, navigation, screenshot capture, and target close.
7. Implemented MCP tools: `start_session`, `list_tabs`, `new_tab`, `claim_tab`, `release_tab`, `focus_tab`, `navigate`, `screenshot`, and `close_tab`.
8. Plugin-facing MCP surfaces migrated to the `visible-browser-lab` facade.
9. Skill text updated to describe the explicit session and owned-tab workflow.
10. Live MCP smoke test added through `cargo xtask live-smoke`.

Remaining implementation group:

1. Owned-tab page actions and diagnostics: `evaluate`, selector-based `click`, `type_text`, `press_key`, `console_messages`, and `network_events`.
2. CDP target attachment, console buffering, and network buffering required by the diagnostic tools.
3. Skill wording update after owned-tab page actions and diagnostics exist.

## Test Plan

Use three test layers.

Unit tests:

- Lease registry rejects unknown sessions, unknown tabs, unowned tabs, non-active leases, and raw target-only actions.
- State transitions cover active, missing, released, closed, external close, release, close, and reclaim.
- Takeover requires `takeover: true` and non-empty `user_instruction`.
- Global readonly listing withholds action handles for other sessions.

Broker contract tests with a fake CDP transport:

- Detached facade startup connects to an existing broker.
- Stale endpoint cleanup only removes local IPC state when the pid file is absent or dead.
- Concurrent claims for the same target serialize to one success and one `target_owned` error.
- Broker restart clears leases without assuming Chrome tabs closed.
- Navigation timeout, missing target, and Chrome unavailable errors return the expected `BrowserToolError` codes and recovery actions.

Live Chrome smoke tests:

- Bootstrap a visible Chrome instance through `scripts/start-visible-browser.sh` and verify endpoint precedence respects CLI and environment overrides.
- Start two sessions, create one tab in each, and verify default `list_tabs` returns only the caller's tab.
- Verify `global_readonly` lists both tabs, groups them by owner, and withholds usable `tab_id`s for tabs owned by another session.
- Verify one session cannot focus, navigate, screenshot, release, or close another session's tab.
- Verify `claim_tab` succeeds for an unowned target and refuses an owned target without takeover.
- Verify closing a Chrome tab outside the broker marks the lease `missing` and produces `target_missing` on the next action.
- Verify the plugin exposes only `visible-browser-lab` and no raw `visible-playwright` or `visible-devtools` tools.
- When owned-tab page actions and diagnostics are implemented, extend the smoke test to verify `evaluate`, `click`, `type_text`, `press_key`, `console_messages`, and `network_events` on owned tabs and ownership refusal for those tools on foreign tabs.

## Acceptance Criteria

For the current Stage 2 implementation:

- The plugin exposes the facade MCP server as `visible-browser-lab`.
- The broker validates ownership for every implemented owned-tab action tool.
- Default tab listing returns the caller's owned working set.
- Global readonly inventory shows visible Chrome targets for coordination without exposing foreign action handles.
- Live smoke verifies session creation, owned/default listing, global readonly inventory, navigation, screenshots, ownership refusal, release and claim transfer, close, and missing-target recovery.

For Stage 3 promotion:

- Owned-tab page actions and diagnostics are implemented and tested, or they are explicitly moved into a follow-up RFC or Exo task before promotion.
- The RFC states the implemented behavior, remaining requirements, and validation evidence clearly.

An agent cannot mutate another agent's tab unless it performs a takeover call that records user instruction text.

The plugin-facing MCP surfaces and visible-browser-lab skill use the facade workflow.
