#!/usr/bin/env bash
# Idempotently create the least-privileged SFTPGo admin used by katfile-worker
# (`kfgw-worker`: view_users + quota_scans, API-key login allowed) and store its API
# key in secrets/sftpgo_worker_api_key. Requires curl and jq on the host.
#
# Usage: scripts/sftpgo-bootstrap.sh
#        SFTPGO_API=http://127.0.0.1:18080 scripts/sftpgo-bootstrap.sh
set -euo pipefail

cd "$(dirname "$0")/.."
if [[ -f .env ]]; then set -a; . ./.env; set +a; fi
API="${SFTPGO_API:-http://127.0.0.1:${SFTPGO_ADMIN_HOST_PORT:-8080}}"
KEY_FILE=secrets/sftpgo_worker_api_key
WORKER_ADMIN=kfgw-worker

command -v jq >/dev/null || { echo "jq is required" >&2; exit 1; }

# Credentials live in a private temp dir as curl config files, never in argv (`ps`).
tmp="$(mktemp -d)"
chmod 700 "$tmp"
trap 'rm -rf "$tmp"' EXIT

echo "waiting for SFTPGo at $API ..."
for _ in $(seq 1 60); do
  curl -fsS "$API/healthz" >/dev/null 2>&1 && break
  sleep 2
done
curl -fsS "$API/healthz" >/dev/null

printf 'user = "admin:%s"\n' "$(cat secrets/sftpgo_admin_password)" > "$tmp/basic"
token="$(curl -fsS -K "$tmp/basic" "$API/api/v2/token" | jq -r .access_token)"
[[ -n "$token" && "$token" != "null" ]] || { echo "cannot obtain admin token" >&2; exit 1; }
printf 'header = "Authorization: Bearer %s"\n' "$token" > "$tmp/bearer"

api() { # api METHOD PATH [json-body-on-stdin]
  local method="$1" path="$2"
  if [[ "$method" == GET ]]; then
    curl -fsS -K "$tmp/bearer" "$API$path"
  else
    curl -fsS -K "$tmp/bearer" -X "$method" -H 'Content-Type: application/json' --data-binary @- "$API$path"
  fi
}

admin_json() {
  jq -n --arg pw "$(od -An -tx1 -N24 /dev/urandom | tr -d ' \n')" '{
    username: "kfgw-worker", status: 1, password: $pw,
    description: "katfile-worker service account (least privilege)",
    permissions: ["view_users", "quota_scans"],
    filters: {allow_api_key_auth: true}
  }'
}

if api GET "/api/v2/admins/$WORKER_ADMIN" >/dev/null 2>&1; then
  # Re-assert least privilege on every run; the existing password is kept.
  admin_json | jq 'del(.password)' | api PUT "/api/v2/admins/$WORKER_ADMIN" >/dev/null
  echo "verified admin $WORKER_ADMIN (permissions: view_users, quota_scans)"
else
  admin_json | api POST /api/v2/admins >/dev/null
  echo "created admin $WORKER_ADMIN (permissions: view_users, quota_scans)"
fi

key_works() {
  [[ -s "$KEY_FILE" ]] || return 1
  printf 'header = "X-SFTPGO-API-KEY: %s"\n' "$(cat "$KEY_FILE")" > "$tmp/apikey"
  curl -fsS -K "$tmp/apikey" "$API/api/v2/users?limit=1" >/dev/null 2>&1
}

if key_works; then
  echo "existing worker API key is valid"
else
  key="$(jq -n '{name: "katfile-worker", scope: 1, admin: "kfgw-worker", description: "katfile-worker service key"}' |
    api POST /api/v2/apikeys | jq -r .key)"
  [[ -n "$key" && "$key" != "null" ]] || { echo "API key creation failed" >&2; exit 1; }
  umask 022
  printf '%s' "$key" > "$KEY_FILE"
  chmod 644 "$KEY_FILE"
  key_works || { echo "new API key does not work" >&2; exit 1; }
  echo "stored new worker API key in $KEY_FILE"
fi
