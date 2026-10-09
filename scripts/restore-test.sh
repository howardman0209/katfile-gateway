#!/usr/bin/env bash
# Backup/restore drill for the worker database (plan V2 acceptance).
#
# 1. Online backup with `katfile-worker backup-db` (SQLite VACUUM INTO, safe while running).
# 2. Integrity check and comparison with the live /admin/status counts.
# 3. Restore drill: stop the worker, put the backup in place, start, compare again.
#
# The drill replaces the database of the selected compose project; run it against a
# test project (default: kfgw-e2e), never blindly against production.
# Note: the database alone cannot recover uploads whose staged files were lost; back up
# the staging volume too if full recovery of unarchived uploads is required.
set -uo pipefail
export KFGW_COMPOSE_PROJECT="${KFGW_COMPOSE_PROJECT:-kfgw-e2e}"
. "$(dirname "$0")/../tests/integration/lib.sh"

command -v sqlite3 >/dev/null || { echo "sqlite3 is required" >&2; exit 1; }
STAMP="$(date +%Y%m%dT%H%M%S)"
IN_CTR="/var/lib/katfile-worker/backup-$STAMP.db"
OUT="$TMP/backup-$STAMP.db"

counts_live() { wadmin GET /admin/status | jq -c '[.jobs | to_entries[] | select(.value.count > 0) | {(.key): .value.count}] | add'; }
counts_file() { sqlite3 "$1" "SELECT json_group_object(state, n) FROM (SELECT state, COUNT(*) n FROM upload_jobs GROUP BY state ORDER BY state)"; }

echo "== online backup"
check "backup-db succeeds while the worker runs" "${COMPOSE[@]}" exec -T katfile-worker katfile-worker backup-db "$IN_CTR"
check "backup copied out" "${COMPOSE[@]}" cp "katfile-worker:$IN_CTR" "$OUT"
check "backup passes PRAGMA integrity_check" test "$(sqlite3 "$OUT" 'PRAGMA integrity_check')" = ok
live="$(counts_live)"
file="$(counts_file "$OUT")"
log "live: $live  backup: $file"
check "backup holds the same job counts as the live service" test "$(jq -cS . <<< "$live")" = "$(jq -cS . <<< "$file")"

echo "== restore drill (project $KFGW_COMPOSE_PROJECT)"
"${COMPOSE[@]}" stop katfile-worker >/dev/null 2>&1
# Restore = copy the backup over the database file; the stale WAL/SHM must go, or SQLite
# would replay newer pages onto the restored copy.
check "backup restored over the database" \
  "${COMPOSE[@]}" run --rm --no-deps --entrypoint /bin/sh -v "$OUT:/restore.db:ro" staging-init -c \
  "cp /restore.db /var/lib/katfile-worker/worker.db && rm -f /var/lib/katfile-worker/worker.db-wal && rm -f /var/lib/katfile-worker/worker.db-shm && chown 1000:1000 /var/lib/katfile-worker/worker.db"
"${COMPOSE[@]}" start katfile-worker >/dev/null 2>&1
wait_for "worker back" 90 curl -fsS "$WORKER/healthz"
check "restored service reports the same job counts" test "$(jq -cS . <<< "$(counts_live)")" = "$(jq -cS . <<< "$file")"
summary
