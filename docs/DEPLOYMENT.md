# Deployment on the existing Vultr VPS (V3)

Target: the existing Vultr instance (1 vCPU, 1 GB RAM, 25 GB SSD) that already runs
WireGuard. **WireGuard stays exactly as it is**: no changes to its config, port,
interface, routes, NAT, firewall rules or systemd unit, and it is never restarted for
gateway work.

Nothing in this document has been executed against the VPS yet. Every step marked
**[approval]** needs an explicit go-ahead from the operator first.

## 0. Before anything: access and rollback route

* Confirm you can log in through the **Vultr web console** (out-of-band) in case SSH or
  the VPN breaks. Do not start without it.
* Take a Vultr **snapshot** of the instance if the plan/budget allows (billable — ask).
* Record a WireGuard baseline **from a real client** (phone/laptop on cellular):
  handshake age (`wg show` on the server), `ping -c 20 <VPN server address>` (RTT, loss),
  a page load through the tunnel (egress works), DNS resolution through the tunnel, and
  optionally `iperf3` throughput. Keep the numbers for comparison after each step.

## 1. Read-only preflight

Copy `scripts/vps-preflight.sh` to the VPS and run it (it changes nothing):

```bash
scp scripts/vps-preflight.sh vps:/tmp/
ssh vps 'sudo bash /tmp/vps-preflight.sh' > preflight-$(date +%F).txt   # redacted by default
```

Check in the output:

| Item | Why |
|---|---|
| `uname -m` | `x86_64` → build the worker image for `linux/amd64` (section 4) |
| MemAvailable, swap | the gateway needs roughly 250–700 MiB including page-cache headroom; stop if the host is already tight (plan §13) |
| Free disk on `/`, other filesystems | a 10 GB upload needs ≥ 10 GB + 4 GiB reserve + pending archives; decide the staging location (section 2) |
| Listening sockets | 80, 443, 2022 (or your SFTP port) must be free; host SSH port stays untouched |
| WireGuard listen port, interface name, peer handshakes | must be identical after every step |
| `net.ipv4.ip_forward`, FORWARD policy and NAT rules (iptables/nftables), ufw/firewalld state | Docker changes forwarding rules (section 3) |
| Docker present? version, `daemon.json` | decides how Docker is installed/configured |
| Address ranges in use (WireGuard subnet, other bridges) | the backend network defaults to `172.30.200.0/24` (`KFGW_BACKEND_SUBNET`); Docker's default bridge is `172.17.0.0/16`; neither may overlap the VPN subnet |

If memory or disk is already close to the limit, **stop and report** with the numbers
and an upgrade recommendation instead of deploying (plan §13.6).

## 2. Decisions for the operator

| Decision | Options | Recommendation |
|---|---|---|
| WebDAV exposure | public `443/tcp` (+`80/tcp` for ACME/redirect) or VPN-only | Start VPN-only (`HTTPS_BIND_ADDRESS=<wg address>`, `CADDY_TLS=internal`), go public later if needed |
| SFTP exposure | public port (e.g. `2022/tcp`) or VPN-only | VPN-only: `SFTP_BIND_ADDRESS=<wg address>` |
| Domain / TLS | ACME needs a public DNS name and public 80/443; VPN-only uses Caddy's internal CA (install its root on clients) | — |
| Staging storage | Docker volume on `/` or a dedicated filesystem | Dedicated filesystem so a full staging area can never fill `/` (SSH, WireGuard, logs): Vultr Block Storage (billable) or a fixed-size loop-mounted image file; set `STAGING_SOURCE=/srv/kfgw-staging` |
| Per-user quota / max file size | SFTPGo user settings | Sum of quotas + 4 GiB reserve ≤ staging size |
| KatFile parent folder | `KATFILE_USERS_PARENT_FOLDER_ID` | A dedicated folder (e.g. `gateway`) instead of the account root |

## 3. Docker on a WireGuard host **[approval]**

When the Docker daemon has to enable IP forwarding itself, it also sets the firewall's
`FORWARD` policy to **DROP** (current Docker documentation, "Docker on a router"); older
Docker releases did this unconditionally. A WireGuard host normally has forwarding
enabled already, but if the WireGuard setup relies on a default `ACCEPT` policy (common
with minimal `wg-quick` configs), a DROP policy cuts VPN clients' internet egress the
moment Docker starts.

* Before installing/starting Docker, create `/etc/docker/daemon.json` with
  `{"ip-forward-no-drop": true, "log-driver": "json-file", "log-opts": {"max-size": "10m", "max-file": "3"}}`
  so Docker never changes the policy (check `dockerd --help | grep ip-forward-no-drop`
  for the installed version). If the option is unavailable, add narrow accept rules for
  the WireGuard interface to the `DOCKER-USER` chain instead.
* Immediately after Docker starts: `iptables -S FORWARD | head -1`, `wg show`, and the
  client checks from section 0. If egress broke: `systemctl stop docker docker.socket`
  and restore the previous FORWARD policy recorded by the preflight. Do not touch
  WireGuard itself.
* Docker-published ports are not governed by ufw rules; only ports bound to non-loopback
  addresses in `.env` are reachable from outside. Admin ports are always loopback.

## 4. Images

The VPS is too small to compile the worker. Build on a workstation and copy:

```bash
docker buildx build --platform linux/amd64 -f crates/katfile-worker/Dockerfile -t katfile-worker:v0.1.0 --load .
docker save katfile-worker:v0.1.0 | gzip | ssh vps 'gunzip | docker load'
```

Set `image: katfile-worker:v0.1.0` (or keep `katfile-worker:local` and tag accordingly);
`compose.yaml` pins SFTPGo and Caddy by digest.

## 5. Install the gateway **[approval]**

1. Copy the repository without secrets (`git archive` or `git clone`); create `./secrets`
   on the VPS (`scripts/init-secrets.sh`), then copy only the KatFile key securely
   (`scp`, then `chmod 0644 secrets/katfile_api_key` inside the `0700` directory).
   Rotate the KatFile key first if it was ever exposed (see SECURITY.md).
2. `cp .env.example .env`; keep every `*_BIND_ADDRESS` on `127.0.0.1` or the WireGuard
   address for the first start; set `DAV_DOMAIN`, `CADDY_TLS`, `STAGING_SOURCE`,
   `KATFILE_USERS_PARENT_FOLDER_ID`.
3. `docker compose up -d sftpgo` → `scripts/sftpgo-bootstrap.sh` (needs `curl` and `jq`
   on the host, or run it from a workstation through an SSH tunnel to the admin port) →
   `docker compose up -d`.
4. `scripts/api-probe.sh` (read-only) from the VPS: proves DNS, TLS and egress to KatFile.
5. Create one test user (required settings in OPERATIONS.md), upload a small file over
   WebDAV and SFTP through the VPN, confirm `archived` in `/admin/status` and in KatFile.
   Re-check WireGuard (section 0).
6. Only now, if approved: change bindings to public addresses, open the firewall
   narrowly (below), create the DNS record, let Caddy obtain the certificate
   (`CADDY_TLS=<email>`), and re-check WireGuard after each change.

## 6. Firewall changes **[approval]**

Add only what is needed, one rule at a time, and write each rule down for rollback.
Never `ufw reset`, `iptables -F`, `nft flush ruleset` or replace rule files.

* ufw: `ufw allow 443/tcp`, `ufw allow 80/tcp` (ACME/redirect), and the SFTP port only if
  public SFTP was approved.
* Plain iptables/nftables: insert narrow `ACCEPT` rules for the same ports in the input
  chain; leave the WireGuard and NAT rules alone.

## 7. Acceptance on the VPS

Run and record (see ACCEPTANCE.md, V3):

* WireGuard from an external client: handshake, connectivity, DNS, egress, latency —
  before, during a large upload, after `docker compose restart`, after rollback.
* TLS valid from a mobile WebDAV app; SFTP app connects and the host key fingerprint
  matches `GET /api/v2/status` (`ssh.host_keys`).
* Real uploads (document, photo, video; a near-10 GB video only if approved: it creates
  large remote data and uses transfer quota) archived into the right user's folder.
* `docker stats` / `free -m` during a large upload; disk admission rejects an oversize
  upload (507 / permission denied) without filling `/`.
* KatFile outage (temporarily block egress for the worker container), restart while
  uploading, webhook replay, cross-user isolation (`tests/integration/v3_caddy.sh`,
  `scripts/smoke-test.sh` with a disposable parent folder).
* Vultr dashboard: transfer allowance and current usage; outbound archive traffic is
  roughly the uploaded volume (≤ 100 GB/month planned) plus retries, on top of VPN traffic.

## 8. Rollback (does not touch WireGuard)

1. `docker compose down` (containers and gateway networks go away; volumes stay).
2. Remove only the firewall rules added in section 6 and the DNS record if created.
3. If Docker changed the FORWARD policy and WireGuard egress suffers:
   `systemctl stop docker docker.socket`, restore the recorded policy.
4. Verify WireGuard from an external client as in section 0.
5. Optional clean-up of data (volumes) only after confirming everything was archived.

## Cost and capacity notes

* Incremental server cost: US$0 if the existing instance has the headroom; possible
  extra costs: Vultr bandwidth overage, snapshots/backups, Block Storage, a larger plan,
  the KatFile premium plan, a domain.
* Disk: streaming protects RAM, not disk. A staged 10 GB file occupies 10 GB until it is
  archived **and** its retention ends (default 24 h; earlier under disk pressure).
  On 25 GB total with OS, Docker images (~0.5 GB) and logs, expect roughly 10–15 GB of
  usable staging; one 10 GB upload at a time is realistic only with that space free.
* Memory: SFTPGo ≤ 512 MiB (mostly reclaimable page cache), worker ≤ 128 MiB (~10 MiB
  RSS), Caddy ≤ 128 MiB (~20 MiB). Measure with the real WireGuard load before go-live.
