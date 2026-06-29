# Chromiumoxide CDP Client Spike

Date: 2026-06-24

## Decision

Use `chromiumoxide 0.9.1` as the candidate CDP transport and generated protocol layer for the installed runtime. Integrate its low-level connection, command, page-session, and event APIs. Keep browser launch, tab ownership, selector semantics, diagnostics retention, and MCP error contracts in Visible Browser Lab.

The spike correctly observed that a narrow raw-CDP sequence returned success without delivering mouse or key input to the page while another application was foregrounded. The later non-intrusive interaction recon under RFC `00005` showed that this observation did not establish an OS-foreground requirement. Playwright-over-CDP, Puppeteer, and ChromeDriver all delivered trusted page input in headed Chrome while preserving the user's active application. The missing piece is target-session attachment, element preparation, actionability, and hit-test preparation before CDP input dispatch.

RFC `00004` records the focus handoff behavior shipped with the installed runtime. RFC `00005` defines the replacement interaction contract for normal page actions.

## Environment

- macOS arm64
- Rust 1.96
- `chromiumoxide 0.9.1`
- Chrome for Testing 150.0.7871.24
- Visible and headless browser modes
- Local HTTP fixture from `visible-browser-lab-test-support`

The reproducible programs are:

- `spikes/chromiumoxide`: real-browser behavior and lifecycle
- `spikes/chromiumoxide-core`: production dependency and cross-target API check

## CDP Results

| Capability | Result | Integration consequence |
| --- | --- | --- |
| Connect to an HTTP CDP endpoint | Passed | Use `Browser::connect_with_config` with viewport emulation disabled. |
| Maintain one multiplexed handler | Passed | Run one `Handler` task for the broker's Chrome connection. |
| Create a background target | Passed | Use a typed `Target.createTarget` command and retain the returned `TargetId`. |
| Resolve a target to a page session | Passed | Map Chrome `TargetId` to Chromiumoxide `Page` handles inside the broker backend. |
| Navigate | Passed | Typed page navigation worked without activating Chrome. |
| Evaluate JavaScript | Passed | `Page::evaluate_expression` matched the facade's main-frame evaluation contract. |
| Insert text | Passed | `Input.insertText` updated the focused element while Chrome remained behind another application. |
| Mouse input in a background target | Not delivered in the narrow raw-CDP sequence | `Input.dispatchMouseEvent` returned successfully but did not trigger the element click. Later recon showed browser-protocol preparation can deliver trusted click input without OS foreground activation. |
| Keyboard input in a background target | Not delivered in the narrow raw-CDP sequence | `Input.dispatchKeyEvent` returned successfully but did not trigger the page key listener. Later recon showed browser-protocol preparation can deliver trusted key input without OS foreground activation. |
| DOM click in a background target | Passed | `HTMLElement.click()` triggered the click handler without changing application focus. |
| Console events | Passed | Typed `Runtime.consoleAPICalled` events arrived through a page event stream. |
| Network events | Passed | Typed `Network.requestWillBeSent` events arrived through a page event stream. |
| Screenshot in a background target | Passed | Typed `Page.captureScreenshot` returned a valid PNG without activating Chrome. |
| Target close | Passed | The page target closed through its attached session. |
| Fresh-browser reconnect | Passed | A new `Browser::connect` and `Browser::new_page` succeeded after Chrome restarted. |
| Chrome process disappearance | Reported as WebSocket reset | Classify connection reset without a close handshake as browser disappearance and reconnect on the next broker action. |
| Frontmost application preservation | Passed in the stable visible run | Microsoft Edge remained frontmost through both background and `background: false, focus: false` target flows. |
| `background: false, focus: false` mouse input | Not delivered | The newer `focus` field preserved application focus but did not make trusted mouse input available. |

## API Boundaries

The spike found a reliable low-level integration surface:

- `Browser::connect_with_config`
- `Browser::execute`
- `Browser::get_page`
- `Page::execute`
- `Page::evaluate_expression`
- `Page::event_listener`

The high-level `Browser::new_page` path worked after a fresh reconnect. During background-target experiments, direct typed target creation plus `Browser::get_page` provided predictable control over target flags.

The high-level element click path timed out on a background visible target. Visible Browser Lab already owns CSS-selector visibility checks, scrolling, click coordinates, keyboard mapping, and error translation, so these behaviors should remain in the broker and issue generated CDP commands through `Page::execute`.

Chromiumoxide `0.9.1` does not include the current `focus` field in its generated `CreateTargetParams`. The spike verified that a small custom command implementing Chromiumoxide's `Method` and `Command` traits can send the current protocol shape through the same handler.

## Build Measurements

Measurements use clean release builds on the same machine.

| Artifact | Clean release build | Binary size | Dependency entries |
| --- | ---: | ---: | ---: |
| Current `visible-browser-lab-mcp` | 30.21 seconds | 7,234,416 bytes | 182 |
| Minimal connected Chromiumoxide client | 54.74 seconds | 9,201,568 bytes | 147 |
| Full spike with Chrome-for-Testing harness | 66 seconds | 14,889,008 bytes | 227 |

The standalone binary sizes are not additive. The full spike includes test-only Chrome provisioning and TLS dependencies that do not belong in the installed runtime. A production integration can remove `tokio-tungstenite` and the manual CDP transport when Chromiumoxide becomes the sole WebSocket client.

The minimal Chromiumoxide API package passes:

```bash
cargo check --manifest-path spikes/chromiumoxide-core/Cargo.toml
cargo check --manifest-path spikes/chromiumoxide-core/Cargo.toml --target x86_64-pc-windows-msvc
```

## Integration Shape

1. Add a broker-owned `ChromiumoxideRuntime` containing `Browser`, the handler task, and target-to-page lookup.
2. Connect that runtime to the endpoint selected by the managed or external runtime mode.
3. Replace manual request IDs, WebSocket loops, event parsing, and diagnostics monitor connections with Chromiumoxide commands and event streams.
4. Preserve the current `BrowserBackend` boundary so fake-CDP tests continue to exercise broker ownership independently.
5. Translate handler termination and WebSocket reset into `chrome_unavailable`; rebuild the runtime on the next request.
6. Retain broker-owned selector, click, text, key, screenshot, and error semantics while sending typed commands through `Page::execute`.
7. Validate the replacement with the existing deterministic, state-machine, headless, and visible browser suites before deleting the manual CDP transport.

## Superseded Interaction Decision

The spike left two apparent contracts before changing `click` and `press_key`:

- **DOM-mediated background actions:** preserve application focus; click and key events are untrusted DOM events and may differ from physical input.
- **Explicit trusted input:** preserve CDP input semantics; return `focus_required` until the caller explicitly invokes `focus_tab`.

RFC `00004` shipped the explicit trusted-input handoff. The RFC `00005` non-intrusive interaction recon supersedes that decision for normal page actions: use target-session attachment, element preparation, actionability, and CDP input while preserving the user's active application. Keep `focus_tab` for user handoff to the visible browser window.
