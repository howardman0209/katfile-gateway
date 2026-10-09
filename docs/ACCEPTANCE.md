# Acceptance status (V0–V3)

Legend: **done** = verified with the evidence listed; **local** = verified locally, still
to be repeated on the VPS; **pending** = needs the VPS and/or operator approval.
Environment for local evidence: macOS (Apple Silicon), Docker Engine 29.8.2 (Docker Desktop), SFTPGo
2.7.6, Caddy 2.11.7, Rust 1.99, live katfile.biz premium account (test data confined to
the disposable folder `kfgw-probe-20261009T0102Z`, id 211628).

## V0 — KatFile API and streaming feasibility

| Criterion | Status | Evidence |
|---|---|---|
| Small test file arrives in the designated folder with correct metadata | done | `katfile-worker probe --write` 15/15 (docs/API_COMPATIBILITY.md): 4 uploads (fixed length, chunked, Unicode name) verified by account-scoped listing in folder 211639 with exact size and name |
| API key stays out of logs, Git and CI | done | POST-only API calls; reqwest errors stripped of URLs; repository, probe output, build log and image layers scanned for the key: 0 matches; `.gitignore`/`.dockerignore`; automated tests use the mock only |
| Streaming memory bounded for a large synthetic file | done | `large_upload` test: 2 GiB → +3 MiB RSS, 10 GiB → +3 MiB, 1 GiB chunked → +2 MiB (client and mock in one process) |
| Typed, sanitized errors | done | `KatFileError` with classes Transient/Permanent/Auth/Ambiguous; redaction unit tests; 15 mock integration tests |
| 10 GB / HTTPS limitations documented | done | Upload host is HTTPS (allow-listed); 10 GB on the live service, max file size and session lifetime are **not verified** (API_COMPATIBILITY.md) |

## V1 — Local SFTPGo WebDAV + SFTP and isolation

| Criterion | Status | Evidence |
|---|---|---|
| Alice and Bob upload/list in isolated homes over WebDAV and SFTP with the same permissions | done | `tests/integration/v1_protocols.sh` 49/49 (with the 10 GiB upload), re-run 49/49 after the V3 network split (1 GiB) |
| Cross-user traversal, MOVE, COPY fail | done | 8 encoded/Unicode traversal GETs, MOVE/COPY with `../` and foreign-host destinations, PUT into another home, SFTP rename/get across homes; verified on the filesystem with a positive control |
| 10 GB upload succeeds locally, otherwise rejected safely | done | 10 GiB WebDAV PUT in 55 s; quota cut-off rejects and deletes the partial file; finding: SFTPGo at 256 MiB was OOM-killed in 2 of 3 10 GiB uploads → limit 512 MiB, then 4×10 GiB without OOM |
| Event delivered only on completed upload | done | recorded hooks: exactly one `status=1` event per completed upload; interrupted WebDAV/SFTP uploads and the quota rejection never report success |
| Restart preserves users and files | done | users, files and SSH host-key fingerprints unchanged after `docker compose restart sftpgo` |
| Mobile clients | pending | no device in this environment; part of V3 on-device tests |

## V2 — Rust worker integration

| Criterion | Status | Evidence |
|---|---|---|
| Creating users in SFTPGo provisions distinct KatFile root folders automatically | done | smoke steps 1–2 (real KatFile): two folders, IDs persisted, exactly one folder per name; unit e2e `new_users_get_distinct_root_folders_once` |
| SFTPGo WebAdmin/API is the only user portal | done | provider hooks + reconciliation; worker API is diagnostics/decisions only |
| Hook replay and worker downtime recover without duplicate folders | done | smoke step 11 (worker stopped while a user was created → one folder); e2e `uncertain_folder_create_is_resolved_by_listing_not_retried`, `worker_downtime_during_user_creation_is_recovered_once` |
| Failed provisioning visible; files stay queued until mapping succeeds | done | e2e `failed_provisioning_keeps_uploads_queued_until_approved` (status shows `failed`, approval completes the archive) |
| Disable/delete block new archives, remote content preserved, reuse cannot inherit | done | smoke step 10 (delete + re-create → new folder `<name>-2`, empty home, old archive intact); e2e disabled/deleted/re-created scenarios incl. operator rescue into the old folder |
| Alice's files only under Alice's root, Bob's under Bob's, both protocols | done | smoke steps 3–5: every file code checked with KatFile's own `file/list` in a sub-folder of the owner's root |
| Restart while uploading recovers without silent loss | done | e2e `restart_mid_upload_recovers_without_silent_loss` (before/after body-sent markers) |
| Folder assignment failure retries without re-upload | done | e2e `folder_assignment_failure_retries_without_reupload` (upload.cgi called once) |
| Duplicate events do not duplicate jobs | done | e2e `duplicate_events_do_not_duplicate_jobs` |
| Ambiguous outcome flagged, not re-uploaded | done | e2e `ambiguous_upload_is_reconciled_or_flagged_never_reuploaded`, `anonymous_storage_is_never_reported_as_archived` |
| Failed/unfinished files never auto-deleted | done | e2e `failed_and_unfinished_files_are_never_auto_deleted` (even under disk pressure) |
| SQLite backup/restore tested | done | `scripts/restore-test.sh` 6/6 (online `VACUUM INTO`, integrity check, restore drill) |
| KatFile outage → retry with local copy retained | done | smoke step 6 (worker cut off from egress: `retry_waiting`, file kept, archived after reconnect) |

## V3 — Deployment, WireGuard coexistence, operations

| Criterion | Status | Evidence / next step |
|---|---|---|
| TLS valid, mobile WebDAV connects; SFTP host key validated | local | `tests/integration/v3_caddy.sh`: TLS with Caddy's internal CA, HTTP→HTTPS redirect, all WebDAV verbs incl. LOCK/UNLOCK, MOVE/COPY through the proxy; SFTP fingerprint check in V1. Mobile apps and public certificate: pending |
| PUT success clearly distinguished from archive completion | done | README "Read this first"; `/admin/status` `semantics`; job states |
| Real test files verified in the correct remote folders | local | smoke test (KB-sized synthetic files). Documents/photos/videos from devices: pending |
| No WebAdmin / worker endpoints exposed to the internet | local | admin ports bound to 127.0.0.1; through Caddy `/web/admin` and hook paths are plain WebDAV paths (401). External port scan of the VPS: pending |
| Worker keeps bounded RAM and disk | local | worker ~10 MiB RSS, Caddy peak 19–24 MiB for 1 GiB; declared-size admission refuses oversize uploads with 507 in < 1 s and stages nothing; VPS measurement: pending |
| Cleanup only removes verified archives after retention | done | e2e retention/pressure tests; smoke step 8 |
| Backup restore and restart recovery tested | done | restore-test 6/6; smoke step 7 |
| WireGuard works before/during/after deployment | pending | DEPLOYMENT.md §0, §3, §7 |
| Gateway restarts do not break WireGuard | pending | DEPLOYMENT.md §7 |
| Fits 1 GB RAM / 25 GB SSD; 10 GB admission rejects safely when space is short | pending | run preflight (`scripts/vps-preflight.sh`), then measure |
| Vultr transfer allowance and shared traffic monitored | pending | Vultr dashboard (DEPLOYMENT.md cost notes) |
| Rollback verified without touching WireGuard | pending | DEPLOYMENT.md §8 |
| Incremental cost and storage/egress assumptions documented | done | DEPLOYMENT.md "Cost and capacity notes" |
| Worker image for the VPS architecture | done | `docker buildx build --platform linux/amd64` (5 min 41 s, 38.5 MB); binary runs under emulation |

## Commands used

Last full local run (2026-10-09, after the V3 changes): fmt and clippy clean, 98 unit and
in-process end-to-end tests, V1 49/49, smoke 40/40, Caddy 28/28, restore 6/6.

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                     # unit + in-process end-to-end
KFGW_LARGE_TEST_BYTES=10737418240 cargo test -p katfile-api --release --test large_upload -- --ignored --nocapture
tests/integration/v1_protocols.sh          # WORKER_COMMAND=record-hooks, LARGE_BYTES=10737418240
KFGW_COMPOSE_PROJECT=kfgw-e2e scripts/smoke-test.sh
KFGW_COMPOSE_PROJECT=kfgw-e2e tests/integration/v3_caddy.sh
KFGW_COMPOSE_PROJECT=kfgw-e2e scripts/restore-test.sh
scripts/api-probe.sh --write --parent-folder-id 211628
```
