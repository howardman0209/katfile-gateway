#!/usr/bin/env bash
# Run the opt-in KatFile compatibility probe in the worker image (same DNS, TLS and
# secrets as production). Read-only by default; pass --write to create a disposable
# kfgw-probe-<UTC> folder with a few KB of synthetic files (never deleted remotely).
#
# Usage: scripts/api-probe.sh [--write --parent-folder-id <id>] [--file-bytes N]
set -euo pipefail
cd "$(dirname "$0")/.."
overlay=()
[[ -f compose.dev.yaml && "${KFGW_PROBE_DEV:-1}" == 1 ]] && overlay=(-f compose.yaml -f compose.dev.yaml)
exec docker compose "${overlay[@]}" run --rm --no-deps -e RUST_LOG=warn katfile-worker probe "$@"
