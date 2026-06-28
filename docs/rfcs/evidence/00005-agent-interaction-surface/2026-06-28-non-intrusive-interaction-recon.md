# Non-Intrusive Browser Interaction Recon

Date: 2026-06-28

## Question

Browser Lab should support visible browser acceptance work while preserving the
user's active application. During the v0 RFC 0013 run, `focus_tab` activated the
managed Chrome application so that subsequent native pointer or keyboard input
reached the owned document.

This recon evaluates the implementation directions that can deliver normal page
actions while keeping the user's active application stable:

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

### Source-Code Reading

Temporary package root:
`/tmp/vbl-browser-recon`

Versions:

- `playwright` 1.61.1
- `puppeteer-core` 25.2.1
- `selenium-webdriver` 4.45.0
- `chromedriver` 150.0.0

Playwright locator click:

- `playwright-core/lib/coreBundle.js` implements `click` through
  `_retryPointerAction`.
- `_performPointerAction` waits for visible, enabled, and stable element state,
  scrolls the target into view, computes a clickable point, installs injected
  hit-target interception, then calls `page.mouse.click`.
- `page.mouse.click` sends protocol mouse move/down/up operations. Playwright's
  value is the actionability pipeline around browser input, not OS-level mouse
  movement.

Puppeteer element click and typing:

- `ElementHandle.click` scrolls the element into view, computes a clickable
  point, and calls `page.mouse.click`.
- `ElementHandle.type` and `ElementHandle.press` focus the element and call the
  page keyboard API.
- Puppeteer's CDP input implementation sends `Input.dispatchKeyEvent`,
  `Input.insertText`, and mouse events through the Chrome DevTools Protocol.

Selenium WebDriver:

- `WebElement.click()` sends the WebDriver `CLICK_ELEMENT` command to the
  remote end.
- `sendKeys()` sends `SEND_KEYS_TO_ELEMENT`; file inputs receive path-specific
  handling.
- The Actions API sends pointer and keyboard action sequences to the remote
  end. ChromeDriver owns translation into browser input.

Observed implementation pattern:

- Playwright-style APIs present element actions as semantic operations with
  actionability checks: visible, stable, receives events, enabled, editable, and
  operation-specific readiness.
- Playwright, Puppeteer, and ChromeDriver deliver page input through browser
  automation channels. They do not move the user's physical pointer.
- CDP provides the low-level input commands used by Puppeteer and available to
  Playwright-over-CDP. Browser Lab needs the same target activation and focused
  element setup that those tools perform inside the browser.
- WebDriver-style headed automation delegates input semantics to the remote end.
  It is useful prior art for actionability and remote-end ownership of browser
  input, even if Browser Lab continues to use CDP directly.
- Xvfb is a Linux display-server strategy for running headed browser automation
  with a display without using the user's visible desktop. It is useful for CI
  and Linux isolation. It is not a direct macOS solution.
- Chrome headless provides unattended automation without a visible browser. It
  supports validation, but it does not satisfy the Browser Lab goal that the user
  can inspect and take over a visible browser profile.

## Targeted Experiments

Sanitized result summary:
`docs/rfcs/evidence/00005-agent-interaction-surface/2026-06-28-non-intrusive-interaction-results.json`

Fixture:

- local HTTP server with a button, menu button, text input, contenteditable
  editor, submit form, and iframe button;
- client-side event log recording event kind, target, `isTrusted`, and
  `document.hasFocus()`;
- server-side submit endpoint for form-submission evidence.

Chrome:

- Chrome for Testing
  `/Users/wycats/plugins/visible-browser-lab/target/chrome-for-testing/150.0.7871.24/mac-arm64/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing`

Host:

- macOS on Apple Silicon.
- Before each action sequence, the harness activated Finder and recorded the
  frontmost bundle identifier.
- The page effect and frontmost application were recorded after the sequence.

Commands:

```bash
CFT_CHROME="/Users/wycats/plugins/visible-browser-lab/target/chrome-for-testing/150.0.7871.24/mac-arm64/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing" \
  node /tmp/vbl-browser-recon/recon-one.cjs playwright

CFT_CHROME="/Users/wycats/plugins/visible-browser-lab/target/chrome-for-testing/150.0.7871.24/mac-arm64/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing" \
  node /tmp/vbl-browser-recon/recon-one.cjs playwright-cdp

CFT_CHROME="/Users/wycats/plugins/visible-browser-lab/target/chrome-for-testing/150.0.7871.24/mac-arm64/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing" \
  node /tmp/vbl-browser-recon/recon-one.cjs puppeteer

CFT_CHROME="/Users/wycats/plugins/visible-browser-lab/target/chrome-for-testing/150.0.7871.24/mac-arm64/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing" \
  node /tmp/vbl-browser-recon/recon-one.cjs selenium
```

Results:

| Stack | Browser ownership mode | Frontmost before actions | Frontmost after actions | Page effects | Event evidence |
| --- | --- | --- | --- | --- | --- |
| Playwright launched Chrome | Tool-launched headed Chrome | Finder | Another pre-existing app became frontmost; Chrome did not remain frontmost | Button click, menu open, input fill, contenteditable fill, form submit, iframe click | Trusted click/input/submit events; document focus was true inside Chrome |
| Playwright connected over CDP | Fresh headed Chrome profile with dynamic CDP port | Finder | Finder | Button click, menu open, input fill, contenteditable fill, form submit, iframe click | Trusted click/input/submit events; document focus was true inside Chrome |
| Puppeteer launched Chrome | Tool-launched headed Chrome | Finder | Finder | Button click, menu open, input typing, contenteditable typing, form submit, iframe click | Trusted click/key/input/submit events; document focus was true inside Chrome |
| Selenium/ChromeDriver | ChromeDriver-launched headed Chrome | Finder | Finder | Button click, menu open, input typing, contenteditable typing, form submit, iframe click | Trusted click/key/input/submit events; document focus was true inside Chrome |

Findings:

- OS foreground application focus is not required for normal headed-browser page
  actions.
- A Chrome document can report `document.hasFocus() === true` and receive
  trusted automation input while Finder remains the frontmost macOS application.
- Playwright-over-CDP is the closest match to Browser Lab's architecture: a
  fresh headed Chrome profile, a CDP endpoint, browser-side target focus, locator
  actionability, and trusted page events with no Chrome foreground activation.
- The current Browser Lab focused-document preflight conflates browser document
  focus with OS application foregrounding through the recovery path. The browser
  automation path should make the Chrome target active inside the browser and
  focus the resolved element inside the document, then dispatch browser input
  through CDP.
- The raw-CDP follow-up attempted from Node was killed before producing output.
  It is not used as evidence. The completed Playwright-over-CDP and Puppeteer
  runs establish that CDP-backed headed automation can preserve the user's active
  app when the target and element preparation are correct.

## Option Evaluation

| Option | Preserves active app | Browser remains inspectable | Product behavior fidelity | Cross-platform shape | Notes |
| --- | --- | --- | --- | --- | --- |
| Browser-protocol actionability plus CDP input | Yes in macOS headed experiments | Yes | High for ordinary controls, typed input, submit, menu, and iframe actions | Matches Playwright/Puppeteer CDP mechanics; portable through Chrome | Best fit for Browser Lab: implement browser-side target focus, element focus, actionability, hit-target checks, and CDP input without OS activation. |
| Background semantic actions | Yes | Yes | High for ordinary DOM controls; lower for browser-native or site-specific trusted-input checks | Broker-owned and portable | Useful fallback for controls where DOM activation is the intended web-platform operation. Should report semantic delivery explicitly. |
| Explicit focused-document handoff | Activates managed Chrome | Yes | Reserved for user-directed handoff to an interactive browser session | Implemented today | Keep as a named handoff operation for the user taking control of the browser window. |
| Isolated display/session | Preserves main desktop when available | Separate inspection surface | High for test-like browser sessions | Strongest on Linux/Xvfb; macOS/Windows need separate runtime design | Useful for CI or remote automation sessions. It is a runtime mode, not the primary Browser Lab interaction mechanism. |
| Headless mode | Yes | No visible profile | High for validation paths | Already part of test harness | Good for CI and property tests. It does not meet the user-watchable Browser Lab workflow. |

## Workflow Decision Matrix

| Workflow | Preferred non-intrusive path | Evidence required | Foreground handoff condition |
| --- | --- | --- | --- |
| Navigation | CDP page navigation | URL/load state and optional snapshot | None for normal navigation. |
| Snapshot/inspection | Accessibility/DOM/CDP read APIs | Snapshot tree and bounds metadata | None. |
| Form fill | Browser-protocol element focus plus CDP text input or DOM value setting by operation | Value/input event state and optional form validity | User handoff for manual editing sessions. |
| Submit button | Browser-protocol click after actionability; semantic submit as explicit fallback | URL, network request, editor/form state, dialog, or DOM mutation | User handoff for manual browser takeover. |
| Menus/popovers | Browser-protocol click after target resolution and hit-test evidence | Popover/menu state, target hit-test, topmost element stack | User handoff for manual menu selection. |
| Dialogs | Browser-protocol trigger plus CDP dialog handler | Dialog event and accepted/dismissed result | User handoff for OS-level dialogs outside browser automation. |
| File upload | File input assignment through workspace-contained path resolution | File input files list and page change | User handoff for an OS file picker session. |
| Keyboard shortcuts | Browser target activation inside Chrome plus CDP key events | Page effect or key listener result | User handoff for application-level manual keyboard control. |
| Iframe actions | Browser-protocol action after frame-aware reference resolution | Frame id, resolved element, frame-local hit-test, page effect | User handoff for manual frame interaction. |
| Coordinate/pointer actions | CDP pointer path with hit-test evidence | Topmost element at point and page effect | User handoff for exploratory manual pointer control. |

## Recommendation

The next implementation slice should make routine page interaction preserve the
user's active application by adopting the browser-protocol action path proven by
Playwright-over-CDP, Puppeteer, and ChromeDriver:

1. Keep ownership validation unchanged.
2. Resolve the element through snapshot reference or CSS fallback.
3. Make the owned target active inside Chrome without activating the Chrome
   application.
4. Focus the resolved element inside the document when the operation needs
   keyboard or editable state.
5. Run strict actionability and hit-test checks modeled on Playwright's locator
   pipeline.
6. Dispatch browser input through CDP for clicks, typing, keyboard shortcuts,
   iframe targets, and coordinate actions.
7. Use semantic DOM activation as an explicit fallback for controls where the
   browser-protocol action reports no page effect and the requested operation is
   a web-platform activation.
8. Return structured action evidence:
   - delivery mode (`browser_protocol_input`, `semantic_dom_activation`, or
     `user_handoff`);
   - resolved element summary;
   - center-point hit-test and obstruction stack;
   - post-action observation signals such as URL, network, DOM, dialog, or
     accessibility change.
9. Keep `focus_tab` as the explicit operation for user handoff to the managed
   Chrome window.

The model-selector ambiguity from the v0 run should be the first fixture. The
desired result is either an opened model menu or a precise explanation naming
the element that would receive the action and why it differs from the requested
target.

## Open Items

- Define the exact action result schema extension for delivery mode, target
  focus, event trust, hit-test evidence, and post-action observation.
- Build a raw Browser Lab reproduction that mirrors Playwright-over-CDP:
  target activation inside Chrome, element focus, CDP mouse/key dispatch, and
  no macOS application activation.
- Add a visible-mode macOS test harness that records the active application,
  activates Finder before browser actions, and treats a vanished previous
  application as a skipped restoration target.
- Add v0 model-selector, contenteditable, menu/popover, iframe, dialog, file
  upload, and keyboard-shortcut fixtures to the production real-browser tests.
