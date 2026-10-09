#!/usr/bin/env bash
# V1 acceptance: WebDAV + SFTP with shared identities, isolation, inbox permissions,
# completed-upload hook delivery, safe rejection and restart persistence.
#
# Prerequisites (local only):
#   WORKER_COMMAND=record-hooks in .env (worker records hooks, no archival)
#   docker compose -f compose.yaml -f compose.dev.yaml up -d && scripts/sftpgo-bootstrap.sh
# Options: LARGE_BYTES=10737418240 runs the large local upload (sparse source file).
set -uo pipefail
. "$(dirname "$0")/lib.sh"

RUN="${RUN:-$(date +%H%M%S)$(random_hex 1)}"
A="alice-$RUN"
B="bob-$RUN"
LARGE_BYTES="${LARGE_BYTES:-0}"

not_created() { [[ "$1" != 201 && "$1" != 204 ]]; }

log "run $RUN: users $A and $B"
admin_login
create_user "$A"
# Bob gets a 64 MiB quota to exercise safe rejection.
create_user "$B" 67108864 67108864

echo "== host key"
check "SFTP host key fingerprint matches SFTPGo status API" trust_host_key

echo "== WebDAV basics ($A)"
head -c 65536 /dev/urandom > "$TMP/a.jpg"
check "unauthenticated PROPFIND is rejected" \
  test "$(curl -sS -o /dev/null -w '%{http_code}' -X PROPFIND -H 'Depth: 1' "$DAV/")" = 401
printf 'user = "%s:wrong-password"\n' "$A" > "$TMP/wrong.davcfg"
check "wrong password is rejected" \
  test "$(curl -sS -o /dev/null -w '%{http_code}' -K "$TMP/wrong.davcfg" -X PROPFIND -H 'Depth: 1' "$DAV/")" = 401
check "PROPFIND root" test "$(dav "$A" PROPFIND / -H 'Depth: 1')" = 207
check "MKCOL /photos" test "$(dav "$A" MKCOL /photos/)" = 201
check "PUT /photos/a.jpg" test "$(dav "$A" PUT /photos/a.jpg -T "$TMP/a.jpg")" = 201
get_matches() { curl -sS -K "$TMP/$1.davcfg" -o "$TMP/get.out" "$DAV$2" && [[ "$(sha_of "$TMP/get.out")" == "$(sha_of "$3")" ]]; }
check "GET returns identical bytes" get_matches "$A" /photos/a.jpg "$TMP/a.jpg"
check "overwrite of an existing file is denied (inbox policy)" \
  test "$(dav "$A" PUT /photos/a.jpg -T "$TMP/a.jpg")" = 403

echo "== SFTP basics ($A), same home and permissions"
head -c 32768 /dev/urandom > "$TMP/s.bin"
sftp_fails() { ! sftp_run "$@"; }
sftp_ls_has() { sftp_run "$1" "ls -1 $2" && grep -q "$3" "$TMP/sftp.out"; }
sftp_ls_lacks() { sftp_run "$1" "ls -1 $2" && ! grep -q "$3" "$TMP/sftp.out"; }
dav_dir_has() {
  local out
  out="$(curl -sS -K "$TMP/$1.davcfg" -X PROPFIND -H 'Depth: 1' -w '\n%{http_code}' "$DAV$2")" || return 1
  [[ "${out##*$'\n'}" == 207 && "$out" == *"$3"* ]]
}
check "SFTP mkdir + put" sftp_run "$A" "$(printf 'mkdir docs\nput %s docs/s.bin' "$TMP/s.bin")"
check "SFTP lists the file uploaded over WebDAV" sftp_ls_has "$A" photos a.jpg
check "WebDAV lists the file uploaded over SFTP" dav_dir_has "$A" /docs/ s.bin
check "SFTP overwrite of an existing file is denied" sftp_fails "$A" "put $TMP/s.bin docs/s.bin"
check "SFTP symlink creation is denied" sftp_fails "$A" "symlink docs/s.bin docs/link"
sftp_run "$A" "$(printf 'cd ..\ncd ..\npwd')"
check "SFTP cd .. stays at the virtual root" grep -q 'Remote working directory: /$' "$TMP/sftp.out"
sftp_run "$A" "rename docs/s.bin ../$B/stolen.bin"
check "SFTP rename to ../$B does not reach $B" sftp_ls_lacks "$B" . stolen.bin
check "SFTP rename inside the home works" sftp_run "$A" "rename docs/s.bin docs/renamed.bin"

echo "== cross-user isolation over WebDAV ($B attacks $A)"
check "control: filesystem search finds $A's own file" present_in "$A" a.jpg
for p in "/../$A/photos/a.jpg" "/%2e%2e/$A/photos/a.jpg" "/..%2f$A%2fphotos%2fa.jpg" \
  "/%2e%2e%2f$A%2fphotos%2fa.jpg" "/photos/..%5c..%5c$A/photos/a.jpg" \
  "/%ef%bc%8e%ef%bc%8e/$A/photos/a.jpg" "/%c0%ae%c0%ae/$A/photos/a.jpg" "/../../srv/sftpgo/data/$A/photos/a.jpg"; do
  code="$(curl -sS --path-as-is -o "$TMP/x" -w '%{http_code}' -K "$TMP/$B.davcfg" "$DAV$p")"
  leaked=no
  [[ "$code" == 200 && "$(sha_of "$TMP/x")" == "$(sha_of "$TMP/a.jpg")" ]] && leaked=yes
  check "GET $p does not leak $A's file (HTTP $code)" test "$leaked" = no
done
dav_root_lacks() {
  local out
  out="$(curl -sS -K "$TMP/$1.davcfg" -X PROPFIND -H 'Depth: 1' -w '\n%{http_code}' "$DAV/")" || return 1
  [[ "${out##*$'\n'}" == 207 && "$out" != *"$2"* ]]
}
check "$B's PROPFIND (207) does not show $A's folders" dav_root_lacks "$B" photos
code="$(dav "$A" MOVE /photos/a.jpg -H "Destination: $DAV/../$B/moved.jpg")"
check "MOVE with ../$B destination does not reach $B (HTTP $code)" absent_from "$B" moved.jpg
code="$(dav "$A" COPY /docs/renamed.bin -H "Destination: $DAV/%2e%2e/$B/copied.bin")"
check "COPY with encoded ../$B destination does not reach $B (HTTP $code)" absent_from "$B" copied.bin
code="$(dav "$A" COPY /docs/renamed.bin -H "Destination: http://evil.example/x.bin")"
check "COPY to a foreign host is refused (HTTP $code)" not_created "$code"
code="$(curl -sS --path-as-is -o /dev/null -w '%{http_code}' -K "$TMP/$B.davcfg" -X PUT -T "$TMP/a.jpg" "$DAV/../$A/planted.jpg")"
check "$B's PUT to /../$A does not land in $A's home (HTTP $code)" absent_from "$A" planted.jpg

echo "== interrupted uploads leave nothing behind"
head -c 52428800 /dev/urandom > "$TMP/big.bin"
curl -sS -o /dev/null -K "$TMP/$A.davcfg" --limit-rate 2M --max-time 3 -T "$TMP/big.bin" "$DAV/interrupted-dav.bin" 2>/dev/null
( sftp -q -l 16000 -i "$TMP/$A.key" -P "$SFTP_PORT" -o UserKnownHostsFile="$TMP/known_hosts" -o StrictHostKeyChecking=yes \
    -o BatchMode=yes -o IdentitiesOnly=yes -o LogLevel=ERROR "$A@127.0.0.1" <<< "put $TMP/big.bin interrupted-sftp.bin" >/dev/null 2>&1 &
  pid=$!; sleep 3; kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null )
sleep 3
tmp_dir_empty() { [[ -z "$("${COMPOSE[@]}" exec -T sftpgo ls -A /srv/sftpgo/uploads-tmp)" ]]; }
check "no partial file at the WebDAV target path" absent_from "$A" interrupted-dav.bin
check "no partial file at the SFTP target path" absent_from "$A" interrupted-sftp.bin
check "atomic-upload temp dir is empty" tmp_dir_empty

echo "== quota cutoff rejects oversized uploads safely ($B: 64 MiB)"
head -c 100000000 /dev/urandom > "$TMP/over.bin"
code="$(dav "$B" PUT /over.bin -T "$TMP/over.bin")"
check "oversized upload is rejected (HTTP $code)" not_created "$code"
check "no partial oversized file remains" absent_from "$B" over.bin

if [[ "$LARGE_BYTES" -gt 0 ]]; then
  echo "== large local upload ($LARGE_BYTES bytes, sparse source)"
  : > "$TMP/large.bin"
  truncate -s "$LARGE_BYTES" "$TMP/large.bin" 2>/dev/null || mkfile -n "$LARGE_BYTES" "$TMP/large.bin"
  start=$(date +%s)
  code="$(curl -sS -o /dev/null -w '%{http_code}' -K "$TMP/$A.davcfg" -T "$TMP/large.bin" "$DAV/large.bin")"
  took=$(( $(date +%s) - start ))
  check "large WebDAV upload succeeds (HTTP $code, ${took}s)" test "$code" = 201
  remote_size="$(curl -sS -K "$TMP/$A.davcfg" -X PROPFIND -H 'Depth: 0' "$DAV/large.bin" | grep -o '<D:getcontentlength>[0-9]*' | grep -o '[0-9]*$')"
  check "large file size matches ($remote_size)" test "$remote_size" = "$LARGE_BYTES"
  log "sftpgo memory: $(docker stats --no-stream --format '{{.MemUsage}}' kfgw-sftpgo-1)"
  check "large test file removed again via WebDAV DELETE" test "$(dav "$A" DELETE /large.bin)" = 204
fi

echo "== hook delivery (completed uploads only)"
sleep 2
recorded_hooks > "$TMP/hooks.jsonl"
fs_upload() { jq -c --arg u "$1" --arg p "$2" 'select(.path == "sftpgo/fs" and .body.action == "upload" and .body.username == $u and .body.virtual_path == $p)' "$TMP/hooks.jsonl"; }
check "one OK upload event for the WebDAV PUT" test "$(fs_upload "$A" /photos/a.jpg | jq -s '[.[] | select(.body.status == 1)] | length')" = 1
check "WebDAV event reports protocol DAV and the exact size" test "$(fs_upload "$A" /photos/a.jpg | jq -s -r '.[0].body.protocol + ":" + (.[0].body.file_size | tostring)')" = "DAV:65536"
check "one OK upload event for the SFTP put" test "$(fs_upload "$A" /docs/s.bin | jq -s '[.[] | select(.body.status == 1 and .body.protocol == "SFTP")] | length')" = 1
check "interrupted WebDAV upload never reports success" test "$(fs_upload "$A" /interrupted-dav.bin | jq -s '[.[] | select(.body.status == 1)] | length')" = 0
check "interrupted SFTP upload never reports success" test "$(fs_upload "$A" /interrupted-sftp.bin | jq -s '[.[] | select(.body.status == 1)] | length')" = 0
check "rejected oversized upload never reports success" test "$(fs_upload "$B" /over.bin | jq -s '[.[] | select(.body.status == 1)] | length')" = 0
check "provider add events for both users" test "$(jq -c --arg a "$A" --arg b "$B" 'select(.path == "sftpgo/provider" and (.query | test("action=add")) and (.query | test("object_name=(" + $a + "|" + $b + ")(&|$)")))' "$TMP/hooks.jsonl" | wc -l | tr -d ' ')" = 2
check "unauthenticated hook calls are refused by the receiver" \
  test "$(curl -sS -o /dev/null -w '%{http_code}' -X POST -d '{}' "$WORKER/hooks/sftpgo/fs")" = 401
cp "$TMP/hooks.jsonl" "${HOOK_CAPTURE:-/dev/null}" 2>/dev/null || true

echo "== restart persistence"
fps_before="$(cat "$TMP/host_fps")"
"${COMPOSE[@]}" restart sftpgo >/dev/null 2>&1
for _ in $(seq 1 30); do curl -fsS "$API/healthz" >/dev/null 2>&1 && break; sleep 2; done
admin_login
host_fps_now() { api GET /api/v2/status | jq -r '.ssh.host_keys[].fingerprint' | sort; }
check "$A still exists after restart" api GET "/api/v2/users/$A"
check "files survive a restart" dav_dir_has "$A" /docs/ renamed.bin
check "SSH host key is unchanged after restart" test "$(host_fps_now)" = "$fps_before"

summary
