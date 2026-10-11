# KatFile native VPS authentication POC guide

Updated: 2026-10-11. See [observed results](docs/KATFILE_VPS_POC_RESULTS.md)
for the successful Ubuntu VPS run and remaining verification gaps.

## Scope and success criteria

Run the existing Python/CDP flow with headful Chromium and Xvfb as a normal Linux
user. Do not introduce Docker, VNC, FlareSolverr, Node.js or a Rust rewrite for
this POC. Respect the website's authentication requirements; record rejection
or timeout instead of retrying indefinitely.

Success requires login HTTP 302 to /recommended.html, a non-empty xfss cookie,
and HTTP 302 for the test file. Browser startup or token availability alone is
insufficient. This does not prove production renewal or download URL usability.

## 1. Connect and inspect

```bash
ssh <VPS_USER>@<VPS_IP>
```

On the VPS:

```bash
id
cat /etc/os-release
uname -m
free -h
df -h /
swapon --show
sudo wg show interfaces
```

If WireGuard runs in wg-easy, inspect it inside the container:

```bash
sudo docker ps --format '{{.Names}} {{.Image}} {{.Status}}'
sudo docker exec wg-easy wg show interfaces
```

Do not change WireGuard or firewall configuration. Do not expose CDP publicly.

## 2. Install dependencies

Execute each command separately. On Debian 12/13:

```bash
sudo apt-get update
sudo apt-get install --no-install-recommends -y chromium xvfb python3 python3-venv curl ca-certificates procps
```

On Ubuntu, first inspect the distribution and package source:

```bash
command -v snap chromium chromium-browser
apt-cache policy chromium chromium-browser
```

The tested Ubuntu 26.04.1 installation uses Snap Chromium via chromium-browser:

```bash
sudo apt-get update
sudo apt-get install --no-install-recommends -y chromium-browser xvfb python3-venv curl ca-certificates procps
```

Verify and create the isolated Python environment:

```bash
chromium --version
python3 --version
command -v Xvfb chromium python3 curl
mkdir -p ~/.local/share/katfile
python3 -m venv ~/.local/share/katfile/venv
~/.local/share/katfile/venv/bin/pip install --no-cache-dir websocket-client
~/.local/share/katfile/venv/bin/python -c 'import websocket; print("CDP websocket ready", websocket.__version__)'
```

## 3. Check memory and swap

```bash
free -h
swapon --show
df -h /
```

Use one Chromium at a time. If swap already exists, do not create another file.
If there is no swap and sufficient disk space, obtain operator approval before
creating a 1 GB swap file or changing /etc/fstab. Swap is an OOM buffer, not RAM.

## 4. Provision the secret

On the VPS:

```bash
install -d -m 700 ~/.config/katfile ~/.local/state/katfile ~/katfile-poc
```

The secret must contain exactly two non-empty lines: username, then password.
It is not an .env file. From the Mac, copy the existing secret over SSH without
printing it. This command refuses to overwrite an existing remote secret:

```bash
ssh <VPS_USER>@<VPS_IP> 'umask 077; mkdir -p ~/.config/katfile; test ! -e ~/.config/katfile/secret || exit 1; cat > ~/.config/katfile/secret; chmod 600 ~/.config/katfile/secret' < ~/.config/katfile/secret
```

Verify permissions without printing contents:

```bash
stat -c '%a %n' ~/.config/katfile/secret
```

Expected mode: 600. The script validates the two-line format. Never use set -x,
print the secret, or put credentials in command arguments.

## 5. Start Xvfb

Check for an existing display before starting another instance:

```bash
pgrep -a Xvfb
ls -l /tmp/.X11-unix/X99
```

If display :99 is unused:

```bash
umask 077
nohup Xvfb :99 -screen 0 1280x900x24 -nolisten tcp > /tmp/katfile-xvfb.log 2>&1 < /dev/null &
echo $! > ~/.local/state/katfile/xvfb.pid
export DISPLAY=:99
```

Do not remove a live X11 lock or socket. Record the PID of the instance you start.

## 6. Deploy the script

The canonical implementation is [scripts/katfile_login_vps.sh](scripts/katfile_login_vps.sh).
It reads the secret locally, uses a fresh temporary profile, runs without
--no-sandbox, selects a loopback CDP port, stores state with restricted permissions,
and cleans up its browser process group.

From the repository on the Mac, check for an existing destination before copying:

```bash
ssh <VPS_USER>@<VPS_IP> 'test ! -e ~/katfile-poc/katfile_login_vps.sh'
scp scripts/katfile_login_vps.sh <VPS_USER>@<VPS_IP>:katfile-poc/katfile_login_vps.sh
```

On the VPS:

```bash
chmod 700 ~/katfile-poc/katfile_login_vps.sh
bash -n ~/katfile-poc/katfile_login_vps.sh
```

## 7. Run the POC

```bash
DISPLAY=:99 bash ~/katfile-poc/katfile_login_vps.sh
```

Expected final evidence:

```text
Login HTTP status: 302
Login redirect: /recommended.html
[6] xfss acquired
[7] Direct Link status: 302
SUCCESS
```

The bounded verification timeout is 300 seconds, with at most five click attempts.
On failure, diagnose the observed error before another run.

## 8. Observe resources and VPN

In a separate SSH terminal during the run:

```bash
free -h
ps -eo pid,ppid,stat,rss,comm
top
```

Record peak usage if needed; before/after snapshots do not measure peaks.
Repeat the applicable WireGuard checks from step 1. Container health alone does
not establish VPN throughput or handshake behavior under load.

## 9. Check the session without exposing it

```bash
stat -c '%a %n' ~/.local/state/katfile/session.cookies
curl -sS --max-time 30 --cookie "$HOME/.local/state/katfile/session.cookies" --output /dev/null --write-out 'HTTP=%{http_code}\n' 'https://katfile.biz/zr4x96pk1tt6/probe-b.txt.html'
```

Expected: mode 600 and HTTP=302. Do not print cookies or complete signed URLs.
HTTP 200 may indicate an invalid session, unavailable fixture or changed behavior;
inspect the response context before classifying the cause.

## 10. Verify cleanup

```bash
ps -eo pid,ppid,stat,comm
ss -ltnp
```

There should be no POC Chromium processes, CDP listener or new zombies. The CDP
port is dynamic; checking only port 9233 is insufficient. Xvfb may remain running.
If stopping it, confirm no other client uses :99, then send SIGTERM to the exact
recorded PID. Never use pkill chromium. SIGKILL, OOM or forced disconnection can
prevent Python finally blocks from running; inspect for leftovers in those cases.

## 11. Diagnose failures

```bash
tail -n 100 ~/.local/state/katfile/chromium.log
sudo journalctl -k --no-pager
```

| Symptom | Check |
|---|---|
| Browser exits or no CDP target | Chromium log, display, package and sandbox setup |
| Cannot open display | Xvfb process and :99 socket |
| No usable sandbox | Non-root account and OS sandbox configuration; do not immediately disable it |
| Verification timeout or 600010 | Browser verification state and network; record rejection without unbounded retries |
| Login is not 302 | Current form, token freshness and sanitized response metadata |
| Missing xfss | Cookie jar and login result |
| File page is not 302 | Session and fixture availability |
| Process killed | Memory, swap and privileged kernel OOM log |
| VPN slowdown | Peak resource use and VPN traffic observations |

Review logs for sensitive values before sharing them.

## 12. Decide the next step

A complete pass establishes this Python flow for the tested environment and file.
Use it as the baseline for Rust integration. Production acceptance still requires
repeatability, encrypted session storage, renewal, bounded retries, download URL
validation, reboot recovery and resource/VPN performance tests.
