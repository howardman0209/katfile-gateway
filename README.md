# KatFile WebDAV/SFTP Gateway

Upload-first gateway that lets existing WebDAV and SFTP apps (Android, iOS, desktop)
archive files into one private **KatFile** account, with isolated users managed in
**SFTPGo**. A Rust worker turns completed uploads into durable jobs, streams them to
KatFile, files them under each user's own KatFile folder and verifies the result.

```
WebDAV app ──HTTPS──> Caddy ──> SFTPGo ──(hooks)──> katfile-worker ──HTTPS──> KatFile
SFTP app ───SSH/SFTP────────────> SFTPGo        │          │
                                   └─ staging volume (homes, snapshots) ─┘
```

## Read this first: what "uploaded" means

* **A finished WebDAV `PUT` or SFTP transfer only means the file reached the gateway's
  staging disk.** It is archived when its job reaches `archived` — the KatFile file
  code was verified inside the user's own KatFile folder via the account-scoped
  listing. Check `/admin/status` or `/admin/jobs` (operators only).
* **This is not a two-way cloud drive.** Archived files stay visible for the retention
  period (default 24 h) and are then removed locally; they disappear from WebDAV/SFTP
  but remain on KatFile. Use "upload/backup" modes in mobile apps, not two-way sync,
  or the app may upload the same file again after it disappears.
* **Inbox rules for users:** files cannot be overwritten in place (the `overwrite`
  permission is refused by the worker); delete and re-upload instead. A file that was
  completely uploaded is archived even if the user deletes or renames it afterwards.
* Remote renames/deletes after archival are out of scope.

## Components

| Path | Purpose |
|---|---|
| `crates/katfile-api` | Typed streaming KatFile client (POST-only API calls, SSRF guard, ambiguity-aware uploads) |
| `crates/katfile-worker` | Webhooks, SQLite queue, provisioning, runner, reconciler, cleanup, admin API, probe |
| `crates/katfile-mock` | KatFile test double reproducing behaviour verified on the live service |
| `compose.yaml` | SFTPGo v2.7.6 + worker + Caddy, pinned by digest, loopback-only by default |
| `compose.dev.yaml` | Local overlay: direct WebDAV on loopback, public DNS for the worker |
| `scripts/` | `init-secrets.sh`, `sftpgo-bootstrap.sh`, `api-probe.sh`, `smoke-test.sh`, `restore-test.sh`, `vps-preflight.sh` |
| `docs/` | Architecture, API compatibility evidence, security, deployment, operations, acceptance |

## Quick start (local, macOS/Linux with Docker)

```bash
cp .env.example .env               # then adjust; keep bindings on 127.0.0.1
scripts/init-secrets.sh            # generates separate secrets in ./secrets (0700)
# put the KatFile API key into secrets/katfile_api_key (never commit it)
docker compose -f compose.yaml -f compose.dev.yaml up -d sftpgo
scripts/sftpgo-bootstrap.sh        # least-privilege API key for the worker
docker compose -f compose.yaml -f compose.dev.yaml up -d
```

* SFTPGo WebAdmin: `http://127.0.0.1:${SFTPGO_ADMIN_HOST_PORT}/web/admin` (user `admin`,
  password in `secrets/sftpgo_admin_password`). Create users there — the worker
  provisions their KatFile folders automatically. **Required user settings** are in
  [docs/OPERATIONS.md](docs/OPERATIONS.md#creating-users).
* WebDAV (dev overlay): `http://127.0.0.1:${WEBDAV_DEV_PORT}/`; via Caddy:
  `https://${DAV_DOMAIN}:${HTTPS_PORT}/`. SFTP: `sftp -P ${SFTP_PORT} user@127.0.0.1`.
* Worker status: `curl -H "Authorization: Bearer $(cat secrets/worker_admin_token)" http://127.0.0.1:${WORKER_ADMIN_HOST_PORT}/admin/status`

Before touching a real account, read [docs/API_COMPATIBILITY.md](docs/API_COMPATIBILITY.md)
and run the probe: `scripts/api-probe.sh` (read-only) or
`scripts/api-probe.sh --write --parent-folder-id <disposable folder>`.

## Tests

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                       # unit + in-process end-to-end (mock KatFile/SFTPGo)
cargo build --workspace --release
KFGW_LARGE_TEST_BYTES=10737418240 cargo test -p katfile-api --release --test large_upload -- --ignored --nocapture
tests/integration/v1_protocols.sh            # WebDAV/SFTP isolation (WORKER_COMMAND=record-hooks)
KFGW_COMPOSE_PROJECT=kfgw-e2e scripts/smoke-test.sh   # real KatFile, creates test data
KFGW_COMPOSE_PROJECT=kfgw-e2e tests/integration/v3_caddy.sh   # TLS, proxy, 507 admission
KFGW_COMPOSE_PROJECT=kfgw-e2e scripts/restore-test.sh
```

Each compose project on one host needs its own `KFGW_BACKEND_SUBNET` (the backend
network has a fixed subnet so SFTPGo can trust Caddy's `X-Forwarded-For`): export a
different subnet, or `docker compose -p <other> down` first (`stop` keeps the network).
Integration tests that hit the real KatFile service are opt-in and create persistent
remote test data (a few KB per run under the configured parent folder); nothing is
deleted remotely. Evidence of the last runs is in [docs/ACCEPTANCE.md](docs/ACCEPTANCE.md).

## Known limitations

* KatFile has no idempotency key: a lost reply after a fully sent upload is resolved by
  an exact name/size/time search; if that is not unique the job waits in `needs_review`.
  Exactly-once remote uploads are not guaranteed.
* An invalid/expired upload session makes KatFile store the file **anonymously**
  (publicly reachable by code). The worker never reports such a file as archived
  (`remote_ownership_unverified`), but it cannot delete it. The session lifetime for very
  long (10 GB) uploads is unverified — a V3 acceptance item.
* Only uploads up to a few KB were verified on the live service. 10 GB uploads were
  verified locally (SFTPGo staging, mock KatFile streaming with ~3 MiB RSS growth).
* Users must not have `overwrite`, `*` or `create_symlinks` permissions, groups,
  virtual folders or a non-default home; otherwise archival is blocked (visible in
  the status API) until the account is fixed.
* `direct_link` is disabled on the account, so remote content cannot be re-downloaded
  for hash comparison; verification is file code + size + folder ownership.

Licensing: SFTPGo is AGPLv3 (used unmodified as a container image); evaluate the
obligations before redistributing modified components. This repository's own license
is not decided yet (`UNLICENSED`).
