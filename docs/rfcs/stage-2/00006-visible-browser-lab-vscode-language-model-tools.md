<!-- exo:6 ulid:01jz58vblvscodelmtool0006 -->

# RFC 00006: Visible Browser Lab VS Code Language Model Tools

## Summary

Visible Browser Lab will ship a real VS Code extension that contributes native
language model tools while preserving the browser semantics already exposed
through MCP.

The browser runtime remains Rust-owned. Tab leases, Chrome lifecycle, semantic
snapshots, artifact ownership, diagnostics, help responses, and structured
browser errors continue to flow through the broker and the shared agent surface.
MCP and VS Code become two presentations of the same contract. MCP presents the
contract to portable agent hosts. VS Code presents it through the editor's
language model tool API, where tool picker metadata, invocation messages,
confirmation prompts, workspace context, cancellation, output channels, and
Marketplace installation can make the same browser operations feel native.

The release path keeps the existing six-target Rust binary matrix. The VS Code
work adds one extension-host build and then combines that build with each
target-specific runtime binary. It does not introduce a second browser runtime
or a second matrix of browser builds.

## Motivation

Visible Browser Lab has two jobs that are easy to conflate. It must define a
safe browser contract for agents, and it must make that contract usable from the
host where an agent is running.

The browser contract is already intentionally host-independent. A session owns
tabs. A tab contains document-scoped element references. The broker validates
the session and tab before it resolves a reference or invokes Chrome. Errors use
stable codes and recovery guidance. These rules should not change because the
caller is Codex, Claude Code, VS Code, or a test harness.

The host experience is different. VS Code knows the active workspace. It can
show a progress message before a tool runs. It can ask for confirmation with
copy that names the tab or target about to be affected. It can cancel a running
tool from the chat request. It can surface configuration, output channels, and
Marketplace installation status in the editor instead of relying on a generic
MCP server entry. Those capabilities are not browser semantics; they are host
ergonomics. The design should use them without letting them fork the contract.

This is the core constraint of the RFC: VS Code should become a better front
door, not a second implementation. A snapshot obtained through VS Code and a
snapshot obtained through MCP must describe the same page state. A click must
use the same ownership and actionability checks. A lease error must carry the
same code and recovery instruction. If a future maintainer changes the browser
surface, the MCP package and the VS Code extension should move together or fail
the same validation.

## Background

The repository currently builds one runtime binary, `visible-browser-lab-mcp`.
When started without a subcommand it serves MCP over stdio. When started as the
broker it owns the persistent Chrome connection, tab leases, diagnostics, and
artifacts. The catalog and schemas live in `agent-surface-contract`, and the
MCP server adapts those definitions into `rmcp` types.

RFC 00002 established trusted binary releases: CI builds macOS, Linux, and
Windows binaries for both x86_64 and aarch64, packages host archives around the
prebuilt binary, writes checksums, and publishes GitHub Release assets. Its
current VS Code package is a placeholder in the Claude plugin format. It proves
that the release machinery can produce an archive with a VS Code label, but it
does not produce a VS Code extension and it cannot use the language model tool
API.

This RFC replaces that placeholder with an extension whose native VS Code tool
surface is generated from the same catalog as the MCP server.

## Design Principles

The implementation should be organized around three boundaries.

The first boundary is the browser surface. It is independent of MCP and VS
Code. It knows the production tools, schemas, help behavior, broker startup,
request forwarding, and structured result shape. It does not know about
`CallToolResult`, `LanguageModelToolResult`, VS Code workspace folders, or
Codex metadata.

The second boundary is the host adapter. It translates host context into the
shared surface and translates the shared result back into the host's tool API.
The MCP adapter extracts MCP metadata and returns MCP result types. The VS Code
adapter extracts workspace and settings context, participates in VS Code's
tool-invocation UI, and returns VS Code language model result parts.

The third boundary is packaging. Release packages must prove that the static
metadata a host sees matches the runtime catalog the binary will execute. A
VS Code `package.json` contribution that advertises a stale schema is as much a
contract violation as an MCP server that advertises a tool the broker cannot
run.

These boundaries let the project add VS Code-specific affordances without
making the browser harder to reason about.

## Detailed Design

### The shared surface

The Rust crate exposes a host-neutral `VisibleBrowserLabSurface`. It constructs
the production catalog from `agent-surface-contract`, handles `help`, starts or
connects to the broker, forwards browser tool calls, and returns a small result
enum containing either a structured success value or a structured browser error.

The MCP server is reduced to an adapter over this type. It converts
`ToolDefinition` into `rmcp::model::Tool`, extracts host metadata such as the
Codex `sandboxCwd`, calls the shared surface, and converts the result into an
MCP `CallToolResult`. All browser behavior stays outside the MCP module.

The same binary also exposes a non-MCP command surface for host adapters that do
not want to speak MCP to themselves:

```text
visible-browser-lab-mcp surface catalog
visible-browser-lab-mcp surface call <tool> [--workspace-root <dir>]
```

`surface catalog` returns the server instructions and tool definitions as JSON.
`surface call` reads a JSON object from stdin and writes one JSON response:

```json
{ "ok": true, "result": { } }
```

or:

```json
{ "ok": false, "error": { "code": "...", "message": "...", "recovery": "..." } }
```

The CLI boundary is deliberately boring. It is easy for the VS Code extension
to spawn, easy to test, and keeps the extension from importing MCP protocol
types. The broker remains persistent, so the expensive Chrome connection and
lease registry are not recreated for each tool invocation. If process startup
overhead becomes visible, the same boundary can grow a persistent JSON-line
transport without changing the browser contract or the VS Code tool names.

### The VS Code extension

The extension contributes one language model tool for each production browser
tool. The contributed names use a `visible_browser_lab_` prefix so they do not
collide with tools from other extensions. For example, the browser method
`snapshot` is contributed as `visible_browser_lab_snapshot` and receives a
prompt reference name such as `vbl_snapshot`.

The extension's `package.json` is generated from the shared catalog. Each
language model tool contribution receives the shared title, the shared input
schema, a model description derived from the shared tool description, an
activation event, and prompt-reference metadata. This generation step matters
because VS Code language model tools are statically contributed. Runtime catalog
discovery alone cannot make tools visible in the tool picker or available for
agent mode.

At activation time the extension reads its own contributions and registers a
generic `LanguageModelTool` implementation for each one. The implementation
maps the contributed name back to the browser method, writes the invocation
input to `visible-browser-lab-mcp surface call`, and returns the result as a
`LanguageModelToolResult`. Structured browser errors are surfaced as tool
errors whose message includes the error code, message, and recovery field. That
keeps the language model's retry path aligned with MCP.

The generic implementation is also where VS Code gets to be VS Code. It can
provide `prepareInvocation` messages that say what is about to happen, such as
capturing a snapshot, navigating an owned tab, or clicking a referenced element.
For operations that close, release, claim, or focus a tab, it can ask for
confirmation with copy derived from the actual invocation input. It can pass the
active workspace folder directly as `workspace_root`, observe the
`CancellationToken`, and route runtime configuration through VS Code settings.

None of these affordances change what the broker does. They change how clearly
the editor explains and supervises the operation.

### Binary resolution and configuration

The extension resolves the runtime binary from a small, explicit search path.
A user or developer setting may point at a custom `visible-browser-lab-mcp`
binary. Otherwise the extension uses the packaged binary under `bin/`, with the
Windows executable suffix when needed.

Runtime settings mirror the existing environment contract instead of inventing
extension-only configuration. The extension can set the state directory, Chrome
path, CDP endpoint, and CDP port by passing through the existing
`VISIBLE_BROWSER_LAB_STATE_DIR`, `VISIBLE_BROWSER_LAB_CHROME_PATH`,
`VISIBLE_BROWSER_CDP_ENDPOINT`, and `VISIBLE_BROWSER_CDP_PORT` variables. The
managed Chrome default remains the default when those settings are absent.

### Release packaging

The release workflow continues to build the Rust runtime for these targets:

```text
aarch64-apple-darwin
x86_64-apple-darwin
x86_64-unknown-linux-musl
aarch64-unknown-linux-musl
x86_64-pc-windows-msvc
aarch64-pc-windows-msvc
```

The VS Code work adds one extension-host build. Packaging then combines that
single extension build with each target-specific Rust binary. The output is a
real VS Code extension package per supported target, not a Claude-format archive
with a VS Code label.

Package validation compares the generated VS Code language model tool
contributions with the shared catalog. A release fails if the VS Code package
omits a production tool, advertises a schema different from the runtime catalog,
duplicates a prompt reference name, or omits the activation event for a
registered tool. This check is the guardrail that keeps static VS Code metadata
and runtime behavior in lockstep.

## User Experience

For a VS Code user, installing Visible Browser Lab should look like installing
an ordinary extension. The extension contributes named browser tools that can be
enabled, disabled, and referenced in chat. The first browser task starts or
reuses the managed Chrome profile unless the user configured an external CDP
endpoint. When an agent invokes a tool, VS Code shows a tool-specific progress
message. When the operation affects visible browser ownership or focus, VS Code
can ask for confirmation before the broker runs it.

The agent-facing workflow remains the one taught by RFC 00005: start a session,
work only with owned tab IDs, inspect pages through snapshots, act through
snapshot references when possible, and use diagnostics or artifacts when the
page result differs from intent. VS Code improves how that workflow is surfaced;
it does not teach a separate browser model.

## Drawbacks

VS Code requires static language model tool contributions. This means the
package build must generate or validate `package.json` before the extension is
loaded. A dynamic runtime catalog is useful for validation and development, but
it cannot replace the contribution point.

The first adapter invokes the runtime binary once per tool call. That is simple
and keeps the extension implementation small, but it is not the lowest-overhead
transport. The design accepts this because the broker is persistent and owns the
expensive Chrome state. If the spawn cost becomes user-visible, the CLI surface
can evolve into a long-lived subprocess protocol behind the same VS Code tool
contract.

Cancellation in the first adapter is host-side: ending the wrapper process
reports cancellation to VS Code, while a browser action the broker has already
dispatched runs to completion. Lease ownership bounds the effect to the
session's own tabs, and the structured result of a retried call reflects the
true page state. A broker-level cancel channel that aborts in-flight Chrome
operations is follow-up work on the same `surface` boundary.

The repository gains a small amount of Node and TypeScript tooling for the VS
Code extension. That is a real cost for a project whose release path has been
intentionally Rust-native. The constraint is therefore explicit: Node builds the
extension host code once; Rust still builds the browser runtime, and the release
workflow reuses the existing runtime matrix.

Target-specific VS Code packages also require care. A universal package with
all six binaries would be simpler to reason about, but it would increase install
size and abandon the release model already used by the other hosts. The
target-specific approach is the better fit unless Marketplace constraints force
a different packaging shape.

## Alternatives

The first alternative is to use the existing MCP server inside VS Code. That is
portable and already works for browser semantics, but it leaves VS Code's native
tool affordances unused. The editor cannot provide the same static tool picker
metadata, input-specific confirmation copy, workspace-root handling,
cancellation behavior, or Marketplace-native tool contribution surface through
an opaque MCP entry.

The second alternative is to implement the browser runtime directly in the
extension. That would give the extension full control over VS Code integration,
but it would fork the hardest part of the project: Chrome lifecycle, tab leases,
element references, diagnostics, artifacts, and recovery semantics. This RFC
rejects that path. VS Code integration is not a reason to duplicate the browser
contract.

The third alternative is a single generic VS Code tool such as
`visible_browser_lab_call`. That would avoid manifest generation, but it would
erase the tool-selection metadata that makes language model tools useful. The
model would see one broad escape hatch instead of a catalog of precise browser
operations, and VS Code could not tailor invocation or confirmation copy by
operation. This would recreate the ergonomics problem the RFC is meant to solve.

## Implementation Strategy

The implementation can proceed in layers that each leave the repository in a
coherent state.

The first layer extracts the shared Rust surface and keeps MCP behavior stable.
The MCP server should become a thin adapter, and the new `surface catalog` and
`surface call` commands should prove that VS Code can reach the same runtime
without speaking MCP.

The second layer introduces the VS Code extension as a generic adapter. It does
not need bespoke TypeScript for every browser tool. It should register the
generated language model tools, resolve the packaged or configured binary, pass
workspace context to `surface call`, observe cancellation, and return structured
JSON result parts.

The third layer moves packaging from a placeholder VS Code archive to real
extension artifacts. This layer generates or validates the static
`languageModelTools` contribution metadata from the shared catalog, builds the
extension host code once, and packages it with each target-specific runtime
binary.

The final layer adds installation validation. A smoke should load the VS Code
extension package, verify that the generated tools are registered, check the
catalog against the packaged binary, and invoke at least a non-browser-starting
tool such as `help`. A fuller smoke can later exercise managed Chrome from the
installed extension, but the first validation should focus on package identity,
tool registration, and the adapter boundary.

## Stage 2 Criteria

This RFC is implementation-ready when the following contract is precise enough
to build and validate:

- MCP and VS Code use one Rust surface for catalog, help, broker calls, and
  structured errors.
- The MCP server remains behaviorally unchanged after becoming a host adapter.
- The VS Code extension contributes one native language model tool per
  production browser tool.
- VS Code tool contributions are generated from, or validated against, the
  shared runtime catalog.
- VS Code calls pass editor workspace context to the shared surface, observe
  cancellation, and provide useful invocation and confirmation messages.
- The release workflow reuses the existing Rust binary matrix and performs only
  one additional extension-host build.
- Release assets include real VS Code extension artifacts instead of the
  current Claude-format placeholder package.