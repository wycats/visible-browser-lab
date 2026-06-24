# Chromiumoxide CDP Spike

This package evaluates `chromiumoxide` against the Visible Browser Lab broker's
CDP requirements without adding it to release builds or the normal workspace.

Run the headless transport and reconnect check:

```bash
cargo run --manifest-path spikes/chromiumoxide/Cargo.toml --release
```

Run the same target, action, diagnostics, and focus check with visible Chrome:

```bash
VISIBLE_BROWSER_LAB_TEST_BROWSER_MODE=visible \
  cargo run --manifest-path spikes/chromiumoxide/Cargo.toml --release
```

The visible macOS run restores the application that was frontmost before Chrome
started, then verifies that creating and operating on a background target does
not change the frontmost application.

The companion `../chromiumoxide-core` package checks the prospective production
dependency graph without the Chrome-for-Testing harness:

```bash
cargo check --manifest-path spikes/chromiumoxide-core/Cargo.toml
cargo check --manifest-path spikes/chromiumoxide-core/Cargo.toml \\
  --target x86_64-pc-windows-msvc
```

The full findings are recorded in
`docs/rfcs/evidence/00004-installed-runtime/2026-06-24-chromiumoxide-spike.md`.
