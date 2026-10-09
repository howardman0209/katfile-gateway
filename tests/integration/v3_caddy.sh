#!/usr/bin/env bash
# V3 checks for the HTTPS front end: TLS, redirect, WebDAV verbs through Caddy,
# declared-size admission before transfer, real client IPs and streaming.
#
# Prerequisites: full stack incl. Caddy and a serve-mode worker, e.g.
#   KFGW_COMPOSE_PROJECT=kfgw-e2e tests/integration/v3_caddy.sh
# Creates one test user whose small uploads are archived to the real KatFile account.
set -uo pipefail
. "$(dirname "$0")/lib.sh"

DOMAIN="${DAV_DOMAIN:-localhost}"
DAVS="https://$DOMAIN:${HTTPS_PORT:-443}"
HTTP_URL="http://$DOMAIN:${HTTP_PORT:-80}"
RUN="${RUN:-$(date +%H%M%S)$(random_hex 1)}"
U="dav-$RUN"

"${COMPOSE[@]}" cp caddy:/data/caddy/pki/authorities/local/root.crt "$TMP/caddy-root.crt" >/dev/null 2>&1
davs() { # davs METHOD PATH [curl args] -> HTTP status
  local method="$1" p="$2"
  shift 2
  curl -sS -o "$TMP/davs.body" -w '%{http_code}' --cacert "$TMP/caddy-root.crt" -K "$TMP/$U.davcfg" -X "$method" "$@" "$DAVS$p"
}

admin_login
create_user "$U"
log "user $U via $DAVS"

echo "== TLS and redirect"
check "Caddy's internal CA was exported" test -s "$TMP/caddy-root.crt"
redirect="$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' "$HTTP_URL/x")"
check "HTTP is redirected to HTTPS ($redirect)" bash -c "[[ '$redirect' == 30[18]\ https://* ]]"
code="$(curl -sS -o /dev/null -w '%{http_code}' --cacert "$TMP/caddy-root.crt" -X PROPFIND "$DAVS/")"
check "TLS handshake verifies; anonymous WebDAV gets 401 (HTTP $code)" test "$code" = 401

echo "== WebDAV verbs through Caddy"
head -c 8192 /dev/urandom > "$TMP/a.bin"
check "PROPFIND" test "$(davs PROPFIND / -H 'Depth: 1')" = 207
check "MKCOL" test "$(davs MKCOL /in/)" = 201
check "PUT (Content-Length)" test "$(davs PUT /in/a.bin -T "$TMP/a.bin")" = 201
check "GET returns the same bytes" bash -c "curl -sS --cacert '$TMP/caddy-root.crt' -K '$TMP/$U.davcfg' '$DAVS/in/a.bin' | cmp -s - '$TMP/a.bin'"
check "COPY keeps working behind the proxy" test "$(davs COPY /in/a.bin -H "Destination: $DAVS/in/copy.bin")" = 201
check "MOVE keeps working behind the proxy" test "$(davs MOVE /in/copy.bin -H "Destination: $DAVS/in/moved.bin")" = 201
lock="$(davs LOCK /in/a.bin -D "$TMP/lock.headers" -H 'Timeout: Second-60' -H 'Content-Type: application/xml' \
  --data '<?xml version="1.0"?><d:lockinfo xmlns:d="DAV:"><d:lockscope><d:exclusive/></d:lockscope><d:locktype><d:write/></d:locktype></d:lockinfo>')"
token="$(grep -i '^lock-token:' "$TMP/lock.headers" | sed -E 's/^[^:]*: *<?([^>]*)>?.*/\1/' | tr -d '\r')"
check "LOCK returns a lock token (HTTP $lock)" test "$lock" = 200 -a -n "$token"
check "UNLOCK" test "$(davs UNLOCK /in/a.bin -H "Lock-Token: <$token>")" = 204
head -c 4096 /dev/urandom > "$TMP/chunked.bin"
check "chunked PUT (no declared size) is admitted" test "$(davs PUT /in/chunked.bin -H 'Transfer-Encoding: chunked' --data-binary @"$TMP/chunked.bin")" = 201

echo "== admission before transfer"
free="$(wadmin GET /admin/status | jq -r .staging.free_bytes)"
truncate -s "$(( free * 2 ))" "$TMP/huge.bin" 2>/dev/null || mkfile -n "$(( free * 2 ))" "$TMP/huge.bin"
start=$(date +%s)
code="$(davs PUT /in/huge.bin -T "$TMP/huge.bin" --max-time 60)"
took=$(( $(date +%s) - start ))
check "upload larger than free space refused with 507 (${took}s)" test "$code" = 507 -a "$took" -lt 30
check "refusal names the reason" grep -q "Insufficient staging space" "$TMP/davs.body"
check "nothing was staged for the refused upload" absent_from "$U" huge.bin
tmp_empty() { [[ -z "$("${COMPOSE[@]}" exec -T sftpgo ls -A /srv/sftpgo/uploads-tmp)" ]]; }
check "no temp file left behind" tmp_empty
# Raw '&' and '+' in the URI: SFTPGo's pre-upload must claim the proxy's declaration.
head -c 2048 /dev/urandom > "$TMP/amp.bin"
check "PUT with '&' and '+' in the name" test "$(davs PUT '/in/a&b+c.bin' -T "$TMP/amp.bin")" = 201
no_reservations() { [[ "$(wadmin GET /admin/status | jq -r .staging.admitted_uploads_in_flight)" == 0 ]]; }
check "no admission reservation is left behind" wait_for "reservations released" 20 no_reservations

echo "== proxy hygiene"
code="$(curl -sS -o "$TMP/admin.html" -w '%{http_code}' --cacert "$TMP/caddy-root.crt" "$DAVS/web/admin")"
check "SFTPGo WebAdmin is not reachable through Caddy (HTTP $code)" bash -c "! grep -qi 'sftpgo' '$TMP/admin.html' || [[ '$code' == 401 ]]"
code="$(curl -sS -o /dev/null -w '%{http_code}' --cacert "$TMP/caddy-root.crt" -X POST "$DAVS/hooks/sftpgo/fs")"
check "worker hooks are not reachable through Caddy (HTTP $code)" test "$code" = 401 -o "$code" = 405
sleep 1
# Successful WebDAV requests are not access-logged at info level; the transfer log is.
remote="$("${COMPOSE[@]}" logs sftpgo 2>&1 | grep '"sender":"Upload"' | grep "$U/in/a.bin" | tail -1 | grep -o '"remote_addr":"[^"]*"' | cut -d'"' -f4)"
subnet="${KFGW_BACKEND_SUBNET:-172.30.200.0/24}"
check "SFTPGo logs the client address, not Caddy's ($remote)" bash -c "[[ -n '$remote' && '$remote' != ${subnet%.*/*}.* ]]"

echo "== streaming through Caddy"
truncate -s 1073741824 "$TMP/big.bin" 2>/dev/null || mkfile -n 1073741824 "$TMP/big.bin"
( peak=0; while :; do m="$(docker stats --no-stream --format '{{.MemUsage}}' "$("${COMPOSE[@]}" ps -q caddy)" | awk '{print $1}')"; echo "$m" >> "$TMP/caddy-mem"; sleep 1; done ) &
sampler=$!
code="$(davs PUT /in/big.bin -T "$TMP/big.bin")"
kill "$sampler" 2>/dev/null
wait "$sampler" 2>/dev/null
check "1 GiB upload through Caddy succeeds (HTTP $code)" test "$code" = 201
peak_mib="$(sed -E 's/MiB//; s/GiB/*1024/; s/KiB/\/1024/' "$TMP/caddy-mem" | bc 2>/dev/null | sort -n | tail -1 | cut -d. -f1)"
check "Caddy memory stays bounded (peak ${peak_mib:-?} MiB)" test "${peak_mib:-999}" -lt 100
check "1 GiB test file removed again" test "$(davs DELETE /in/big.bin)" = 204

echo "== archival of uploads made through Caddy"
# The MOVE target has no job of its own: the COPY created /in/copy.bin, which is archived.
for p in /in/a.bin /in/copy.bin /in/chunked.bin '/in/a&b+c.bin'; do
  st() { wadmin GET "/admin/jobs?limit=1000" | jq -r --arg u "$U" --arg p "$1" '[.jobs[] | select(.username == $u and .virtual_path == $p)][0].state // "none"'; }
  archived() { [[ "$(st "$1")" == archived ]]; }
  check "$p archived" wait_for "$p archived" 240 archived "$p"
done
summary
