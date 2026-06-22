<!-- exo:2 ulid:01kvrete0gprvgvsnrfm67dzhv -->

# RFC 2: Visible Browser Lab Plugin Publishing

## Summary

Visible Browser Lab should be installable and updateable through the agent surfaces where this browser tooling is used: Codex, Claude Code, and VS Code. Today the repository is a local plugin source tree. That is enough for development, but it does not give us a repeatable publishing path for installed copies or a CI check that generated plugin artifacts stay current.

This RFC adds a publishing track for the repository. It follows the local packaging pattern in `/Users/wycats/Code/vscode-ai-plugin`: keep source files on `main`, generate marketplace or installable outputs from CI, and publish those outputs to the expected branch or directory for each host.

This RFC is independent from the tab-isolation MCP facade. The facade can ship without publishing automation, and publishing automation can be implemented once the plugin surface is ready to distribute.

## Current State

The repository currently contains a Codex plugin manifest at `.codex-plugin/plugin.json`, local skill files under `skills/`, local MCP configuration, shell scripts, and the new Rust MCP facade crate.

The repository does not yet contain:

- a Claude Code plugin manifest or package shape;
- a VS Code plugin output shape;
- CI workflows for generated plugin artifacts;
- packaging scripts that can be run locally and in CI;
- a validation command that proves generated outputs match source.

## Proposal

Add explicit packaging support for three distribution surfaces:

- Codex plugin distribution.
- Claude Code plugin distribution.
- VS Code plugin distribution.

The source repository remains authoritative. Generated outputs must be reproducible from source, and CI should refresh the publication targets on pushes to `main` after validation passes.

Use `/Users/wycats/Code/vscode-ai-plugin` as the implementation reference for script names, workflow structure, branch publishing, and generated marketplace layout. Copy the pattern, not unrelated implementation details.

## Package Targets

### Codex

Codex packaging produces a `.codex-plugin` package rooted in this repository's Codex manifest and plugin files.

CI publishes a generated Codex marketplace root to the `codex-plugin` branch. That branch is a publication target, not the source of truth.

The generated output must include the plugin manifest, skills, scripts, MCP configuration, and built Rust binary or runnable launcher needed by installed Codex copies.

### Claude Code

Claude Code packaging produces a `.claude-plugin` package with the same user-facing plugin capability: visible browser lab tooling backed by the repository's MCP facade.

CI publishes a generated Claude Code marketplace root to the `cc-plugin` branch. That branch is a publication target, not the source of truth.

The Claude Code package should expose the same plugin name, description, skill guidance, and MCP facade entry point as the Codex package unless host-specific manifest fields require different names.

### VS Code

VS Code packaging produces installable source output under `dist/visible-browser-lab/` and updates a root `marketplace.json` entry for source installation.

The VS Code output must not become a hand-edited source tree. It is generated from repository source and checked by CI.

## CI Shape

Add workflows comparable to the `vscode-ai-plugin` publishing workflows:

- `publish-codex.yml` validates source, builds the Codex package, and publishes the generated marketplace branch.
- `publish-cc.yml` validates source, builds the Claude Code package, and publishes the generated marketplace branch.
- `publish-vscode.yml` validates source, builds the VS Code output, updates `dist/visible-browser-lab/` and `marketplace.json`, and commits the generated result when needed.

Workflows should run on pushes to `main`. They should be safe to rerun and should avoid creating commits when generated output is unchanged.

## Local Commands

Add local package scripts so CI and developers use the same entry points:

- `package-codex`
- `publish-codex`
- `package-cc`
- `publish-cc`
- `package-vscode`
- `publish-vscode`
- `validate`

The exact runner can be chosen during implementation. If Node scripts are used, prefer the established local pattern from `vscode-ai-plugin`. If Rust or shell helpers are used, keep the command names stable and document any host-specific prerequisites.

## Artifact Rules

Generated publication outputs must include only files required by the target host.

Generated outputs must not include local runtime state, Exo runtime files, Cargo `target/`, temporary logs, or machine-specific cache paths.

The packaged MCP entry point should prefer a built release binary when available. Development launcher scripts may remain in source, but installed packages should not require a source checkout unless the target host explicitly expects source installation.

## Non-Goals

This RFC does not define browser tab isolation, the broker protocol, CDP action semantics, or MCP tool behavior.

This RFC does not require publishing before the MCP facade is functionally complete.

This RFC does not require publishing secrets or marketplace credentials to be solved in the source tree. CI secret names and branch permissions can be finalized during implementation.

## Implementation Plan

1. Inventory the packaging and workflow pattern in `/Users/wycats/Code/vscode-ai-plugin`.
2. Add manifest/package shape for Claude Code and VS Code while preserving the existing Codex plugin manifest.
3. Add local package scripts for Codex, Claude Code, and VS Code.
4. Add validation that checks source manifests, generated package contents, and ignored runtime/build outputs.
5. Add CI workflows for the three publication targets.
6. Verify each package command locally.
7. Verify CI leaves no source-tree changes outside expected generated outputs.

## Test Plan

- Run the shared validation command from a clean checkout.
- Build the Codex package and inspect its generated marketplace root.
- Build the Claude Code package and inspect its generated marketplace root.
- Build the VS Code package and inspect `dist/visible-browser-lab/` plus root `marketplace.json`.
- Verify generated artifacts do not include `.exo/runtime/`, Cargo `target/`, logs, or machine-local cache paths.
- Verify workflow scripts can run idempotently without changing source files when outputs are current.

## Acceptance Criteria

Codex, Claude Code, and VS Code plugin artifacts can be generated from source with stable local commands.

CI keeps the publication targets current on pushes to `main`.

Installed or source-installed copies can be updated through the target host's expected plugin/update mechanism.

The tab-isolation RFC remains focused on the MCP facade and does not carry publishing requirements.
