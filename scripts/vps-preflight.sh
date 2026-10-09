#!/usr/bin/env bash
# READ-ONLY preflight audit for the existing Vultr VPS (plan section 13).
# It changes nothing: no package installs, no firewall/routing/WireGuard changes.
#
# Run on the VPS as root (needed to read WireGuard and firewall state):
#   sudo bash vps-preflight.sh > preflight-$(hostname)-$(date +%F).txt
# Public IPv4/IPv6 addresses, WireGuard keys and peer endpoints are redacted by default
# (set PREFLIGHT_NO_REDACT=1 only for a copy you keep to yourself). Never commit the output.
set -u
export LC_ALL=C

redact() {
  if [[ "${PREFLIGHT_NO_REDACT:-0}" == 1 ]] || ! command -v perl >/dev/null 2>&1; then
    cat
    return
  fi
  perl -pe '
    sub keep { my ($a,$b)=@_; return $a==10 || $a==127 || $a==0 || $a==255 || ($a==172 && $b>=16 && $b<=31)
      || ($a==192 && $b==168) || ($a==100 && $b>=64 && $b<=127) || ($a==169 && $b==254) }
    s/\b(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})\b/keep($1,$2) ? "$1.$2.$3.$4" : "<public-ipv4>"/ge;
    s/\b[23][0-9a-f]{3}:[0-9a-f:]{2,}(\/\d+)?/<public-ipv6>/gi;
    s{[A-Za-z0-9+/]{42,43}=}{<wg-key>}g;
  '
}

section() { printf '\n===== %s =====\n' "$1"; }
run() { # run LABEL COMMAND...: print a command's output (redacted) or note its absence
  local label="$1"
  shift
  printf '\n--- %s\n' "$label"
  if command -v "$1" >/dev/null 2>&1; then
    "$@" 2>&1 | redact
  else
    echo "(not installed: $1)"
  fi
}

echo "katfile gateway VPS preflight (read-only), $(date -u +%FT%TZ)"
[[ $EUID -eq 0 ]] || echo "WARNING: not running as root; WireGuard/firewall sections will be incomplete."

section "system"
run "kernel" uname -a
run "os-release" cat /etc/os-release
run "uptime/load" uptime
run "cpus" nproc
run "architecture (worker image must match)" uname -m
run "memory" free -h
run "swap" swapon --show
run "filesystems" df -hT -x tmpfs -x devtmpfs -x overlay
run "block devices" lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINTS
run "inode usage of /" df -i /

section "kernel settings relevant to the gateway"
for k in net.ipv4.ip_forward net.ipv6.conf.all.forwarding fs.protected_hardlinks fs.protected_symlinks \
  vm.swappiness vm.dirty_ratio vm.dirty_background_ratio; do
  printf '%s = %s\n' "$k" "$(sysctl -n "$k" 2>/dev/null || echo n/a)"
done

section "network"
run "addresses" ip -brief address
run "IPv4 routes" ip route
run "IPv6 routes" ip -6 route
run "policy routing rules" ip rule
run "listening sockets" ss -tulpn
printf '\n--- gateway ports already in use (80, 443, 2022, 8080, 8090, 18080)\n'
ss -Htuln 2>/dev/null | awk '{print $5}' | grep -E ':(80|443|2022|8080|8090|18080)$' | redact || true

section "WireGuard (must stay untouched)"
if command -v wg >/dev/null 2>&1; then
  for ifc in $(wg show interfaces 2>/dev/null); do
    printf '\n--- interface %s\n' "$ifc"
    printf 'listen-port: %s\n' "$(wg show "$ifc" listen-port 2>/dev/null)"
    printf 'peers: %s\n' "$(wg show "$ifc" peers 2>/dev/null | wc -l)"
    now=$(date +%s)
    wg show "$ifc" latest-handshakes 2>/dev/null | awk -v now="$now" '{ if ($2 == 0) print "peer handshake: never"; else print "peer handshake age (s): " now-$2 }'
    wg show "$ifc" allowed-ips 2>/dev/null | awk '{ $1 = "<peer>"; print }' | redact
    printf 'transfer (rx tx bytes): '
    wg show "$ifc" transfer 2>/dev/null | awk '{ rx += $2; tx += $3 } END { print rx, tx }'
  done
  [[ -z "$(wg show interfaces 2>/dev/null)" ]] && echo "no active WireGuard interfaces reported"
else
  echo "(wg not installed or not in PATH)"
fi
run "wg-quick units" systemctl list-units --all --no-pager 'wg-quick@*'

section "firewall (read-only dumps)"
run "ufw" ufw status verbose
run "firewalld" firewall-cmd --state
run "nftables ruleset" nft list ruleset
run "iptables (IPv4)" iptables-save
run "iptables (IPv6)" ip6tables-save

section "docker"
run "docker version" docker version --format '{{.Server.Version}} (API {{.Server.APIVersion}})'
run "docker info" docker info
run "docker daemon.json" cat /etc/docker/daemon.json
run "running containers" docker ps --format '{{.Names}}\t{{.Image}}\t{{.Ports}}'
run "docker networks" docker network ls
run "docker disk usage" docker system df

section "summary hints"
mem_avail=$(awk '/MemAvailable/ {print int($2/1024)}' /proc/meminfo 2>/dev/null)
root_free=$(df -BG --output=avail / 2>/dev/null | tail -1 | tr -dc '0-9')
echo "MemAvailable: ${mem_avail:-?} MiB (gateway needs ~250-700 MiB incl. page cache headroom)"
echo "Free on /: ${root_free:-?} GiB (a 10 GiB upload needs >= 10 GiB + 4 GiB reserve + pending archives)"
echo "Done. Review the output, then share it (it is redacted) before any change is planned."
