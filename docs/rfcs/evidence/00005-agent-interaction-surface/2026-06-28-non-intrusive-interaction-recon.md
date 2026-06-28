# Non-Intrusive Browser Interaction Recon

Date: 2026-06-28

## Question

Browser Lab should support visible browser acceptance work while preserving the
user's active application. The intrusive path observed during the v0 RFC 0013
run was `focus_tab`: it activates the managed Chrome application so that
subsequent native pointer or keyboard input reaches the owned document.

This recon evaluates three implementation directions:

1. background semantic interaction for routine page actions;
2. background Chrome DevTools Protocol input where Chrome reliably delivers it;
3. isolated display or desktop execution for workflows that need browser focus
   away from the user's main desktop.

## Current Contract

RFC 00004 currently defines `click` and `press_key` as focused-document
operations. The broker checks `document.hasFocus()` and
`document.visibilityState === "visible"` before dispatching native input. When
the target does not have document focus, the broker returns `focus_required`.
`focus_tab` activates the Chrome target and then invokes the platform activation
adapter for managed Chrome.

Code points:

- `src/broker.rs`: `broker_click` returns `focus_required` before resolving and
  dispatching the click when the target does not have focus.
- `src/broker.rs`: `broker_press_key_v3` follows the same focused-document
  requirement before dispatching keyboard input.
- `src/managed_chrome.rs`: macOS `focus_tab` activation calls
  `NSRunningApplication::activateWithOptions`.
- `src/cdp.rs`: `has_focus` evaluates
  `document.hasFocus() && document.visibilityState === "visible"`.

## Local Evidence

### Existing Chromiumoxide Spike

The existing evidence record
`docs/rfcs/evidence/00004-installed-runtime/2026-06-24-chromiumoxide-spike.md`
contains the key behavior that led to the current contract:

| Capability | Result |
| --- | --- |
| `Input.dispatchMouseEvent` in a background visible target | Command returned successfully but did not trigger the element click. |
| `Input.dispatchKeyEvent` in a background visible target | Command returned successfully but did not trigger the page key listener. |
| `HTMLElement.click()` in a background visible target | Triggered the click handler without changing the active application. |
| `Input.insertText` in a background visible target | Updated the focused element while preserving the active application. |
| Screenshot, navigation, evaluation, diagnostics | Worked in a background target. |

### Refreshed Local Runs

Commands run from `/Users/wycats/plugins/visible-browser-lab`:

```bash
cargo test --test headless_mcp focus_contract -- --nocapture
cargo run --manifest-path spikes/chromiumoxide/Cargo.toml --release
VISIBLE_BROWSER_LAB_TEST_BROWSER_MODE=visible cargo run --manifest-path spikes/chromiumoxide/Cargo.toml --release
```

Results:

- `cargo test --test headless_mcp focus_contract -- --nocapture` passed.
- The headless Chromiumoxide spike connected to Chrome, created a target,
  navigated the fixture, evaluated the title, clicked the fixture button, typed
  into the fixture input, and received console/network events. The process did
  not exit after the relevant behavior checks and was terminated manually.
- The visible Chromiumoxide spike launched Chrome, then failed during
  frontmost-application restoration because the previously frontmost app
  (`Star Trek Fleet Command`) was no longer a valid running application. This
  failure is useful evidence about the test harness: frontmost restoration must
  tolerate a vanished application when running visible-mode focus tests.

### Active v0 Acceptance Feedback

The v0 RFC 0013 run supplied two concrete Browser Lab observations:

- Send-button click worked after the focused-document transition. Hit testing
  showed the enabled submit button at the click center, the center resolved to a
  child span inside the button, and the action produced `/chat/...` navigation
  plus `POST /chat/api/chat` and `/chat/api/chat/leaf` responses.
- The model-selector click remains the useful ambiguity case. The first click
  appeared to trigger a Design Systems card/action instead of opening the model
  selector. That should become a fixture for target resolution, center-point
  hit testing, overlay reporting, and post-action observation.

## Ecosystem Comparison

Primary references:

- Playwright actionability:
  <https://playwright.dev/docs/actionability>
- Playwright input:
  <https://playwright.dev/docs/input>
- Chrome DevTools Protocol Input domain:
  <https://chromedevtools.github.io/devtools-protocol/tot/Input/>
- Chrome headless mode:
  <https://developer.chrome.com/docs/chromium/headless>
- Selenium window interactions:
  <https://www.selenium.dev/documentation/webdriver/interactions/windows/>
- WebdriverIO headless and Xvfb:
  <https://webdriver.io/docs/headless-and-xvfb/>

Observed pattern:

- Playwright-style APIs present element actions as semantic operations with
  actionability checks: visible, stable, receives events, enabled, editable, and
  operation-specific readiness.
- CDP provides low-level input commands, but the protocol surface does not make
  OS foreground delivery guarantees for a headed browser window behind another
  application.
- WebDriver-style headed automation commonly assumes control of a browser
  session/window. It is a strong fit for test environments, and a weaker fit for
  a shared desktop where the user is doing other work.
- Xvfb is a Linux display-server strategy for running headed browser automation
  with a display without using the user's visible desktop. It is useful for CI
  and Linux isolation. It is not a direct macOS solution.
- Chrome headless provides unattended automation without a visible browser. It
  supports validation, but it does not satisfy the Browser Lab goal that the user
  can inspect and take over a visible browser profile.

## Option Evaluation

| Option | Preserves active app | Browser remains inspectable | Product behavior fidelity | Cross-platform shape | Notes |
| --- | --- | --- | --- | --- | --- |
| Background semantic actions | Yes | Yes | High for ordinary DOM controls; lower for browser-native or site-specific trusted-input checks | Broker-owned and portable | Best fit for routine agent workflows: buttons, links, form submission, menus, form fields, contenteditable insertion. Must report `input_mode` and action evidence. |
| Background CDP native input | Yes when delivered | Yes | Inconsistent in current macOS visible evidence | Protocol is portable; delivery is platform/window-state dependent | Commands can return success without page effects. Useful to keep testing, but not sufficient as the primary non-intrusive path. |
| Explicit focused-document input | No; activates managed Chrome | Yes | Highest fidelity for native pointer/key delivery | Implemented today | Good as an explicit handoff path when the user wants Chrome active or when a page workflow requires focused native input. |
| Isolated display/session | Preserves main desktop when available | Depends on display access; often separate from user's visible browser | High for test-like browser sessions | Strongest on Linux/Xvfb; requires separate investigation on macOS/Windows | Useful for CI or dedicated automation environments. It is a deployment/runtime mode, not the first fix for the shared visible profile. |
| Headless mode | Yes | No visible profile | High for validation paths | Already part of test harness | Good for CI and property tests. It does not meet the user-watchable Browser Lab workflow. |

## Workflow Decision Matrix

| Workflow | Preferred non-intrusive path | Evidence required | Foreground handoff condition |
| --- | --- | --- | --- |
| Navigation | CDP page navigation | URL/load state and optional snapshot | None for normal navigation. |
| Snapshot/inspection | Accessibility/DOM/CDP read APIs | Snapshot tree and bounds metadata | None. |
| Form fill | Existing `fill` / `type_text` style DOM or CDP text insertion | Value/input event state and optional form validity | Native focus only when the site rejects background insertion and user-visible input is requested. |
| Submit button | Background semantic click or form submit after actionability | URL, network request, editor/form state, dialog, or DOM mutation | Focused-document click when semantic action is unsupported or explicitly requested. |
| Menus/popovers | Background semantic click with post-action observation | Popover/menu state, target hit-test, topmost element stack | Foreground only when semantic activation does not open the menu and no page-level reason explains it. |
| Dialogs | Semantic trigger plus dialog handler | Dialog event and accepted/dismissed result | Foreground only for browser/OS dialogs outside CDP control. |
| File upload | DOM file input assignment through existing workspace-contained path resolution | File input files list and page change | Foreground only for OS file picker workflows, which Browser Lab should avoid in automated mode. |
| Keyboard shortcuts | Prefer semantic operation when available; otherwise CDP key path | Page effect or key listener result | Foreground for shortcuts that depend on browser/window focus. |
| Iframe actions | Same semantic action path after frame-aware reference resolution | Frame id, resolved element, frame-local hit-test, page effect | Foreground only after semantic and CDP paths fail with evidence. |
| Coordinate/pointer actions | CDP pointer path with hit-test evidence | Topmost element at point and page effect | Foreground when exact pointer behavior is the requested workflow. |

## Recommendation

The next implementation slice should make routine page interaction
non-intrusive by adding a background semantic action path for `click` and
submit-like controls:

1. Keep ownership validation unchanged.
2. Resolve the element through snapshot reference or CSS fallback.
3. Run strict actionability and hit-test checks.
4. Perform a page-level semantic activation for ordinary controls while Chrome
   stays behind the user's active application.
5. Return structured action evidence:
   - delivery mode (`semantic_background`, `cdp_input`, or
     `focused_document_input`);
   - resolved element summary;
   - center-point hit-test and obstruction stack;
   - post-action observation signals such as URL, network, DOM, dialog, or
     accessibility change.
6. Keep `focus_tab` as the explicit handoff for focused-document input.

The model-selector ambiguity from the v0 run should be the first fixture. The
desired result is either an opened model menu or a precise explanation naming
the element that would receive the action and why it differs from the requested
target.

## Open Items

- Define the exact `click` result schema extension for delivery mode and action
  evidence.
- Decide whether semantic activation is the default `click` behavior or a new
  explicit mode on `click`.
- Add a visible-mode macOS test harness that records the active application and
  treats a vanished previous application as a skipped restoration target, not a
  test failure.
