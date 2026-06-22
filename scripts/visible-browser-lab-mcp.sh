#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

exec cargo run --quiet --manifest-path "$ROOT/Cargo.toml" --bin visible-browser-lab-mcp -- "$@"
