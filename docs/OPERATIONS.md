# Operations

All admin endpoints are loopback-only on the host. Over SSH:
`ssh -L 18080:127.0.0.1:8080 -L 18090:127.0.0.1:8090 vps` (or use the WireGuard address).

```bash
W=http://127.0.0.1:8090            # worker API (WORKER_ADMIN_HOST_PORT)
T="$(cat secrets/worker_admin_token)"
wapi() { curl -sS -H "Authorization: Bearer $T" "$@"; }
wapi $W/admin/status | jq .
```

## Creating users

Create users in SFTPGo WebAdmin (Users → Add). The worker provisions a KatFile folder
named after the user under `KATFILE_USERS_PARENT_FOLDER_ID` automatically (suffix `-2`,
`-3`, … if the name is already used there). **Required settings** — otherwise the user
is `blocked` and uploads wait in `awaiting_principal`, with the reason in the status API:

| Setting | Value |
|---|---|
| Username | lower-case `a-z 0-9 . _ - ~` (SFTPGo naming rules 6 enforce it) |
| Home directory | leave empty (= `/srv/sftpgo/data/<username>`) |
| Permissions for `/` | `list, download, upload, create_dirs, rename, delete` — **never** `*` (All), `overwrite` or `create_symlinks` |
| Groups, virtual folders | none |
| Storage | Local filesystem |
| Quota (recommended) | size quota and "max upload file size", e.g. 11 GiB per file |
| Credentials | strong unique password; SSH public keys for SFTP where the app supports them |

WebAdmin pre-selects "All" permissions for new users; change it before saving. Fixing a
blocked user in WebAdmin (save) re-evaluates it automatically.

Tell users: an upload is first staged, then archived; archived files disappear from
WebDAV/SFTP after the retention period (default 24 h); files cannot be overwritten
(delete and upload again); use backup/upload modes, not two-way sync.

## Daily checks

```bash
wapi $W/admin/status | jq '{attention, queue_depth, oldest_unarchived_age_s, staging, katfile_auth_error, principals}'
```

* `attention.needs_review|failed|blocked` should be 0 — inspect with
  `wapi "$W/admin/jobs?state=needs_review" | jq '.jobs[] | {id, username, virtual_path, last_error_category, last_error}'`.
* `staging.disk_pressure` must be false; `staging.free` vs `WORKER_MIN_FREE_BYTES`.
* `katfile_auth_error` non-null means the API key was rejected (rotate/fix it).
* `last_reconcile` shows the last reconciliation summary (missed uploads, stale temp files).
* `docker compose ps` healthy; `docker stats --no-stream` within limits;
  `docker events --since 24h --filter event=oom` empty.
* WireGuard: handshakes recent (`wg show`), clients online (see DEPLOYMENT.md).

## Handling jobs that need attention

| Category (`last_error_category`) | Meaning | Action |
|---|---|---|
| `ambiguous_upload`, `ambiguous_upload_crash` | the whole body was sent but no usable reply; the reconciler retries a unique name/size match for 7 days | Look for the file in KatFile (root folder, exact name/size). If it is there: `POST /admin/jobs/{id}/adopt {"file_code": "..."}`. If not: `POST /admin/jobs/{id}/retry` (explicit approval to upload again) |
| `remote_ownership_unverified` | KatFile returned a code but the file is not in the account (possible anonymous upload) | Do not adopt. Retry once; if it repeats, stop and check the session/key; report the anonymous code to KatFile support if needed |
| `snapshot_changed`, `snapshot_changed_during_upload`, `snapshot_missing` | staged content changed (should be impossible without `overwrite`) | Investigate the user's permissions; `retry` or `discard` |
| `remote_size_mismatch` | remote size differs | Investigate; `retry` re-runs from the recorded file code |
| `upload_rejected` (failed) | KatFile refused the file | `discard` or fix and `retry` |
| `principal_disabled` (blocked) | account disabled | re-enable the user; jobs requeue automatically |
| `principal_deleted` (blocked) | account deleted before archival | `retry` archives into the deleted account's own folder; `discard` drops it |

```bash
wapi -X POST $W/admin/jobs/<id>/retry
wapi -X POST -H 'Content-Type: application/json' -d '{"file_code":"abcd1234efgh"}' $W/admin/jobs/<id>/adopt
wapi -X POST $W/admin/jobs/<id>/discard          # removes local copies, never remote files
wapi -X POST $W/admin/jobs/<id>/expire-retention # archived job: free its space now
wapi -X POST $W/admin/reconcile                  # run a reconciliation pass now
wapi -X POST $W/admin/cleanup                    # run a cleanup pass now
```

## Principals

`wapi $W/admin/principals | jq '.principals[] | {username, state, state_reason, remote_folder_id, remote_folder_name}'`

* `provisioning` retries transient KatFile errors with backoff.
* `failed` needs a decision: `POST /admin/principals/{id}/approve` with
  `{"folder_name": "alice-archive"}` (create a new folder with that name) or
  `{"bind_folder_id": 123456}` (bind an existing child of the users parent folder — only
  when you know it belongs to this account, e.g. after restoring a backup).
* `blocked` = unsupported SFTPGo settings (see above); `disabled` = disabled/expired user.
* `deleted` = tombstone. The KatFile folder and files are kept forever.

## Deleting users

Delete the user in WebAdmin. The worker then: tombstones the principal, blocks uploads
that were not archived yet, keeps the KatFile folder, moves the home to
`spool/deleted-homes/<principal-id>` (out of SFTPGo's reach), cleans already archived
files there at once and removes the directory when it is empty. Blocked uploads stay
until you `retry` or `discard` them. Re-creating the same username creates a new,
empty home and a new KatFile folder.

## Rotating secrets

| Secret | Steps |
|---|---|
| KatFile API key | write the new key to `secrets/katfile_api_key`; `docker compose up -d --force-recreate katfile-worker`; `scripts/api-probe.sh` |
| Webhook secret | delete `secrets/worker_webhook_secret`; `scripts/init-secrets.sh`; `docker compose up -d --force-recreate sftpgo katfile-worker` |
| Admin token / Caddy admission secret | delete the file; `scripts/init-secrets.sh`; recreate `katfile-worker` (and `caddy`) |
| Worker's SFTPGo API key | revoke it in WebAdmin (API keys), empty the file, `scripts/sftpgo-bootstrap.sh`, recreate the worker |
| SFTPGo admin password | change it in WebAdmin, then update `secrets/sftpgo_admin_password` (bootstrap uses it) |

Never rotate `sftpgo_kms_master_key` in place (SFTPGo could no longer decrypt secrets).

## Backups and restore

| Data | How | Notes |
|---|---|---|
| Worker DB | `docker compose exec -T katfile-worker katfile-worker backup-db /var/lib/katfile-worker/backup-$(date +%F).db`, then `docker compose cp` it off the host | consistent online copy (`VACUUM INTO`); drill: `scripts/restore-test.sh` |
| SFTPGo users/settings | WebAdmin → Maintenance → Backup, or `GET /api/v2/dumpdata` | contains password hashes: store encrypted |
| Staging volume | optional | the DB alone cannot recover uploads that were not archived yet; KatFile holds everything archived |
| Secrets | encrypted copy of `./secrets` | KMS master key is required to restore SFTPGo |

Restore = stop the worker, copy the backup over `worker.db`, remove `worker.db-wal` and
`worker.db-shm`, start; then `POST /admin/reconcile` to re-attach any staged files.
Jobs that were archived and cleaned after the backup was taken reappear as
`needs_review` (`snapshot_missing`): find their file in KatFile and `adopt` it, or
`discard` them. Nothing is uploaded twice automatically.

## Upgrades

1. Read release notes (SFTPGo Open Source, Caddy). Check that env names used in
   `compose.yaml` still exist and that custom actions keep the same payload
   (compare with `tests/fixtures`).
2. Pin the new tag **and digest** (`docker buildx imagetools inspect <image>:<tag>`).
3. Back up (above), `docker compose pull && docker compose up -d`, run
   `tests/integration/v3_caddy.sh` against a test user, watch `/admin/status`.
4. Rollback: restore the previous digests in `compose.yaml` and `docker compose up -d`
   (volumes are untouched). WireGuard is never restarted for gateway changes.

## Troubleshooting

* **Uploads fail with "permission denied" right away:** `pre-upload` admission refused
  (staging below reserve) or the worker is down (fail-closed). Check `/admin/status`.
* **WebDAV PUT gets 507:** declared size does not fit `free - reservations - reserve`.
* **Jobs stuck in `retry_waiting` with DNS/connect errors:** egress or DNS from the
  worker container (`docker compose exec katfile-worker katfile-worker probe` runs a
  read-only API check).
* **`katfile_auth_error`:** key rejected; rotate.
* **SFTPGo restarted mid-upload:** partial `.sftpgo-upload.*` files are removed by the
  reconciler after `WORKER_STALE_TEMP_SECS` (1 h).
* **Logs:** `docker compose logs --since 1h katfile-worker | grep -E 'WARN|ERROR'`.
