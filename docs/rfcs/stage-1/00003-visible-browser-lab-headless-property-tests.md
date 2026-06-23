<!-- exo:3 ulid:01kvtxcp8a0592mpeaggnjke98 -->

# RFC 3: Visible Browser Lab Headless Property Tests

## Summary

Visible Browser Lab needs a CI-safe real-browser validation layer for the tab ownership facade. Unit tests and fake-CDP broker tests validate isolated Rust behavior. The visible smoke test validates the installed workflow against a user-watchable Chrome profile. This RFC adds a Cargo integration test layer that provisions Chrome for Testing through Rust, launches an isolated headless browser, drives the real `visible-browser-lab-mcp` stdio server, and checks the broker-enforced tab ownership contract through generated command sequences.

The new test target is `cargo test --test headless_mcp`. It runs against a temporary Chrome profile, a temporary broker state directory, and a local HTTP fixture. It uses this repository's MCP facade and CDP implementation directly.

## Motivation

RFC 00001 defines a local capability boundary around a shared visible Chrome profile. That boundary is only useful if the facade consistently validates ownership before every Chrome action. The current test stack covers important slices of that behavior, but each slice leaves a different gap:

- Unit tests cover data structures and helper functions without a browser.
- Fake-CDP broker tests cover broker behavior without Chrome's target lifecycle, event timing, or screenshot/input behavior.
- `cargo xtask live-smoke` covers the real facade against a visible Chrome profile, but it depends on a manually available browser endpoint and is shaped for local workflow validation.

The missing validation layer is a repeatable CI test that exercises the real MCP facade against real Chrome while keeping the browser profile, broker state, and test pages isolated. Property testing fits the tab-lease contract because many important failures appear only after a sequence of operations: release followed by claim, takeover followed by use of the old `tab_id`, target disappearance followed by an owned action, or diagnostics reads across lease boundaries.

## Layered Test Contract

The repository has three complementary test layers:

```text
cargo test --workspace
cargo test --test headless_mcp
cargo xtask live-smoke --cdp-endpoint http://127.0.0.1:9222
```

`cargo test --workspace` remains the fast unit and fake-CDP layer. `cargo test --test headless_mcp` is the CI real-browser regression layer. `cargo xtask live-smoke` remains the local visible Chrome validation path.

The headless test target validates the same system boundary as RFC 00001: the broker issues `agent_session_id` and `tab_id` bearer identifiers, then validates tab ownership before Chrome actions.

The generated command sequences exercise:

- browser session creation;
- owned and read-only tab listing;
- tab creation;
- tab claim;
- explicit takeover;
- tab release;
- tab close;
- externally missing Chrome targets;
- navigation;
- screenshots;
- page evaluation;
- CSS selector click;
- text input;
- key press;
- console diagnostics;
- network diagnostics.

The invariant set is the durable contract for this validation layer:

- default `list_tabs` returns only caller-owned leases;
- `global_readonly` returns action handles only for caller-owned tabs;
- owned-tab action tools reject foreign `tab_id` values;
- `release_tab` clears ownership and leaves the Chrome target claimable;
- `close_tab` closes the target and closes the lease;
- an externally closed target marks the lease `missing` on the next owned action;
- takeover invalidates the old `tab_id` and returns a new owned lease;
- diagnostics buffers reset at release, close, takeover, and missing-target boundaries.

## Browser Provisioning

Use Chrome for Testing as the browser source for CI and local headless runs. Chrome for Testing is Google's versioned Chrome flavor for browser automation and is released through machine-readable metadata.

Add this dev dependency:

```toml
chrome-for-testing-manager = { version = "0.12", default-features = false }
```

Use `chrome-for-testing-manager` to resolve and cache a regular Chrome for Testing package. The test harness launches the downloaded Chrome executable directly, then connects `visible-browser-lab-mcp` to Chrome through the selected CDP endpoint.

The Chrome for Testing cache path is `VISIBLE_BROWSER_LAB_CFT_CACHE_DIR` when set, otherwise `${CARGO_TARGET_DIR:-target}/chrome-for-testing`.

Launch Chrome with:

```text
--headless=new
--remote-debugging-port=0
--user-data-dir=<temp-profile-dir>
--no-first-run
--no-default-browser-check
```

On Linux CI, the harness also passes `--disable-dev-shm-usage` to avoid small shared-memory mounts on hosted runners.

The harness discovers the selected CDP endpoint from Chrome's `DevToolsActivePort` file in the temporary profile directory. It then starts `visible-browser-lab-mcp` with that endpoint and an isolated broker state directory.

## Property Model

Add these dev dependencies:

```toml
proptest = "1"
proptest-state-machine = "0.8"
```

Use `proptest-state-machine` for sequential model-based tests. The reference model tracks sessions, owned leases, released targets, closed targets, missing targets, claimable targets, takeover epochs, and diagnostic buffer epochs. The system under test is the real MCP facade connected to the isolated headless Chrome process.

Generated transitions cover the public MCP tools from RFC 00001. Each transition updates the reference model and checks the resulting MCP response, tab inventory, target ownership, and diagnostic state against the model.

The target supports normal Proptest controls. CI uses a modest default case count. Local deep runs can set `PROPTEST_CASES` to increase generated sequence coverage. Proptest failure persistence records replayable failing cases.

## Test Harness

Add a publish-disabled workspace crate named `visible-browser-lab-test-support`. Both `xtask` and `tests/headless_mcp.rs` use this crate for shared protocol and fixture helpers:

- stdio MCP client;
- local HTTP fixture;
- tool discovery checks;
- broker shutdown;
- tab cleanup;
- target close through Chrome HTTP endpoints;
- response helpers for tab summaries and browser tool errors.

Add a headless browser harness that owns:

- Chrome for Testing cache directory selection;
- version resolution and download;
- temporary Chrome profile creation;
- Chrome process startup;
- `DevToolsActivePort` endpoint discovery;
- process shutdown;
- temporary directory cleanup.

The first integration test is deterministic. It proves the harness by starting Chrome, starting `visible-browser-lab-mcp`, listing tools, creating two sessions, navigating to the local fixture, taking a screenshot, exercising page actions and diagnostics, and shutting down cleanly.

The state-machine tests build on that harness and run generated sequential command flows.

## CI Shape

Add a Linux headless browser job to CI. The job runs:

```text
cargo test --test headless_mcp
```

The job caches the Chrome for Testing artifact directory. The existing OS matrix continues to run `cargo test --workspace`, formatting checks, and `cargo xtask validate`.

Release PR dry-runs include the headless test before package generation. The headless test becomes part of the evidence for promoting RFC 00001 to Stage 3 because it exercises the implemented facade contract through a real browser in CI.

## Drawbacks

The headless test downloads and launches a real browser, so it is slower than unit and fake-CDP tests. The CI job should be separate from the fast Rust matrix so ordinary failures stay easy to read.

The test adds a network-dependent first run when the Chrome for Testing cache is empty. CI cache configuration and a local override path keep repeated runs fast.

State-machine tests require careful model maintenance. When RFC 00001 changes the facade contract, this RFC's model must change with it.

## Alternatives

Keep real-browser validation in `cargo xtask live-smoke`: this preserves one smoke command, but it keeps CI property testing outside Cargo's test harness and ties the real-browser check to a user-supplied visible Chrome endpoint.

Use system Chrome from the CI runner image: this removes the first-run download, but it makes test behavior depend on the runner image and its update cadence. Chrome for Testing gives the test harness an explicit browser source and cache.

Use a WebDriver client for the headless test: this is well-established browser-testing practice, but the facade is a CDP-facing tool. The validation layer should exercise this repository's MCP facade and CDP implementation directly.

Use deterministic scripted integration tests only: this is simpler to implement, and the RFC includes one deterministic harness test. It leaves sequence-sensitive lease bugs underexplored, so property/state-machine tests remain part of the design.

## Stage 2 Readiness

This RFC is ready for Stage 2 when the implementation task starts and the RFC still matches:

- the Cargo test target name;
- the Chrome for Testing provisioning crate and cache path;
- the shared test-support crate boundary;
- the generated transition model;
- the Linux CI job shape;
- the RFC 00001 Stage 3 validation relationship.

## References

- Chrome for Testing: https://developer.chrome.com/blog/chrome-for-testing
- Chrome Headless mode: https://developer.chrome.com/docs/chromium/headless
- Chrome DevTools Protocol endpoint discovery: https://chromedevtools.github.io/devtools-protocol/
- `chrome-for-testing-manager`: https://docs.rs/chrome-for-testing-manager
- Proptest state-machine testing: https://proptest-rs.github.io/proptest/proptest/state-machine.html

## Implementation Plan

1. Add the test-only dependencies for Chrome for Testing provisioning and property testing.
2. Add the `visible-browser-lab-test-support` workspace crate and move shared MCP smoke-test helpers from `xtask` into it.
3. Add the headless Chrome harness around Chrome for Testing.
4. Add `tests/headless_mcp.rs` with a deterministic smoke case that proves the harness, MCP facade, and headless Chrome wiring.
5. Add sequential state-machine property tests for the tab ownership invariants.
6. Add the Linux CI job with Chrome for Testing cache support.
7. Include the headless test in release PR dry-runs before package generation.
8. Update RFC 00001's Stage 3 validation language to reference the headless CI validation layer.

## Test Plan

RFC verification:

```text
git diff --check
exo rfc show 00003
```

Implementation verification:

```text
cargo fmt --check
cargo test --workspace
cargo test --test headless_mcp
cargo check --target x86_64-pc-windows-msvc
cargo xtask validate
cargo xtask live-smoke --cdp-endpoint http://127.0.0.1:9222
git diff --check
```

CI verification:

- existing Rust matrix runs `cargo test --workspace`;
- Linux headless job provisions Chrome for Testing and runs `cargo test --test headless_mcp`;
- release PR dry-run runs the headless test before package generation;
- Proptest failure artifacts remain available for replay.

## Acceptance Criteria

`cargo test --test headless_mcp` provisions Chrome for Testing through Rust, launches an isolated headless Chrome process, starts `visible-browser-lab-mcp`, and validates the facade through real CDP connections.

The headless target covers the broker-enforced tab ownership invariants with generated command sequences and replayable failures.

CI runs the headless test on Linux with a cached Chrome for Testing artifact directory.

The implementation exercises this repository's MCP facade and CDP code directly.

RFC 00001 can cite the headless CI validation layer as Stage 3 evidence for the implemented facade contract.

## Assumptions

This RFC defines a validation layer for the tab ownership facade. RFC 00001 remains the source of truth for the facade's MCP tool contract.

The first CI browser job targets Linux. macOS and Windows headless browser jobs can be added after the Linux job is stable.

Chrome for Testing is the primary browser source for headless validation.

The implementation uses direct CDP through the facade. WebDriver crates are ecosystem context for browser testing. This validation layer exercises the repository's own CDP implementation.
