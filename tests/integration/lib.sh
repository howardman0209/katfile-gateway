#!/usr/bin/env bash
# Shared helpers for the shell-based integration tests. Secrets are passed to curl
# through private config files (never argv), and only synthetic data is uploaded.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || exit 1
if [[ -f .env ]]; then set -a; . ./.env; set +a; fi

API="http://127.0.0.1:${SFTPGO_ADMIN_HOST_PORT:-8080}"
DAV="http://127.0.0.1:${WEBDAV_DEV_PORT:-8081}"
WORKER="http://127.0.0.1:${WORKER_ADMIN_HOST_PORT:-8090}"
SFTP_PORT="${SFTP_PORT:-2022}"
COMPOSE=(docker compose)
if [[ -n "${KFGW_COMPOSE_PROJECT:-}" ]]; then COMPOSE+=(-p "$KFGW_COMPOSE_PROJECT"); fi
COMPOSE+=(-f compose.yaml -f compose.dev.yaml)
CURL_IMAGE="curlimages/curl:8.16.0@sha256:463eaf6072688fe96ac64fa623fe73e1dbe25d8ad6c34404a669ad3ce1f104b6"

TMP="$(mktemp -d)"
chmod 700 "$TMP"
trap 'rm -rf "$TMP"' EXIT

PASS=0
FAIL=0
FAILED_NAMES=()

log() { printf '\033[2m%s\033[0m %s\n' "$(date +%H:%M:%S)" "$*"; }

# check NAME COMMAND...: run COMMAND, record pass/fail.
check() {
  local name="$1"
  shift
  if "$@" >/dev/null; then
    PASS=$((PASS + 1))
    printf '  \033[32mPASS\033[0m %s\n' "$name"
  else
    FAIL=$((FAIL + 1))
    FAILED_NAMES+=("$name")
    printf '  \033[31mFAIL\033[0m %s\n' "$name"
  fi
}

summary() {
  echo
  echo "passed: $PASS  failed: $FAIL"
  for n in "${FAILED_NAMES[@]+"${FAILED_NAMES[@]}"}"; do echo "  failed: $n"; done
  [[ "$FAIL" -eq 0 ]]
}

random_hex() { od -An -tx1 -N"$1" /dev/urandom | tr -d ' \n'; }

# Admin bearer token for the SFTPGo REST API.
admin_login() {
  printf 'user = "admin:%s"\n' "$(cat secrets/sftpgo_admin_password)" > "$TMP/admin.basic"
  local token
  token="$(curl -fsS -K "$TMP/admin.basic" "$API/api/v2/token" | jq -r .access_token)"
  printf 'header = "Authorization: Bearer %s"\n' "$token" > "$TMP/admin.bearer"
}

# api METHOD PATH [JSON body on stdin] -> prints body; returns curl status.
api() {
  local method="$1" url_path="$2"
  if [[ "$method" == GET || "$method" == DELETE ]]; then
    curl -fsS -K "$TMP/admin.bearer" -X "$method" "$API$url_path"
  else
    curl -fsS -K "$TMP/admin.bearer" -X "$method" -H 'Content-Type: application/json' --data-binary @- "$API$url_path"
  fi
}

# create_user NAME [quota_bytes] [max_file_bytes]: user with inbox permissions,
# a random password (stored in $TMP/NAME.pass) and an ed25519 key ($TMP/NAME.key).
create_user() {
  local name="$1" quota="${2:-12884901888}" maxfile="${3:-11811160064}"
  local pass
  pass="$(random_hex 16)"
  printf '%s' "$pass" > "$TMP/$name.pass"
  printf 'user = "%s:%s"\n' "$name" "$pass" > "$TMP/$name.davcfg"
  # A re-created user gets a fresh key pair.
  if [[ -e "$TMP/$name.key" ]]; then rm "$TMP/$name.key"; rm "$TMP/$name.key.pub"; fi
  ssh-keygen -q -t ed25519 -N '' -C "$name@kfgw-test" -f "$TMP/$name.key"
  jq -n --arg u "$name" --arg p "$pass" --arg k "$(cat "$TMP/$name.key.pub")" \
    --argjson q "$quota" --argjson m "$maxfile" '{
      username: $u, status: 1, password: $p, public_keys: [$k],
      home_dir: ("/srv/sftpgo/data/" + $u),
      permissions: {"/": ["list", "download", "upload", "create_dirs", "rename", "delete"]},
      quota_size: $q, quota_files: 0,
      filters: {max_upload_file_size: $m}
    }' | api POST /api/v2/users >/dev/null
}

# dav USER METHOD PATH [extra curl args]: prints the HTTP status code.
dav() {
  local user="$1" method="$2" path="$3"
  shift 3
  curl -sS -o "$TMP/dav.body" -w '%{http_code}' -K "$TMP/$user.davcfg" -X "$method" "$@" "$DAV$path"
}

# Trust the SFTPGo host key only after matching its fingerprint with the admin API.
trust_host_key() {
  local api_fps scanned
  api_fps="$(api GET /api/v2/status | jq -r '.ssh.host_keys[].fingerprint' | sort)"
  ssh-keyscan -p "$SFTP_PORT" -t ed25519,ecdsa,rsa 127.0.0.1 2>/dev/null > "$TMP/known_hosts.scan"
  scanned="$(ssh-keygen -lf "$TMP/known_hosts.scan" -E sha256 | awk '{print $2}' | sort)"
  [[ -n "$api_fps" && "$api_fps" == "$scanned" ]] || return 1
  cp "$TMP/known_hosts.scan" "$TMP/known_hosts"
  echo "$api_fps" > "$TMP/host_fps"
}

# sftp_run USER "batch commands": runs sftp in batch mode with strict host checking.
sftp_run() {
  local user="$1" batch="$2"
  printf '%s\n' "$batch" > "$TMP/sftp.batch"
  sftp -q -b "$TMP/sftp.batch" -i "$TMP/$user.key" -P "$SFTP_PORT" \
    -o UserKnownHostsFile="$TMP/known_hosts" -o StrictHostKeyChecking=yes \
    -o BatchMode=yes -o IdentitiesOnly=yes -o LogLevel=ERROR "$user@127.0.0.1" > "$TMP/sftp.out" 2>&1
}

# Recorded hook calls (record-hooks mode) as JSON lines.
recorded_hooks() {
  printf 'header = "Authorization: Bearer %s"\n' "$(cat secrets/worker_webhook_secret)" > "$TMP/hook.bearer"
  curl -fsS -K "$TMP/hook.bearer" "$WORKER/recorded"
}

sha_of() { shasum -a 256 "$1" | awk '{print $1}'; }

# Staging paths (relative to /srv/sftpgo/data) of regular files named NAME, any user.
staged_paths() {
  "${COMPOSE[@]}" exec -T sftpgo find /srv/sftpgo/data -type f -name "$1" 2>/dev/null | sed 's#^/srv/sftpgo/data/##'
}
# present_in USER NAME: a file NAME exists somewhere in USER's home.
present_in() { staged_paths "$2" | grep -q "^$1/"; }
# absent_from USER NAME: no file NAME anywhere in USER's home.
absent_from() { ! present_in "$@"; }

# wadmin METHOD PATH [JSON body]: worker admin API with the admin token.
wadmin() {
  local method="$1" url_path="$2" body="${3:-}"
  printf 'header = "Authorization: Bearer %s"\n' "$(cat secrets/worker_admin_token)" > "$TMP/wadmin.bearer"
  if [[ -n "$body" ]]; then
    curl -sS -K "$TMP/wadmin.bearer" -X "$method" -H 'Content-Type: application/json' --data "$body" "$WORKER$url_path"
  else
    curl -sS -K "$TMP/wadmin.bearer" -X "$method" "$WORKER$url_path"
  fi
}

# katfile ENDPOINT FORM: independent KatFile API check from a throw-away container
# (public DNS; the key is read from the secret file inside the container).
katfile() {
  docker run --rm --dns 1.1.1.1 -v "$ROOT/secrets/katfile_api_key:/run/k:ro" "$CURL_IMAGE" \
    -sS -m 60 -X POST --data-urlencode key@/run/k --data "$2" "https://katfile.biz/api/$1"
}

# wait_for DESCRIPTION SECONDS COMMAND...: poll until COMMAND succeeds.
wait_for() {
  local what="$1" secs="$2"
  shift 2
  local end=$(( $(date +%s) + secs ))
  while (( $(date +%s) < end )); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 2
  done
  log "timed out waiting for: $what"
  return 1
}

# dav_dir_has USER DIR NAME: PROPFIND (207) of DIR lists NAME.
dav_dir_has() {
  local out
  out="$(curl -sS -K "$TMP/$1.davcfg" -X PROPFIND -H 'Depth: 1' -w '\n%{http_code}' "$DAV$2")" || return 1
  [[ "${out##*$'\n'}" == 207 && "$out" == *"$3"* ]]
}

# wadmin_internal PATH: worker admin GET from a container on the backend network
# (works while the worker is cut off from the egress network that publishes its port).
wadmin_internal() {
  local net="${KFGW_COMPOSE_PROJECT:-kfgw}_backend"
  printf 'header = "Authorization: Bearer %s"\n' "$(cat secrets/worker_admin_token)" > "$TMP/wadmin.bearer"
  docker run --rm --network "$net" -v "$TMP/wadmin.bearer:/run/h:ro" "$CURL_IMAGE" -sS -K /run/h "http://katfile-worker:8090$1"
}
