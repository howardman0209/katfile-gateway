# KatFile API compatibility (V0 evidence)

Verified against the **live katfile.biz service** on 2026-10-09 with the operator's
premium account, first with `curl` (exploratory) and then with the Rust client via
`katfile-worker probe`. Generic XFileSharing Pro documentation was only used as a
starting point; every rule below is what the real service did.

Remote test data created during verification (never deleted, per operator approval):

| Item | Value |
|---|---|
| Disposable folder | `kfgw-probe-20261009T0102Z` (`fld_id 211628`) at the account root, plus sub-folders created by later probe runs |
| Synthetic files | a handful of 1–5 KB random files inside that folder |
| Anonymous file | one 4 KB synthetic file uploaded with a deliberately **invalid** session (see "Anonymous upload"); not owned by the account, cannot be removed via API |

## Transport

| Topic | Finding | Gateway rule |
|---|---|---|
| Hostname | `katfile.com` answers `301` → `https://katfile.biz/` | Configure `KATFILE_API_BASE_URL=https://katfile.biz/` directly; redirects are refused. |
| HTTP status | API errors are returned as **HTTP 200** with an application `status` (e.g. `{"status":400,"msg":"Invalid key"}`); unknown endpoints return HTTP 404 HTML | Always check JSON `status`; HTML/non-JSON is `Malformed`. |
| Key transport | `GET ?key=…` **and** `POST` form body (`application/x-www-form-urlencoded`) both work for every endpoint used (account/info, folder/list, folder/create, upload/server, file/list, file/info, file/set_folder); an `X-Api-Key` header does not | The client sends **every call as POST with the key in the body**, so the key never appears in URLs, proxy logs or error messages. |
| TLS | API (Cloudflare) and upload hosts (Apache 2.4.62, HTTP/1.1) serve valid HTTPS | `https_only`, HTTP/1.1, no redirects. |
| Upload host | `upload/server` returned `https://sNNNN.katfile.biz/cgi-bin/upload.cgi` (s5000–s5085 observed), a different host per call | Allow-list regex `^s[0-9]{1,6}\.katfile\.biz$`, port 443, exact path, no query; DNS answers must be public IPs (SSRF guard). |
| Local DNS | The development Mac's resolver (`100.64.0.2`, VPN content filter) returns NXDOMAIN for `katfile.biz` and `*.katfile.biz`; public resolvers work | Local probes run in Docker with `--dns 1.1.1.1`. Production DNS must be checked during V3 preflight. |

## Endpoints

| Operation | Verified call | Result shape / quirks |
|---|---|---|
| Account | `account/info` | `result: {storage_used: "180427" (string), storage_left: 879609302040373 (number), premium_expire, balance, email}` — types differ per field. The gateway never stores the e-mail. |
| List folders | `folder/list fld_id=N` | `result: {folders:[{fld_id:int, name, code}], files:[{fld_id, file_code, name, link, uploaded}]}`. **Paging parameters are ignored** (all children returned). Unknown/foreign folder → `{"status":403,"msg":"Folder not exist or not yours"}`. |
| Create folder | `folder/create parent_id=P name=N` | `result: {fld_id: "211628"}` — **string** id (lists return numbers). **Not idempotent: the same name can be created repeatedly** (two `dup-test` folders were created). Names are **case-sensitive** (`CaseTest` ≠ `casetest`). |
| Upload server | `upload/server` | `{result: "<cgi url>", sess_id: <61 chars>}`; a **new session per call**; a session can be reused for several uploads. |
| Upload | `POST <cgi url>` multipart: `sess_id`, `utype=prem`, `file_0` | Reply `[{"file_code":"79cpmm7q3pmg","file_status":"OK"}]` (12 lowercase alphanumerics). Both **known `Content-Length`** and **`Transfer-Encoding: chunked`** stored byte-exact sizes for small files. A `fld_id` form field is **ignored**: files always land in the account root first. |
| Empty file | upload of 0 bytes | `[{"file_status":"null filesize or wrong file path","file_code":"undef"}]` → permanent rejection; the gateway never uploads empty files. |
| Assign folder | `file/set_folder file_code=C fld_id=F` | Idempotent. Unknown folder → `403 "Your account don't have such folder"`. **Unknown or foreign file codes still return 200 OK** (silent no-op). |
| File list | `file/list fld_id=F name=… page=… per_page=…` | Account scoped; paging works (`results_total`, `results`). `name` is a **substring** filter. **`fld_id=0` means "no folder filter"** (lists files in sub-folders too). Entries include `fld_id`, `size`, `file_code`, `name`, `uploaded`. |
| File info | `file/info file_code=C` | `result: [{filecode, name, size, status, uploaded, downloads}]`; unknown code → item `status: 404` with top-level 200. **Not account scoped**: it also describes anonymous files. |
| Direct link | `file/direct_link` | `{"status":403,"msg":"Not enabled"}` for this account → content cannot be downloaded back for hash comparison. |

## Names

* Names are stored as raw bytes and rendered by the JSON API as Latin-1, so UTF-8
  comes back as mojibake: `相片 ü 2026` → `"ç\u009b¸ç\u0089\u0087 Ã¼ 2026"`. Re-encoding
  each char as one byte and decoding UTF-8 restores the original exactly
  (`katfile_api::names::demangle`). Raw UTF-8 in the multipart `filename="…"` works.
* `"` is stored as `&quote;`; `/` is accepted literally inside a name; whitespace is
  preserved; names longer than **128 bytes are truncated**.
* The gateway therefore sends sanitized names only (`" ' < > & \ /` and control
  characters → `_`, ≤ 128 bytes, file extensions preserved) so the stored value is
  predictable, and compares demangled names.

## Anonymous upload (security-relevant)

An upload with an **invalid `sess_id` is still accepted** and returns `file_status: OK`
with a valid-looking `file_code`, but the file is **not owned by the account**: it is
absent from `file/list`, `file/set_folder` silently ignores it, yet `file/info` still
describes it. If a session ever expired mid-upload, a private file could become an
anonymous public file.

Gateway rules:

1. Request a fresh session immediately before every upload.
2. Never treat `file/info` or a `set_folder` "OK" as success. A job reaches `Archived`
   only after the **account-scoped** `file/list fld_id=<user folder>` returns the exact
   `file_code` with the expected size (`KatFileClient::find_file_in_folder`).
3. Anything else ends in `NeedsReview` with category `remote_ownership_unverified`,
   because the gateway cannot delete files it does not own.
4. Session lifetime is unknown; a near-10 GB upload must be tested in V3 before the
   gateway is trusted with large private videos.

## Upload outcome classification

| Situation | Class | Worker action |
|---|---|---|
| Failure before the whole multipart body was handed to the connection | Transient | Retry with backoff (nothing can have been stored). |
| `file_status` other than `OK` | Permanent | `Failed`, keep local copy. |
| HTTP 4xx before completion | Permanent | `Failed`, keep local copy. |
| Connection loss, 5xx, non-JSON reply or timeout **after** the body was sent | Ambiguous | Reconcile by account-wide exact-name search; adopt exactly one candidate, otherwise `NeedsReview`. Never blind re-upload. |
| OK reply with a valid `file_code` | Success | Persist `file_code` before any further step, then assign + verify ownership. |

## Streaming and limits

* The client streams the file in 256 KiB chunks with an exact `Content-Length`
  (default) or chunked encoding (`KATFILE_UPLOAD_MODE=chunked`), hashing on the fly.
* Local measurement against the mock (same process for client and server):
  2 GiB upload → peak RSS growth **3 MiB**; 10 GiB → **3 MiB**; 1 GiB chunked → 2 MiB.
* Upload stalls (no body progress for `KATFILE_UPLOAD_STALL_SECS`, default 120 s) abort
  the attempt as transient; the reply wait after the body is `600 s + 1 s per 100 MiB`.
* **Not yet verified on the live service:** files larger than a few KB, the maximum
  accepted file size, session lifetime during long uploads, rate limits (none were
  observed at ~1 request/second). These are V3 acceptance items and require explicit
  operator approval because they create large remote data.

## Rust probe evidence

`katfile-worker probe --write --parent-folder-id 211628` run in Docker
(`--dns 1.1.1.1`, read-only root filesystem, all capabilities dropped) at
2026-10-09T01:30:23Z: **15 passed, 0 failed**. Every call used POST
bodies; the report and logs were scanned afterwards and contain neither the API key
nor upload session IDs.

| Step | Result | Detail |
|---|---|---|
| `account_info` | pass | storage_used=Some(192865) premium_expire=Some("2036-08-03 20:49:57") |
| `folder_list_root` | pass | 2 folders, 2 files at root |
| `folder_list_unknown` | pass | status 403: Folder not exist or not yours |
| `file_list_page` | pass | results_total=7 page_len=5 |
| `upload_server` | pass | host=s5034.katfile.biz (allowlisted, https) server_time=Some("2026-10-09 05:00:27") |
| `file_info_unknown` | pass | unknown file reported as not found |
| `invalid_key_rejected` | pass | KatFile rejected the API key |
| `folder_create` | pass | kfgw-probe-20261009T0130Z -> fld_id 211639 |
| `folder_create_listed` | pass | listed under parent 211628: true |
| `folder_unicode_roundtrip` | pass | raw="nested ç\u{9b}¸ç\u{89}\u{87} Ã¼" demangled="nested 相片 ü" |
| `folder_create_not_idempotent` | pass | two creates -> 211641 and 211642 |
| `upload_configured_mode` | pass | kfgw-probe-configured.bin -> a2dlcx8i3901 verified in folder 211639; sha256 match=true |
| `upload_chunked` | pass | kfgw-probe-chunked.bin -> xl86i1sb8tow verified in folder 211639; sha256 match=true |
| `upload_fixed_length` | pass | kfgw-probe-fixed.bin -> ie8i1imhlbf1 verified in folder 211639; sha256 match=true |
| `upload_unicode_name` | pass | kfgw-probe 相片 ü.txt -> muy34g2ueb09 verified in folder 211639; sha256 match=true |

Created remotely: folders 211639, 211640, 211641, 211642; files a2dlcx8i3901, xl86i1sb8tow, ie8i1imhlbf1, muy34g2ueb09
(all inside the disposable probe folder).

"sha256 match" compares the hash computed while streaming with the hash of the
synthetic file written locally; the provider cannot return content for comparison
(`direct_link` is disabled), so remote verification is size + name + folder ownership.
