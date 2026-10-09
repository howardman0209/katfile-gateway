#!/usr/bin/env bash
# Read-only capability check. Never print the API key or signed download URLs.
# Requires curl with DNS-over-HTTPS support and jq. No Docker is required.
# Exit 2 means downloads are unavailable or need further verification.
set -euo pipefail
cd "$(dirname "$0")/.."

command -v jq >/dev/null || { echo "ERROR: jq is required." >&2; exit 1; }
[[ -s secrets/katfile_api_key ]] || { echo "ERROR: KatFile secret is missing." >&2; exit 1; }

api() {
  local endpoint="$1"
  shift
  # The development Mac's system resolver filters KatFile domains.
  curl --silent --show-error --connect-timeout 15 --max-time 60 \
    --doh-url https://1.1.1.1/dns-query \
    --data-urlencode key@secrets/katfile_api_key \
    "$@" "https://katfile.biz/api/$endpoint"
}

check_status() {
  local label="$1" response="$2"
  if ! jq -e '(.status | tostring) == "200"' >/dev/null <<<"$response"; then
    printf 'FAIL: %s (API status %s)\n' "$label" "$(jq -r '.status' <<<"$response")"
    exit 1
  fi
  printf 'PASS: %s\n' "$label"
}

response=$(api account/info)
check_status "account authentication" "$response"

response=$(api folder/list --data-urlencode fld_id=0)
check_status "root folder listing" "$response"

response=$(api file/list --data-urlencode page=1 --data-urlencode per_page=100)
check_status "account file listing" "$response"
code=$(jq -r '[.result.files[]? | select((.size | tonumber) > 0)][0].file_code // empty' <<<"$response")
if [[ -z "$code" ]]; then
  echo "INCONCLUSIVE: no non-empty file in the first 100 entries; upload a test file and retry."
  exit 2
fi

response=$(api file/direct_link --data-urlencode "file_code=$code")
if jq -e '(.status | tostring) == "403" and .msg == "Not enabled"' >/dev/null <<<"$response"; then
  echo "BLOCKED: file/direct_link returned 403 Not enabled."
  echo "Ask KatFile to enable API downloads for this account, then rerun this probe."
  exit 2
fi
check_status "direct-link request" "$response"
if ! jq -e '.result.url | strings | startswith("https://")' >/dev/null <<<"$response"; then
  echo "INCONCLUSIVE: no HTTPS download URL in result.url."
  exit 2
fi
echo "PASS: an HTTPS download URL is available (not displayed)."
echo "NEXT: verify downloaded content against a known original and test HTTP Range."
echo "This probe does not download content or prove the full read milestone."
