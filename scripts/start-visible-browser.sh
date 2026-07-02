#!/usr/bin/env bash
set -euo pipefail

CHROME_BIN="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
PROFILE_DIR="${VISIBLE_BROWSER_PROFILE_DIR:-$HOME/.cache/v0-visible-browser-profile}"
CDP_PORT="${VISIBLE_BROWSER_CDP_PORT:-9222}"
START_URL="${1:-http://localhost:3002/}"

mkdir -p "$PROFILE_DIR"

if curl -fsS "http://127.0.0.1:${CDP_PORT}/json/version" >/dev/null 2>&1; then
  echo "Visible browser CDP endpoint is already available on ${CDP_PORT}."
  exit 0
fi

open -na "$CHROME_BIN" --args \
  "--remote-debugging-port=${CDP_PORT}" \
  "--user-data-dir=${PROFILE_DIR}" \
  "$START_URL"

for _ in {1..60}; do
  if curl -fsS "http://127.0.0.1:${CDP_PORT}/json/version" >/dev/null 2>&1; then
    echo "Visible browser CDP endpoint is available on ${CDP_PORT}."
    exit 0
  fi
  sleep 0.25
done

echo "Timed out waiting for visible browser CDP endpoint on ${CDP_PORT}." >&2
exit 1
