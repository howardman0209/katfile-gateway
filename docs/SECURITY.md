# Security

## Trust model

* **One KatFile account** holds every user's archive. Per-user separation is enforced by
  this gateway (SFTPGo homes + the worker's folder mapping), not by KatFile. Anyone with
  the KatFile account, the API key, or root on the VPS can read every user's files.
* **SFTPGo** authenticates users and confines each one to its home. Its admins are fully
  trusted (they can create users with any home directory).
* **Hook payloads are untrusted** even though they are authenticated: the worker uses
  only the username and virtual path, re-derives the physical path, refuses `..`,
  control characters and symlinks on every path component, cross-checks the payload's
  `path`, and verifies account state through the SFTPGo API after the event.
* **KatFile responses are untrusted input**: upload hosts must match
  `^s[0-9]{1,6}\.katfile\.biz$` on port 443 over HTTPS, redirects are refused, DNS answers
  must be public addresses (SSRF guard), reply sizes are bounded.

## Secrets

| Secret | File (`./secrets`, dir 0700) | Consumer | Injection |
|---|---|---|---|
| KatFile API key | `katfile_api_key` | worker | Docker secret `/run/secrets/katfile_api_key` |
| Worker webhook secret | `worker_webhook_secret` | worker, SFTPGo hook client | Docker secret / SFTPGo `env.d` |
| Worker admin token | `worker_admin_token` | operators (`/admin/*`) | Docker secret |
| Caddy admission secret | `caddy_admission_secret` | Caddy → worker `forward_auth` | Docker secret, read by Caddy's `{file.*}` placeholder |
| SFTPGo admin password | `sftpgo_admin_password` | WebAdmin `admin` | `env.d` (`secrets/sftpgo.env`) |
| SFTPGo signing passphrase, KMS master key | `sftpgo_*` | SFTPGo | `env.d` |
| Worker's SFTPGo API key | `sftpgo_worker_api_key` | worker | Docker secret; created by `sftpgo-bootstrap.sh` |

* Secrets are never in images, Git (`.gitignore`, `.dockerignore`), compose environment
  blocks or command lines. Scripts pass credentials to curl through private config files.
* The KatFile key travels only in POST bodies, never in URLs; reqwest errors are stripped
  of URLs; HTTP client crates are capped at `warn` in logs (`KFGW_UNSAFE_HTTP_TRACE` exists
  for debugging only). Probe reports and logs were scanned for the key (none found).
* The SFTPGo KMS master key must never change after first start (encrypted settings).
* Rotation: see [OPERATIONS.md](OPERATIONS.md#rotating-secrets). The KatFile key used during
  development was pasted into a chat transcript and **should be rotated before production**.

## Findings during implementation

1. **SFTPGo debug logging leaks the hook secret.** With the image default log level
   (`debug`), SFTPGo prints its full configuration at start-up including
   `http.headers[].value` — the `Authorization: Bearer <webhook secret>` header.
   Mitigation: `SFTPGO_LOG_LEVEL=info` in `compose.yaml`; the secret exposed during
   testing was rotated and the containers (and their logs) recreated.
2. **KatFile stores uploads with an invalid session anonymously** (public by file code).
   The worker requests a fresh session per upload and only marks a job archived after the
   account-scoped listing shows the file in the user's folder; otherwise the job ends in
   `needs_review` (`remote_ownership_unverified`). Such files cannot be deleted via API.
3. **Caddy applies header deletions after sets**: `header_up -Authorization` would remove
   the proxy's own credential. The Caddyfile sets `Authorization` (replacing the user's
   Basic credentials) and does not delete it.
4. **Caddy's binary carries a file capability**, so `cap_drop: ALL` makes `exec` fail;
   the container keeps only `NET_BIND_SERVICE` and runs as uid 1001.

## Worker privileges (deviation from "read-only staging")

The plan preferred read-only staging access for the worker. That is not feasible with
immutable hard-link snapshots and verified cleanup: `linkat`/`renameat` need write access
and only work within one mount. The deviation is contained by design:

* the worker's only write operations are: create a link in `spool/`, rename a visible
  file into `spool/quarantine/` and unlink it after an inode match, move a deleted
  user's home into `spool/deleted-homes/`, remove empty directories there, and delete
  stale `.sftpgo-upload.*` temp files untouched for an hour;
* every path is reached through directory file descriptors with `O_NOFOLLOW`;
* state gates: only `archived` (verified) content is cleaned automatically; failed,
  retrying, blocked and ambiguous content is never deleted without an operator
  (`discard`) decision;
* the worker cannot reach SFTPGo's database, host keys or configuration (separate volume),
  and its SFTPGo API key can only view users and start quota scans (verified: creating a
  user with it returns 403).

## Exposure

* Public by design (only after operator approval): Caddy TCP 443 (+80 for redirects/ACME)
  and, if chosen, the SFTPGo SSH port. Everything binds to `127.0.0.1` by default.
* Never public: SFTPGo WebAdmin/REST (loopback `SFTPGO_ADMIN_HOST_PORT`), the worker API
  (loopback `WORKER_ADMIN_HOST_PORT`). Reach them through WireGuard or an SSH tunnel.
* Through Caddy only the SFTPGo WebDAV listener is reachable; `/web/admin` and the hook
  paths there are just WebDAV paths (401). The browser WebClient is disabled.
* SFTPGo's defender (brute-force banning) is on; WebDAV sees real client IPs via
  `X-Forwarded-For` trusted from the backend subnet only.

## Containers

Non-root (1000 for SFTPGo/worker, 1001 for Caddy), `cap_drop: ALL`, `no-new-privileges`,
read-only root filesystems, tmpfs `/tmp`, memory/CPU limits, pinned image digests,
rotated JSON logs. Base images: `drakkan/sftpgo:v2.7.6`, `caddy:2.11.7-alpine`,
`gcr.io/distroless/cc-debian13:nonroot` (worker runtime), `rust:1.99.0-slim-trixie` (build).

## Residual risks

* Users can upload anything their quota allows; there is no malware scanning.
* A file deleted by its owner within milliseconds of completing an upload can be missed
  (it is gone before the snapshot); everything later is archived.
* KatFile session lifetime during multi-hour uploads is unknown (V3 test item).
* SFTPGo is AGPLv3; it is used unmodified as a container image. Re-evaluate obligations
  before distributing modified SFTPGo builds.
