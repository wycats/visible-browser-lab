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

The broker waits for `DevToolsActivePort`, validates `/json/version`, and reports a bounded startup error with captured diagnostics. A healthy managed instance is reused across MCP server and broker restarts.

The launcher is implemented in this repository. General-purpose opener crates model opening URLs or files through desktop defaults. The managed runtime needs browser discovery, a persistent profile, CDP port discovery, startup diagnostics, and platform no-activation behavior as one owned contract.

# Focus Contract

Chrome remains visible and available for user interaction. MCP actions preserve the user's active application and active Chrome tab unless the caller invokes `focus_tab` or requests `focus: true` while creating a tab.

`Target.createTarget` receives `background: true` for the default tab-creation path. The broker calls `Target.activateTarget` only for:

- `focus_tab`;
- `start_session` or `new_tab` with `focus: true`.

Navigation, screenshots, evaluation, clicking, text insertion, key dispatch, console reads, and network reads operate on the owned target's CDP session without activating the target. Owned-tab actions continue to enforce `agent_session_id` and `tab_id` before calling Chrome.

The broker's `focused` field describes focus changes issued through the facade and is cleared when the corresponding target disappears, closes, or leaves its lease boundary.

# Installed Package Contract

Each host package contains one target-specific `visible-browser-lab-mcp` binary. Host MCP configuration resolves that binary from the installed plugin root rather than the invoking workspace.

For Codex, the generated MCP server entry sets `cwd` to `.`; Codex resolves a relative plugin MCP working directory against the installed plugin root. The command remains `./bin/visible-browser-lab-mcp` or its Windows executable form. Host-specific package validation applies the equivalent supported root-resolution mechanism for Claude Code and VS Code.

The generated package manifests use the release version supplied by the release workflow. The Codex, Claude Code, and VS Code manifests, `package-manifest.json`, archive names, Git tag, and binary `--version` output identify the same release version.

The Codex MCP entry declares the tool approval policy required for an installed automation invocation to call the facade's browser tools. Users can override that policy through normal Codex plugin configuration.

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

1. Add cross-platform runtime directory resolution and the managed/external runtime mode.
2. Add browser discovery, platform launch adapters, `DevToolsActivePort` readiness, reuse, and startup diagnostics.
3. Make background target creation explicit and reserve target activation for explicit focus operations.
4. Generate host MCP configuration from the installed plugin root and align package versions with the release tag.
5. Add the isolated installed-package smoke harness and CI coverage for package-root and managed-runtime behavior.
6. Update the skill and release documentation with the zero-setup installed workflow and explicit external-endpoint override.

# Drawbacks

The managed profile consumes persistent disk space and keeps browser state across sessions. Visible Browser Lab owns that profile so users can inspect and interact with its tabs.

Desktop focus behavior crosses Chrome, the operating system, and the window manager. The platform adapters express the strongest supported no-activation request, and real-browser tests verify observable focus behavior on supported development and CI platforms.

Browser discovery must track common installation paths. The explicit Chrome path override provides a stable route for custom installations.

# Alternatives

A desktop opener crate can open a browser or URL through system configuration. That API does not provide the lifecycle, profile, CDP endpoint, diagnostics, and no-activation guarantees required by this runtime.

The `browser_launcher` crate provides browser flags and executable discovery. Its public contract does not expose platform no-activation behavior or `DevToolsActivePort` resolution for a dynamically assigned port, so adopting it would still require the runtime layer defined here.

A fixed debugging port simplifies startup but creates collisions between users, tests, and concurrent installations. Chrome's dynamic port and `DevToolsActivePort` provide an isolated endpoint for each managed profile.

# Stage 3 Criteria

- A packaged Codex plugin starts `visible-browser-lab-mcp` from the installed plugin root in an unrelated workspace.
- With no external endpoint configuration, the broker starts or reuses the managed visible Chrome profile and reports its dynamically assigned endpoint.
- Default session and tab creation leave the user's active application and active Chrome tab unchanged on macOS.
- `focus_tab` and `focus: true` activate the owned tab.
- Navigation, screenshots, evaluation, click, text insertion, key dispatch, and diagnostics complete against a background owned tab.
- Runtime directories contain no developer-specific absolute paths.
- Package manifests and binary version output match the release version.
- The isolated Codex installation smoke test verifies installation, MCP discovery, managed Chrome startup, the tab lifecycle, and cleanup.
- Unit, fake-CDP, real-browser, package, macOS visible-mode, Windows compile, and release dry-run checks pass.

