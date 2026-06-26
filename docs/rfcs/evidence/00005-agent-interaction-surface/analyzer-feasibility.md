# Performance Analyzer Feasibility

Visible Browser Lab performs trace analysis inside `visible-browser-lab-mcp`.
The analyzer receives a completed trace artifact and bounded analysis parameters.
It does not receive a CDP endpoint, browser target, session identifier, tab
identifier, or browser profile path.

## Release Targets

Perfetto v56.1 publishes `trace_processor_shell` archives for macOS arm64 and
x86_64, Linux arm64 and x86_64, and Windows x86_64. Its release does not
publish a Windows arm64 archive. Visible Browser Lab publishes Windows arm64
packages as part of its six-target release contract, so a packaged Perfetto
sidecar would create two analyzer implementations with different deployment
and failure behavior.

The in-process analyzer preserves one implementation and one output contract
across all six release targets:

| Target | Analyzer |
| --- | --- |
| `aarch64-apple-darwin` | In-process Rust |
| `x86_64-apple-darwin` | In-process Rust |
| `aarch64-unknown-linux-musl` | In-process Rust |
| `x86_64-unknown-linux-musl` | In-process Rust |
| `aarch64-pc-windows-msvc` | In-process Rust |
| `x86_64-pc-windows-msvc` | In-process Rust |

Source: [Perfetto v56.1 release assets](https://github.com/google/perfetto/releases/tag/v56.1).

## Analysis Contract

`performance(operation: "analyze")` reads Chrome trace JSON from a
session-owned artifact. The analyzer returns bounded findings for:

- main-thread tasks longer than 50 milliseconds;
- script evaluation and execution time;
- style recalculation and layout time;
- paint and compositing time;
- network activity represented in the trace;
- the largest duration-bearing trace slices.

The optional `insight` parameter selects `overview`, `long_tasks`,
`script_execution`, `style_layout`, `paint`, or `network`. The analyzer accepts
trace artifacts up to 128 MiB, deserializes the trace JSON in process, and
returns at most 100 findings. Larger traces remain available through the
artifact API for export and external analysis.

## Distribution Contract

The analyzer is linked into the existing target-specific
`visible-browser-lab-mcp` binary. Release archives retain one executable, one
checksum and attestation path, and no first-run analyzer download. Unit
fixtures establish deterministic findings, and the six-target release build
establishes platform availability.
