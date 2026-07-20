<!-- exo:6 ulid:01kwdah49gd9czpt9v1arw87eg -->

# RFC 6: Visible Browser Lab Binary-Resolved Host Packages

# Summary

Visible Browser Lab should add a binary-resolved host package contract for future releases. The current publishing contract produces self-contained host packages: every host package contains the platform-specific `visible-browser-lab-mcp` binary. That contract gives network-free first startup, and it expands each release into `25` assets: `18` host-and-target plugin archives, `6` standalone binary archives, and `SHA256SUMS`.

In this RFC, a **host package** is the release archive built for one host surface, such as Codex, Claude Code, or VS Code; the **plugin** is the installed Visible Browser Lab capability that host package registers with the host.

The binary-resolved contract separates host package distribution from binary distribution. A release publishes one package for each host surface, one binary archive for each supported target, and one checksum manifest. Host installation or first MCP startup resolves the current platform, downloads the matching binary, verifies it, caches it, and runs the verified binary from the cache.

Network access during installation or first startup is acceptable for this plugin class. Visible Browser Lab is used inside AI workflows that already require network access. The contract therefore optimizes for faster release pickup, fewer release assets, and a smaller host package surface while preserving verified binaries, deterministic cache behavior, and clear recovery when the first binary fetch cannot complete.

# Motivation

The current release workflow performs two separate kinds of work:

- build work: compile `visible-browser-lab-mcp` for six supported targets;
- package work: copy each target binary into each host-specific plugin archive, validate every archive, run an installed package smoke, generate checksums, and attach attestations.

The PR #24 release dry-run, GitHub Actions run `28476938154`, measured the current shape:

| Release step | Observed duration |
| --- | ---: |
| Fastest target build job | 3m 12s |
| Slowest target build job | 8m 04s |
| Package release assets job | 4m 38s |
| Package archive assembly within package job | 3m 28s |
| Installed Codex package smoke within package job | 19s |

The six target builds ran in parallel. After the builds completed, `Package release assets` produced and validated the `18` host-and-target packages plus binary archives.

The six binary builds remain part of the trusted release contract. The packaging fan-out is the part this RFC changes. A binary-resolved host package release reduces the host-package output from `18` platform-specific host packages to `3` host packages: Codex, Claude Code, and VS Code. The release still publishes the six standalone binary archives, so the release asset set becomes `10` assets: `3` host packages, `6` binary archives, and `SHA256SUMS`.

# Current Contract

RFC 00002 defines the current trusted binary publishing track. CI builds all six platform binaries, packages every host plugin around the matching binary, and publishes immutable GitHub Release assets with checksums and provenance.

RFC 00004 defines the installed runtime contract. Each host package contains one target-specific `visible-browser-lab-mcp` binary. The generated MCP configuration resolves that binary from the installed plugin root.

That contract remains valid until binary-resolved packages are implemented and validated. The existing self-contained packages are the compatibility and fallback release path.

# Proposed Contract

A binary-resolved host package contains host metadata, skills, MCP configuration, package metadata, and binary-resolution metadata. The package omits `visible-browser-lab-mcp`; the host lifecycle supplies the verified binary before MCP startup.

The release publishes:

- `visible-browser-lab-codex-<version>.zip`;
- `visible-browser-lab-claude-code-<version>.zip`;
- `visible-browser-lab-vscode-<version>.zip`;
- `visible-browser-lab-mcp-<version>-aarch64-apple-darwin.zip`;
- `visible-browser-lab-mcp-<version>-x86_64-apple-darwin.zip`;
- `visible-browser-lab-mcp-<version>-x86_64-unknown-linux-musl.zip`;
- `visible-browser-lab-mcp-<version>-aarch64-unknown-linux-musl.zip`;
- `visible-browser-lab-mcp-<version>-x86_64-pc-windows-msvc.zip`;
- `visible-browser-lab-mcp-<version>-aarch64-pc-windows-msvc.zip`;
- `SHA256SUMS`.

The host lifecycle resolves and verifies the binary before invoking the MCP server. The resolver:

1. determines the supported target triple for the current platform;
2. locates the release asset and expected checksum for that target;
3. downloads the binary archive when the verified cache does not already contain it;
4. verifies the archive and extracted binary before execution;
5. writes the binary through an atomic temporary path and publishes it into the cache only after verification succeeds;
6. runs the cached binary as the MCP server command.

Cache identity includes plugin name, plugin version, target triple, asset name, and SHA-256 digest. A cache hit is valid only when the cached binary matches the release metadata for the installed host package.

The cache directory is selected by the host lifecycle. When the host exposes a plugin cache root, the resolver stores binaries under that root at `visible-browser-lab/binaries/<plugin-version>/<target-triple>/<sha256>/`. When the host does not expose a cache root, the default cache root is the platform user-cache directory: `~/Library/Caches/visible-browser-lab/binaries` on macOS, `${XDG_CACHE_HOME:-~/.cache}/visible-browser-lab/binaries` on Linux, and `%LOCALAPPDATA%\\visible-browser-lab\\Cache\\binaries` on Windows. A temporary bundled resolver may support `VISIBLE_BROWSER_LAB_BINARY_CACHE_DIR` as an explicit override; a host-managed resolver uses the host's plugin-cache configuration surface for the same purpose.

# Network and Recovery Contract

First startup may require network access.

Offline operation is supported after one successful verified binary download. If a verified cached binary exists for the installed version and target, the host uses it without requiring network access.

If no verified cached binary exists and the network is unavailable, startup fails with a setup error that names:

- plugin name and version;
- resolved target triple;
- expected release asset;
- cache directory;
- the fact that a first verified binary download is required.

If a download starts and fails, the resolver leaves any previous verified binary in place and never executes a partial or unverified candidate.

If checksum verification fails, the resolver deletes the candidate file and reports a supply-chain verification error that names the expected digest, observed digest, release asset, and version.

If attestation verification is supported by the host environment and verification fails, the resolver deletes the candidate file and reports a provenance verification error. When attestation verification is not available, checksum verification remains mandatory and the result records that local attestation verification was unavailable.

If the release asset for the resolved target is missing, the resolver reports an unsupported-target error that includes the resolved operating system, architecture, target triple, plugin version, and release URL.

Concurrent startup for the same package, target, and cache key uses one download and one verification path. Other callers wait for the result and then use the verified cache entry.

# Trust Model

CI still builds all supported target binaries from the protected source commit. GitHub Releases remain the publication source. Releases continue to publish `SHA256SUMS` and artifact attestations.

The binary-resolved host package trusts release metadata, the checksum manifest, and verified artifact identity. The resolver executes only a binary that matches the installed package version and target metadata.

The host package remains free of Rust, Node, pnpm, and source checkout requirements. The installed machine runs a CI-built runtime binary.

# Host Lifecycle Requirement

Current Codex plugin documentation describes marketplace installation, plugin manifests, bundled MCP server configuration, and MCP commands. It does not describe a host install hook that can download and verify a platform binary before MCP startup.

The preferred implementation requires a trusted host lifecycle step: binary resolution runs before the MCP command is invoked. That lifecycle can be implemented by the host installer, by a host-owned plugin runtime resolver, or by an equivalent trusted host surface.

A bundled resolver executable remains available as an implementation fallback. A fallback resolver would still need to be platform-specific or depend on an installed runtime, which reduces the benefit of platform-neutral host packages. The preferred design keeps the resolver in the host lifecycle where the host already manages plugin installation, cache locations, and trust policy.

# Release Workflow Shape

The release workflow continues to run native tests, real-browser tests, and all six target builds.

The package job changes from host-and-target package fan-out to host package assembly plus binary metadata validation. It validates:

- all six binary archives exist;
- `SHA256SUMS` covers every release asset;
- host package metadata maps every supported target to one binary asset and checksum;
- host package manifests expose the binary-resolved package kind;
- no host package contains `visible-browser-lab-mcp`;
- installed package smoke can exercise cache-miss download and cache-hit offline startup.

# Acceptance Criteria

A binary-resolved release publishes `10` assets for the existing host and target set: `3` host packages, `6` binary archives, and `SHA256SUMS`.

A fresh install with network access downloads, verifies, caches, and runs the correct target binary without requiring Rust, Node, pnpm, or a source checkout.

A subsequent startup with the same installed version and a verified cached binary succeeds without network access.

First startup without network and without a verified cache returns a setup error with enough information for the user or host to retry after network access is restored.

Interrupted downloads, checksum failures, attestation failures, missing assets, unsupported targets, and concurrent startups are covered by deterministic tests.

The existing self-contained package path remains available until the host lifecycle resolver is implemented and validated across Codex, Claude Code, and VS Code.
