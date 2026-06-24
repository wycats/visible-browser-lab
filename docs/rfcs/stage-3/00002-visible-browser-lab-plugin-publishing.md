<!-- exo:2 ulid:01kvrete0gprvgvsnrfm67dzhv -->

# RFC 00002: Visible Browser Lab Plugin Publishing

## Summary

Visible Browser Lab is installable and updateable through the agent surfaces where this browser tooling is used: Codex, Claude Code, and VS Code. The source tree remains the development authority, and release artifacts provide installed copies that run without a local Rust toolchain or a local source checkout.

This RFC defines the trusted binary publishing track. CI builds the Rust MCP facade for supported platforms, packages each host plugin around a prebuilt binary, and publishes immutable GitHub Release assets with checksums and provenance. Installed machines consume release artifacts. They do not compile Rust and they do not need Node or pnpm at runtime.

This publishing track is independent from the tab-isolation tool implementation, except that Windows release binaries require RFC 00001's broker IPC to be cross-platform.

## Implemented Contract

The repository publishes from a protected source tree through GitHub Actions. The implemented release path contains:

- a Codex plugin manifest at `.codex-plugin/plugin.json`;
- Claude Code and VS Code package manifests generated from the package target;
- local skill files under `skills/`;
- local MCP configuration;
- shell scripts for development startup;
- a Rust MCP facade crate.
- Rust-native packaging commands under `cargo xtask`;
- CI workflows for native tests, real-browser tests, multi-platform release binaries, package archives, checksums, and artifact attestations;
- a tag-triggered GitHub Release publication path.

The release interface contains three distribution surfaces:

- Codex plugin distribution.
- Claude Code plugin distribution.
- VS Code plugin distribution.

The source repository remains authoritative. CI builds platform binaries from protected source, assembles platform-specific plugin archives, and publishes those archives through GitHub Releases.

Publication uses release artifacts from CI. Packages are not published through generated branches, local force pushes, Node tooling, or pnpm tooling.

## Binary Targets

The release workflow builds release binaries for:

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

The `xtask` crate under `xtask/` exposes these commands:

```text
cargo xtask validate
cargo xtask package --target <target-triple>
cargo xtask checksums
```

`cargo xtask validate` checks source manifests, package inputs, and ignored runtime/build outputs.

`cargo xtask package --target <target-triple>` packages host plugins around an existing release binary for that target. It fails if the target binary is missing.

`cargo xtask checksums` writes `SHA256SUMS` for generated release assets.

The package path uses Rust dependencies only.

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

The `v0.1.0` release exercised this path and published 25 assets: 18 host plugin archives, 6 standalone binary archives, and `SHA256SUMS`. A release from current `main` packages the Stage 3 tab-lease facade from RFC 00001.

## CI Shape

The repository has release-oriented workflows:

- A PR validation workflow builds and tests the crate on native Linux, macOS, and Windows runners.
- A real-browser CI job runs the MCP facade against Chrome for Testing.
- A release workflow builds every target binary, packages every host/target archive, writes checksums, generates artifact attestations, runs as a PR dry-run, and publishes a GitHub Release only for protected version tags.

Workflow permissions are scoped to the release jobs:

```yaml
permissions:
  contents: write
  id-token: write
  attestations: write
```

Use GitHub-hosted runners for the supported OS/architecture matrix. If a public-preview runner label changes or is unavailable, the workflow should make that unsupported runner explicit rather than silently skipping the target.

## Contract Boundaries

RFC 00001 defines browser tab isolation, CDP action semantics, and MCP tool behavior.

This RFC defines GitHub Release publication for installable plugin and binary archives.

The package path is Rust-native.

Code signing and notarization are a separate release-hardening track. Unsigned binaries are acceptable for v1 unless a host requirement changes.

## Build Map

Available components:

1. Cross-platform broker IPC required for Windows release binaries.
2. `cargo xtask validate`, `cargo xtask package`, and `cargo xtask checksums`.
3. Package generation for Codex, Claude Code, and VS Code.
4. Checksum generation and release asset validation.
5. CI validation for native tests, real-browser tests, release binary builds, package archives, and artifact attestations.
6. Tag-triggered GitHub Release publication.

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
- Verify the release contains 25 assets: 18 host plugin archives, 6 standalone binary archives, and `SHA256SUMS`.
- Verify release checksums with `shasum -a 256 -c SHA256SUMS`.
- Verify artifact attestations with `gh attestation verify`.

## Acceptance Criteria

Codex, Claude Code, and VS Code plugin archives can be generated from prebuilt Rust binaries with stable Rust-native commands.

Release packages do not require Rust, Node, pnpm, or a source checkout on the installed machine.

CI builds macOS, Linux, and Windows binaries, including ARM targets.

GitHub Releases contain package archives, binary archives, checksums, and artifact attestations.

No publication path relies on local force pushes or generated branch publishing.

A release tag from current `main` publishes packages that contain the Stage 3 tab-lease facade.
