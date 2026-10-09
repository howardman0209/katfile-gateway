#!/usr/bin/env bash
# End-to-end smoke test against the REAL KatFile account (plan section 11).
#
# Creates persistent remote test data (one folder per test user under
# KATFILE_USERS_PARENT_FOLDER_ID plus a few KB of synthetic files). Nothing is
# deleted remotely. Requires an approved disposable parent folder in .env:
#   KATFILE_USERS_PARENT_FOLDER_ID=<folder id>   (0 is refused unless SMOKE_ALLOW_ROOT_PARENT=1)
#
# Usage (local dev stack, separate compose project with its own volumes):
#   KFGW_COMPOSE_PROJECT=kfgw-e2e scripts/smoke-test.sh
set -uo pipefail
. "$(dirname "$0")/../tests/integration/lib.sh"

PARENT="${KATFILE_USERS_PARENT_FOLDER_ID:-0}"
if [[ "$PARENT" == 0 && "${SMOKE_ALLOW_ROOT_PARENT:-0}" != 1 ]]; then
  echo "refusing to create test folders in the KatFile account root; set KATFILE_USERS_PARENT_FOLDER_ID" >&2
  exit 2
fi

RUN="${RUN:-$(date +%m%d%H%M)$(random_hex 1)}"
A="alice-$RUN"
B="bob-$RUN"
C="carol-$RUN"
log "smoke run $RUN under KatFile folder $PARENT (users $A, $B, $C)"

job_state() { wadmin GET "/admin/jobs?limit=1000" | jq -r --arg u "$1" --arg p "$2" '[.jobs[] | select(.username == $u and .virtual_path == $p)][0].state // "none"'; }
job_field() { wadmin GET "/admin/jobs?limit=1000" | jq -r --arg u "$1" --arg p "$2" --arg f "$3" '[.jobs[] | select(.username == $u and .virtual_path == $p)][0][$f] // empty'; }
principal() { wadmin GET /admin/principals | jq -c --arg u "$1" '[.principals[] | select(.username == $u)] | sort_by(.created_at_ms)'; }
live_principal_field() { principal "$1" | jq -r --arg f "$2" '[.[] | select(.state != "deleted")][0][$f] // empty'; }
is_state() { [[ "$(job_state "$1" "$2")" == "$3" ]]; }
principal_active() { [[ "$(live_principal_field "$1" state)" == active ]]; }
# Remote check through KatFile's own API: file code listed in the expected folder.
remote_has() { katfile file/list "fld_id=$1&per_page=200" | jq -e --arg c "$2" --argjson f "$1" '.result.files[] | select(.file_code == $c and .fld_id == $f)' >/dev/null; }
child_folders_named() { katfile folder/list "fld_id=$1" | jq -r --arg n "$2" '[.result.folders[] | select(.name == $n)] | length'; }
not_found_over_dav() { [[ "$(dav "$1" PROPFIND "$2" -H 'Depth: 0')" == 404 ]]; }

admin_login

echo "== 1-2. create users in SFTPGo; the worker provisions their KatFile folders"
create_user "$A"
create_user "$B"
check "$A provisioned" wait_for "$A active" 120 principal_active "$A"
check "$B provisioned" wait_for "$B active" 120 principal_active "$B"
FA="$(live_principal_field "$A" remote_folder_id)"
FB="$(live_principal_field "$B" remote_folder_id)"
log "folders: $A=$FA $B=$FB"
check "distinct KatFile root folders" test -n "$FA" -a -n "$FB" -a "$FA" != "$FB"
check "exactly one remote folder named $A" test "$(child_folders_named "$PARENT" "$A")" = 1

echo "== 3-5. upload over WebDAV and SFTP per user; archived and verified remotely"
trust_host_key
for u in "$A" "$B"; do
  head -c 4096 /dev/urandom > "$TMP/$u-dav.bin"
  head -c 3072 /dev/urandom > "$TMP/$u-sftp.bin"
  check "$u WebDAV upload" test "$(dav "$u" MKCOL /photos/)" = 201 -a "$(dav "$u" PUT /photos/dav.bin -T "$TMP/$u-dav.bin")" = 201
  check "$u SFTP upload" sftp_run "$u" "$(printf 'mkdir docs\nput %s docs/sftp.bin' "$TMP/$u-sftp.bin")"
done
for u in "$A" "$B"; do
  for p in /photos/dav.bin /docs/sftp.bin; do
    check "$u$p archived" wait_for "$u$p archived" 180 is_state "$u" "$p" archived
  done
done
for u in "$A" "$B"; do
  root="$(live_principal_field "$u" remote_folder_id)"
  for p in /photos/dav.bin /docs/sftp.bin; do
    code="$(job_field "$u" "$p" remote_file_code)"
    folder="$(job_field "$u" "$p" remote_folder_id)"
    check "$u$p listed by KatFile in its folder ($code in $folder)" remote_has "$folder" "$code"
    parent_of="$(katfile folder/list "fld_id=$root" | jq -r --argjson f "$folder" '[.result.folders[] | select(.fld_id == $f)] | length')"
    check "$u$p folder is a child of $u's root" test "$parent_of" = 1
  done
done
check "staged copies are still visible during retention" dav_dir_has "$A" /photos/ dav.bin

echo "== 9. cross-user access fails over both protocols"
code="$(curl -sS --path-as-is -o /dev/null -w '%{http_code}' -K "$TMP/$B.davcfg" "$DAV/../$A/photos/dav.bin")"
check "WebDAV traversal from $B to $A refused (HTTP $code)" test "$code" != 200
sftp_run "$B" "get ../$A/photos/dav.bin $TMP/stolen.bin"
check "SFTP traversal from $B to $A finds nothing" test ! -s "$TMP/stolen.bin"

echo "== 6. KatFile outage: retry with local copy retained"
EGRESS="${KFGW_COMPOSE_PROJECT:-kfgw}_egress"
WORKER_CTR="$("${COMPOSE[@]}" ps -q katfile-worker)"
docker network disconnect "$EGRESS" "$WORKER_CTR"
head -c 2048 /dev/urandom > "$TMP/outage.bin"
check "upload during outage accepted by SFTPGo" test "$(dav "$A" PUT /outage.bin -T "$TMP/outage.bin")" = 201
retrying() {
  [[ "$(wadmin_internal "/admin/jobs?state=retry_waiting" | jq -r --arg u "$A" '[.jobs[] | select(.username == $u and .virtual_path == "/outage.bin")] | length')" == 1 ]]
}
check "job waits for retry while KatFile is unreachable" wait_for "retry_waiting" 120 retrying
check "local copy retained during outage" present_in "$A" outage.bin
docker network connect "$EGRESS" "$WORKER_CTR"
check "archived after KatFile is reachable again" wait_for "outage archived" 240 is_state "$A" /outage.bin archived

echo "== 7. restart containers: jobs and settings persist"
before="$(wadmin GET /admin/status | jq -c '{a: .jobs.archived.count, p: .principals.active}')"
"${COMPOSE[@]}" restart sftpgo katfile-worker >/dev/null 2>&1
wait_for "worker back" 90 curl -fsS "$WORKER/healthz"
wait_for "sftpgo back" 90 curl -fsS "$API/healthz"
admin_login
after="$(wadmin GET /admin/status | jq -c '{a: .jobs.archived.count, p: .principals.active}')"
check "status unchanged across restart ($before)" test "$before" = "$after"

echo "== 8. retention expiry: verified copy removed safely"
id="$(job_field "$A" /photos/dav.bin id)"
wadmin POST "/admin/jobs/$id/expire-retention" >/dev/null
wadmin POST /admin/cleanup >/dev/null
check "job cleaned" is_state "$A" /photos/dav.bin cleaned
check "staged copy removed from WebDAV view" not_found_over_dav "$A" /photos/dav.bin
check "remote copy still listed" remote_has "$(job_field "$A" /photos/dav.bin remote_folder_id)" "$(job_field "$A" /photos/dav.bin remote_file_code)"

echo "== 10. delete $A and re-create the username"
api DELETE "/api/v2/users/$A" >/dev/null
deleted() { principal "$A" | jq -e 'length == 1 and .[0].state == "deleted"' >/dev/null; }
check "principal tombstoned" wait_for "$A deleted" 60 deleted
home_gone() { [[ -z "$("${COMPOSE[@]}" exec -T sftpgo ls -A /srv/sftpgo/data | grep -x "$A")" ]]; }
check "home moved out of SFTPGo" wait_for "home quarantined" 60 home_gone
create_user "$A"
new_active() { principal "$A" | jq -e 'map(select(.state == "active")) | length == 1' >/dev/null; }
check "re-created $A provisioned as a new principal" wait_for "new $A active" 120 new_active
NEW_FA="$(live_principal_field "$A" remote_folder_id)"
check "new folder differs from the old one ($FA -> $NEW_FA)" test -n "$NEW_FA" -a "$NEW_FA" != "$FA"
check "new folder is named $A-2" test "$(katfile folder/list "fld_id=$PARENT" | jq -r --argjson f "$NEW_FA" '.result.folders[] | select(.fld_id == $f) | .name')" = "$A-2"
check "re-created user starts with an empty home" not_found_over_dav "$A" /docs/sftp.bin
check "old folder still holds the old archive" remote_has "$(job_field "$A" /docs/sftp.bin remote_folder_id)" "$(job_field "$A" /docs/sftp.bin remote_file_code)"

echo "== 11. worker down while a user is created: reconciled exactly once"
"${COMPOSE[@]}" stop katfile-worker >/dev/null 2>&1
create_user "$C"
sleep 5
"${COMPOSE[@]}" start katfile-worker >/dev/null 2>&1
wait_for "worker back" 90 curl -fsS "$WORKER/healthz"
check "$C provisioned after restart" wait_for "$C active" 120 principal_active "$C"
check "exactly one remote folder named $C" test "$(child_folders_named "$PARENT" "$C")" = 1

echo "== final status"
wadmin GET /admin/status | jq '{jobs: (.jobs | with_entries(select(.value.count > 0))), attention, principals, staging: (.staging | {free, snapshot, disk_pressure})}'
summary
