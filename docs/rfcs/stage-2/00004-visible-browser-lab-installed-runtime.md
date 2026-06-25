<!-- exo:4 ulid:01kvxgsb6w91dz0nqth09bbzn6 -->

# RFC 4: Visible Browser Lab Installed Runtime

# Summary

Visible Browser Lab provides a self-contained installed runtime for its visible Chrome MCP facade. The packaged `visible-browser-lab-mcp` binary starts or reuses a dedicated Chrome profile when no external CDP endpoint is configured, creates tabs without activating the Chrome application by default, and resolves its binary and runtime state independently of the invoking workspace.

The runtime preserves the explicit endpoint path for developers who want to attach the facade to an existing Chrome instance. The managed path is the default installed experience.

# Motivation

A host-installed plugin must work from any project directory without requiring the user to start Chrome, choose a debugging port, create a profile, or configure broker state. Browser actions should remain visible while respecting the user's current keyboard and pointer focus. Package metadata must identify the release that produced the archive, and installation validation must exercise the package exactly as the host loads it.

# Runtime Contract

## Runtime selection

`visible-browser-lab-mcp` selects one of two runtime modes:

- **Managed Chrome** is selected when `--cdp-endpoint`, `VISIBLE_BROWSER_CDP_ENDPOINT`, and `VISIBLE_BROWSER_CDP_PORT` are absent. The broker starts or reuses the Visible Browser Lab Chrome profile and discovers its dynamically assigned CDP endpoint.
- **External CDP** is selected when an endpoint or port is supplied through the command line or environment. The broker validates and uses that endpoint without managing the browser process or profile.

The selected mode is recorded in broker status and diagnostic logs.

## Platform runtime directories

The `directories` crate resolves the operating-system cache directory. The default state root is the platform cache location for `visible-browser-lab`. It contains:

- broker IPC, lock, PID, and log files;
- `chrome-profile/`, the persistent managed Chrome user-data directory;
- `DevToolsActivePort`, written by Chrome inside the managed profile;
- launcher diagnostics sufficient to explain browser discovery and startup failures.

`--state-dir` and `VISIBLE_BROWSER_LAB_STATE_DIR` select an explicit state root for tests and controlled installations.

## Browser discovery

`VISIBLE_BROWSER_LAB_CHROME_PATH` selects an explicit Chromium-family executable. Otherwise the launcher searches supported browser installation paths for Google Chrome, Chrome for Testing, Chromium, Microsoft Edge, and Brave in that order where each browser is available.

Discovery returns the executable path and browser family. Startup errors name the attempted executable paths and the configuration override.

## CDP connection

The broker uses `chromiumoxide` 0.9.1 with default features disabled as its low-level Chrome DevTools Protocol client. Each configured CDP endpoint has one `Browser` connection and one continuously driven `Handler`. Viewport emulation is disabled so screenshots, layout coordinates, and input use the visible browser's native viewport.

The broker uses Chromiumoxide's typed protocol commands and event streams for target inventory, target creation and closure, navigation, JavaScript evaluation, screenshots, mouse and keyboard input, console messages, and network events. The broker retains ownership of endpoint selection, Chrome launch, session and tab leases, error translation, and MCP response types.

Handler completion invalidates the endpoint connection. A subsequent broker operation establishes a new `Browser` and `Handler` against the selected endpoint. If the endpoint no longer identifies a running Chrome instance, the operation returns the broker's browser-unavailable error with the connection failure as diagnostic context. The broker does not replay an interrupted Chrome action because the client cannot establish whether Chrome performed it before the connection ended.

## Managed browser launch

The launcher starts Chrome with:

- the managed `chrome-profile/` directory;
- `--remote-debugging-port=0`;
- `--no-first-run` and `--no-default-browser-check`;
- a startup page owned by Visible Browser Lab;
- stdout and stderr captured in the runtime log directory.

Platform launch adapters preserve the common runtime contract:

- macOS asks LaunchServices to open a new Chrome instance without activating it and passes the managed Chrome arguments;
- Windows starts the discovered executable with a non-activating window startup mode;
- Linux starts the discovered executable directly and relies on desktop focus-stealing prevention while creating Chrome targets in the background.

The broker waits for `DevToolsActivePort`, validates `/json/version`, and reports a bounded startup error with captured diagnostics. A healthy managed instance is reused across MCP server and broker restarts. Broker status reports `managed` or `external` together with the resolved CDP endpoint. Broker socket, lock, and PID paths carry protocol version 2 so an older broker cannot satisfy this runtime contract.

The launcher is implemented in this repository. General-purpose opener crates model opening URLs or files through desktop defaults. The managed runtime needs browser discovery, a persistent profile, CDP port discovery, startup diagnostics, and platform no-activation behavior as one owned contract.

# Focus Contract

Chrome remains visible and available for user interaction. Session and tab creation preserve the user's active application and active Chrome tab unless the caller invokes `focus_tab` or requests `focus: true` while creating a tab.

`Target.createTarget` receives `background: true` for the default tab-creation path. The broker calls `Target.activateTarget` only for:

- `focus_tab`;
- `start_session` or `new_tab` with `focus: true`.

For managed Chrome, these explicit focus requests also invoke the platform activation adapter after activating the CDP target. On macOS, the adapter asks LaunchServices to bring the managed Chrome application forward. Background tab creation never invokes the activation adapter.

Navigation, screenshots, evaluation, text insertion, console reads, and network reads operate on the owned target's CDP session without activating Chrome. `type_text` inserts text into the element that already owns DOM focus in that target.

`click` and `press_key` dispatch native mouse and keyboard events only while the owned target is the visible, focused document. After validating `agent_session_id` and `tab_id`, the broker evaluates `document.hasFocus()` and requires `document.visibilityState === "visible"` in the target. When either condition is false, the broker returns `focus_required` without dispatching input. The caller invokes `focus_tab` and retries the action. This explicit transition keeps trusted input attached to the tab the user can see as active.

The broker implements `click` through a CSS selector, a visible-element center point, and CDP mouse events. It implements `press_key` through CDP keyboard events. DOM-mediated click and keyboard fallbacks are not part of the tool contract.

Every owned-tab action validates `agent_session_id` and `tab_id` before evaluating focus or calling Chrome.

The broker's `focused` field describes focus changes issued through the facade and is cleared when the corresponding target disappears, closes, or leaves its lease boundary.

# Installed Package Contract

Each host package contains one target-specific `visible-browser-lab-mcp` binary. Host MCP configuration resolves that binary from the installed plugin root rather than the invoking workspace.

For Codex, the generated MCP server entry sets `cwd` to `.`; Codex resolves a relative plugin MCP working directory against the installed plugin root. The command remains `./bin/visible-browser-lab-mcp` or its Windows executable form. The server advertises the `codex/sandbox-state-meta` experimental capability. Codex then supplies the active turn working directory independently on each tool request at `_meta["codex/sandbox-state-meta"]["sandboxCwd"]`. Plugin-root resolution therefore does not repurpose the workspace value.

Claude Code and VS Code packages use the Claude plugin format. Their MCP command and working directory use `${CLAUDE_PLUGIN_ROOT}`, which both hosts expand to the installed plugin root. Package validation checks each host's generated manifest path, MCP command, and working directory.

The release workflow removes the `v` prefix from a release tag and supplies the resulting semantic version while compiling and packaging. Pull-request dry runs use the Cargo package version. The Codex, Claude Code, and VS Code manifests, `package-manifest.json`, archive names, Git tag, and binary `--version` output identify the same release version.

Codex applies the user's configured MCP approval policy when the facade invokes browser tools. The installed package preserves that policy rather than overriding it.

# Installation Validation

`cargo xtask install-smoke` validates a built or downloaded host package in a disposable environment. The Codex path:

1. creates isolated `HOME`, `CODEX_HOME`, `CODEX_SQLITE_HOME`, workspace, broker state, and Chrome profile directories;
2. installs the package through a local temporary marketplace;
3. verifies the installed manifest version, resolved MCP command, packaged binary path, and plugin cache location;
4. runs the packaged MCP binary through the host configuration;
5. starts a session, lists the owned tab, evaluates a deterministic page title, and closes the tab;
6. confirms the managed browser used the isolated profile and state directory;
7. removes the disposable environment after retaining sanitized diagnostics for failures.

The test accepts a preauthenticated isolated Codex home for model-driven invocation. Package-root resolution, MCP startup, managed browser launch, and the deterministic facade lifecycle are validated without model participation.

# Implementation Map

1. Replace the broker's manual WebSocket transport with a broker-owned Chromiumoxide connection and handler for each CDP endpoint.
2. Add cross-platform runtime directory resolution and the managed/external runtime mode.
3. Add browser discovery, platform launch adapters, `DevToolsActivePort` readiness, reuse, and startup diagnostics.
4. Make background target creation explicit, reserve target activation for explicit focus operations, and require browser focus for trusted mouse and keyboard dispatch.
5. Generate host MCP configuration from the installed plugin root and align package versions with the release tag.
6. Add the isolated installed-package smoke harness and CI coverage for package-root and managed-runtime behavior.
7. Update the skill and release documentation with the zero-setup installed workflow, explicit focus transition, and external-endpoint override.

# Drawbacks

The managed profile consumes persistent disk space and keeps browser state across sessions. Visible Browser Lab owns that profile so users can inspect and interact with its tabs.

Desktop focus behavior crosses Chrome, the operating system, and the window manager. The platform adapters express the strongest supported no-activation request, and real-browser tests verify observable focus behavior on supported development and CI platforms.

Native mouse and keyboard dispatch requires an explicit focus operation for background tabs. Callers handle `focus_required` by deciding whether to activate the tab and retry the input action.

Browser discovery must track common installation paths. The explicit Chrome path override provides a stable route for custom installations.

# Alternatives

A desktop opener crate can open a browser or URL through system configuration. That API does not provide the lifecycle, profile, CDP endpoint, diagnostics, and no-activation guarantees required by this runtime.

The `browser_launcher` crate provides browser flags and executable discovery. Its public contract does not expose platform no-activation behavior or `DevToolsActivePort` resolution for a dynamically assigned port, so adopting it would still require the runtime layer defined here.

A repository-owned WebSocket transport can send the required CDP messages directly. Chromiumoxide already provides typed protocol commands, target sessions, event streams, and a driven connection lifecycle. Using that client concentrates this repository's implementation on browser ownership, focus, error semantics, and MCP behavior.

A fixed debugging port simplifies startup but creates collisions between users, tests, and concurrent installations. Chrome's dynamic port and `DevToolsActivePort` provide an isolated endpoint for each managed profile.

# Stage 3 Criteria

- A packaged Codex plugin starts `visible-browser-lab-mcp` from the installed plugin root in an unrelated workspace.
- With no external endpoint configuration, the broker starts or reuses the managed visible Chrome profile and reports its dynamically assigned endpoint.
- Default session and tab creation leave the user's active application and active Chrome tab unchanged on macOS.
- `focus_tab` and `focus: true` activate the owned tab.
- Navigation, screenshots, evaluation, text insertion, and diagnostics complete against a background owned tab without activating Chrome.
- `click` and `press_key` return `focus_required` without dispatching input when the owned target lacks browser focus.
- After `focus_tab`, retrying `click` or `press_key` dispatches the requested native input to the owned target.
- Handler termination invalidates the Chromiumoxide connection, and the next broker operation reconnects to a running endpoint.
- Runtime directories contain no developer-specific absolute paths.
- Package manifests and binary version output match the release version.
- The isolated Codex installation smoke test verifies installation, MCP discovery, managed Chrome startup, the tab lifecycle, and cleanup.
- Unit, fake-CDP, real-browser, package, macOS visible-mode, Windows compile, and release dry-run checks pass.
