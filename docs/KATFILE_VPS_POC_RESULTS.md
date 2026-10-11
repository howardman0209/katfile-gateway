# Native VPS authentication POC checkpoint

Date: 2026-10-11 (Asia/Hong_Kong)

## Scope

Port the existing Python/CDP authentication flow to a non-root Linux user with
headful Chromium and Xvfb. No Docker, VNC, FlareSolverr, Node.js or Rust toolchain
was introduced for this authentication run. The existing VPN uses Docker.

The implementation is [katfile_login_vps.sh](../scripts/katfile_login_vps.sh).
Setup instructions are in the [VPS test guide](../KATFILE_VPS_TEST_GUIDE.md).

## Environment and checkpoints

| Checkpoint | Evidence | Result |
|---|---|---|
| 1. SSH and environment | Non-root sudo-group account; Ubuntu 26.04.1 LTS, x86_64 | Passed |
| 2. Dependencies | Chromium 155.0.8059.39 Snap; Xvfb; Python 3.14.4; websocket-client 1.9.2; curl | Passed |
| 3. Resources | 950 MiB RAM; existing 2.3 GiB swap; about 14 GB free disk | No new swap needed |
| 4. Credentials | Existing local two-line secret copied over SSH; remote format checked; mode 600 | Passed |
| 5. Display | Xvfb :99, 1280x900x24, TCP disabled | Passed |
| 6. Deployment | ~/katfile-poc/katfile_login_vps.sh, mode 700; shell syntax valid | Passed |
| 7. Authentication | Fresh temporary profile; token obtained after one click; login 302 to /recommended.html; non-empty xfss; file-page redirect 302 | Passed once |
| 8. VPN and resources | User supplied wg-easy container status: Up 2 weeks (healthy); wg show interfaces returned wg0 | Post-run service state confirmed |
| 9. Session reuse | Cookie jar mode 600; independent file-page request returned HTTP=302 | Passed |
| 10. Cleanup | No Chromium processes, CDP listeners or zombie processes observed; Xvfb left running | Passed |
| 11. Diagnostics | No login/browser failure required troubleshooting | Not needed |
| 12. Decision | Native Python flow works for this run; ready to use as the Rust port baseline | Production acceptance pending |

## Sanitized observed output

```text
[1] Chromium started
[2] Login form loaded
[3] Page state: {"title": "KatFile - Free Cloud Storage", "widgetPresent": true, "tokenLength": 0}
[4] Verification token available (1 click(s))
[5] Submitting curl login
Login HTTP status: 302
Login redirect: /recommended.html
[6] xfss acquired
[7] Direct Link status: 302
Direct Link host: s5085.katfile.biz
Direct Link acquired: True
SUCCESS
```

The cookie jar remains on the VPS at ~/.local/state/katfile/session.cookies.
Credentials, cookie values, verification tokens, signed download URLs and SSH
connection details are intentionally excluded from this record.

## Resource observations and limits

Immediately before the run: 697 MiB available RAM and 138 MiB swap used.
After the run: 716 MiB available RAM and 150 MiB swap used.
These snapshots do not measure peak usage. The unprivileged kernel-journal query
returned no entries; it does not establish the absence of OOM events.

The agent could not read Docker state directly because the account lacked Docker
socket permission. Container health and the WireGuard interface were checked by
the operator using sudo after the run. There was no privileged pre-run VPN
baseline, handshake/traffic comparison or VPN throughput measurement.

This run resolved a redirect but did not fetch the newly returned download URL.
Download usability, repeated fresh logins, session expiry/renewal, reboot recovery,
peak CPU/RAM use and VPN performance under load remain unverified. Earlier download
experiments in the authentication plan are separate evidence.

## Checkpoint contents

- Preserve the historical macOS Docker login script as a reference.
- Add the native VPS login script without changing its tested authentication flow.
- Keep deployment instructions separate from the script to avoid duplicate code.
- Update the authentication plan to distinguish this successful run from production readiness.

No Rust runtime or production-service behavior changed in this checkpoint.
