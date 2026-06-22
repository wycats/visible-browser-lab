<!-- exo:2 ulid:01kvrete0gprvgvsnrfm67dzhv -->

# RFC 00002: Visible Browser Lab Plugin Publishing

## Summary

Visible Browser Lab should be installable and updateable through the agent surfaces where this browser tooling is used: Codex, Claude Code, and VS Code. The repository is currently a local plugin source tree. That is enough for development, but not enough for installed copies that should run without a local Rust toolchain or a local source checkout.

This RFC adds a trusted binary publishing track. CI builds the Rust MCP facade for supported platforms, packages each host plugin around a prebuilt binary, and publishes immutable GitHub Release assets with checksums and provenance. Installed machines consume release artifacts. They do not compile Rust and they do not need Node or pnpm at runtime.

This publishing track is independent from the tab-isolation tool implementation, except that Windows release binaries require RFC 00001's broker IPC to be cross-platform.

## Current State

The repository currently contains:

- a Codex plugin manifest at `.codex-plugin/plugin.json`;
- local skill files under `skills/`;
- local MCP configuration;
- shell scripts for development startup;
- a Rust MCP facade crate.

The repository does not yet contain:

- a Claude Code plugin manifest or package shape;
- a VS Code plugin package shape;
- Rust-native packaging commands;
- CI workflows for multi-platform release binaries;
- GitHub Release packaging, checksums, or artifact attestations;
- validation that proves generated packages contain only expected files.

## Proposal

Add Rust-native publishing support for three distribution surfaces:

- Codex plugin distribution.
- Claude Code plugin distribution.
- VS Code plugin distribution.

The source repository remains authoritative. CI builds platform binaries from protected source, assembles platform-specific plugin archives, and publishes those archives through GitHub Releases.

Do not publish through generated branches in v1. Do not force-push publication branches. Do not require Node or pnpm for packaging or runtime.

## Binary Targets

Build release binaries for:

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`
- `x86_64-pc-windows-msvc`
- `aarch64-pc-windows-msvc`

Windows support is part of the publishing scope. If the broker cannot compile on Windows, the publishing track is not complete.

## Package Targets

Each release includes host-specific plugin packages for each binary target:

- Codex package archives.
- Claude Code package archives.
- VS Code package archives.

Each package archive contains exactly one prebuilt `visible-browser-lab-mcp` binary for its target platform. The generated MCP configuration points directly at that packaged binary.

Release assets also include individual binary archives for debugging and manual installation.

## Rust-Native Tooling

Add an `xtask` crate under `xtask/` and expose these commands:

```text
cargo xtask validate
cargo xtask package --target <target-triple>
cargo xtask checksums
```

`cargo xtask validate` checks source manifests, package inputs, and ignored runtime/build outputs.

`cargo xtask package --target <target-triple>` packages host plugins around an existing release binary for that target. It fails if the target binary is missing.

`cargo xtask checksums` writes `SHA256SUMS` for generated release assets.

The xtask may use only Rust dependencies. Do not add Node, pnpm, or JavaScript packaging scripts.

## Package Contents

Generated packages include only files required by the target host:

- the appropriate plugin manifest;
- `skills/`;
- generated MCP configuration;
- the target platform's `visible-browser-lab-mcp` binary;
- minimal README or install notes if needed by the host.

Generated packages must not include:

- `.git/`;
- `.exo/runtime/`;
- Cargo `target/`;
- local runtime logs;
- local cache directories;
- `.DS_Store`;
- source files not needed by the installed plugin.

## Trusted Publication

GitHub Releases are the v1 publication source.

The release workflow attaches:

- platform-specific Codex plugin archives;
- platform-specific Claude Code plugin archives;
- platform-specific VS Code plugin archives;
- individual binary archives;
- `SHA256SUMS`;
- GitHub artifact attestations for release artifacts.

Real publication runs only from protected `v*` tags. Pull requests run the release workflow as a dry-run: they build binaries, package assets, write checksums, validate package contents, and upload workflow artifacts without creating a GitHub Release. Manual dry-runs remain available after the workflow exists on the default branch.

Repository protection is configured outside this repo: mandatory pull requests, required checks, resolved conversations, protected `main`, and protected `v*` tags.

## CI Shape

Add release-oriented workflows:

- A PR validation workflow builds and tests the crate on native Linux, macOS, and Windows runners.
- A release workflow builds every target binary, packages every host/target archive, writes checksums, generates artifact attestations, runs as a PR dry-run, and publishes a GitHub Release only for protected version tags.

Workflow permissions should be minimal:

```yaml
permissions:
  contents: write
  id-token: write
  attestations: write
```

Use GitHub-hosted runners for the supported OS/architecture matrix. If a public-preview runner label changes or is unavailable, the workflow should make that unsupported runner explicit rather than silently skipping the target.

## Non-Goals

This RFC does not define browser tab isolation, CDP action semantics, or MCP tool behavior.

This RFC does not publish generated plugin branches.

This RFC does not add Node, pnpm, or JavaScript tooling.

This RFC does not solve code signing or notarization. Unsigned binaries are acceptable for v1 unless host requirements force signing.

## Implementation Plan

1. Amend RFC 00001 so broker IPC is cross-platform local IPC instead of Unix sockets only.
2. Add an Exo phase and goal for this publishing track.
3. Replace direct Unix socket usage with a cross-platform IPC abstraction.
4. Add the `xtask` crate and the `cargo xtask` command surface.
5. Implement package generation for Codex, Claude Code, and VS Code.
6. Add checksum generation and release asset validation.
7. Add PR validation and release workflows.
8. Verify all local commands and CI-facing package layouts.

## Test Plan

Local validation:

- `cargo test`
- `cargo xtask validate`
- `cargo xtask package --target <current-host-target>`
- `cargo xtask checksums`
- `git diff --check`

CI validation:

- Build all six Rust targets.
- Run tests on native macOS, Linux, and Windows runners.
- Verify each package contains exactly one target binary plus required plugin files.
- Verify packages exclude `.git/`, `.exo/runtime/`, `target/`, logs, caches, and `.DS_Store`.
- Verify generated checksums match release assets.

Release validation:

- Run the release workflow in PR dry-run mode before merging the publishing change.
- Verify GitHub Release assets, checksums, and attestations are generated from the same commit.
- Verify Codex, Claude Code, and VS Code package archives can be unpacked and locate their packaged binary.

## Acceptance Criteria

Codex, Claude Code, and VS Code plugin archives can be generated from prebuilt Rust binaries with stable Rust-native commands.

Release packages do not require Rust, Node, pnpm, or a source checkout on the installed machine.

CI builds macOS, Linux, and Windows binaries, including ARM targets.

GitHub Releases contain package archives, binary archives, checksums, and artifact attestations.

No publication path relies on local force pushes or generated branch publishing.
