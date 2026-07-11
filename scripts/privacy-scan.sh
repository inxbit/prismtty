#!/usr/bin/env bash
set -euo pipefail

path_pattern='(/Users|/home)/[^[:space:]"<>]+/(Desktop|Documents|Downloads)|(^|[^[:alnum:]_/.-])Desktop/[A-Za-z0-9_.-]+\.(txt|log|trace|pcap|pcapng|cfg|conf)'
host_pattern='(^|[^[:alnum:]_.-])[A-Za-z0-9][A-Za-z0-9.-]*\.(corp|internal|lab|lan|local|localdomain)(\.[A-Za-z0-9.-]+)?([^[:alnum:]_.-]|$)'
exclude=(
  ':!scripts/privacy-scan.sh'
)

if git grep --untracked --exclude-standard -n -E "$path_pattern|$host_pattern" -- . "${exclude[@]}"; then
  echo "Sensitive real-world capture marker found. Use synthetic fixtures instead."
  exit 1
fi

python3 - <<'PY'
import ipaddress
import re
import subprocess
import sys

capture_path_pattern = re.compile(
    rb"(?:\.pcapng|\.pcap|\.trace|\.cap)(?:\.(?:gz|bz2|xz|zst|lz4|zip))*\Z",
    re.IGNORECASE,
)
tracked_or_untracked = subprocess.run(
    ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=False,
)
if tracked_or_untracked.returncode != 0:
    sys.stderr.buffer.write(tracked_or_untracked.stderr)
    sys.exit(tracked_or_untracked.returncode)
capture_paths = [
    path
    for path in tracked_or_untracked.stdout.split(b"\0")
    if capture_path_pattern.search(path)
]
if capture_paths:
    print("Capture artifacts are not allowed; use minimal synthetic text fixtures instead.")
    for path in capture_paths:
        print(repr(path))
    sys.exit(1)

allowed_networks = [
    ipaddress.ip_network("0.0.0.0/32"),
    ipaddress.ip_network("127.0.0.0/8"),
    ipaddress.ip_network("192.0.2.0/24"),
    ipaddress.ip_network("198.51.100.0/24"),
    ipaddress.ip_network("203.0.113.0/24"),
]
allowed_ipv6_networks = [
    ipaddress.ip_network("::/128"),
    ipaddress.ip_network("::1/128"),
    ipaddress.ip_network("2001:db8::/32"),
    ipaddress.ip_network("fe80::1/128"),
]
allowed_netmasks = {
    "255.0.0.0",
    "255.255.0.0",
    "255.255.255.0",
    "255.255.255.128",
    "255.255.255.192",
    "255.255.255.224",
    "255.255.255.240",
    "255.255.255.248",
    "255.255.255.252",
}
allowed_wildcard_masks = {
    "0.0.0.255",
}
exclude = [
    ":!scripts/privacy-scan.sh",
]
result = subprocess.run(
    [
        "git",
        "grep",
        "--untracked",
        "--exclude-standard",
        "-n",
        "-I",
        "-E",
        r"(^|[^0-9])([0-9]{1,3}\.){3}[0-9]{1,3}([^0-9]|$)",
        "--",
        ".",
        *exclude,
    ],
    text=True,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=False,
)
if result.returncode not in (0, 1):
    sys.stderr.write(result.stderr)
    sys.exit(result.returncode)

ip_pattern = re.compile(r"(?<![0-9])(?:[0-9]{1,3}\.){3}[0-9]{1,3}(?![0-9])")
findings = []
for line in result.stdout.splitlines():
    for match in ip_pattern.finditer(line):
        try:
            address = ipaddress.ip_address(match.group(0))
        except ValueError:
            continue
        if (
            match.group(0) not in allowed_netmasks
            and match.group(0) not in allowed_wildcard_masks
            and not any(
                address in network for network in allowed_networks
            )
        ):
            findings.append(line)
            break

if findings:
    print("IPv4 address outside documentation ranges found:")
    print("\n".join(findings))
    sys.exit(1)

result = subprocess.run(
    [
        "git",
        "grep",
        "--untracked",
        "--exclude-standard",
        "-n",
        "-I",
        "-E",
        ":",
        "--",
        ".",
        *exclude,
    ],
    text=True,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=False,
)
if result.returncode not in (0, 1):
    sys.stderr.write(result.stderr)
    sys.exit(result.returncode)

ipv6_pattern = re.compile(
    r"(?<![0-9A-Za-z_])"
    r"(?:\[(?P<bracketed>[0-9A-Fa-f:.]+)(?:%(?P<bracket_zone>[0-9A-Za-z_.-]+))?\]"
    r"|(?P<plain>[0-9A-Fa-f:.]*:[0-9A-Fa-f:.]+)"
    r"(?:%(?P<plain_zone>[0-9A-Za-z_.-]+))?)"
    r"(?:/(?P<prefix>[0-9]{1,3}))?"
    r"(?![0-9A-Za-z_])"
)
findings = []
for line in result.stdout.splitlines():
    for match in ipv6_pattern.finditer(line):
        address_text = match.group("bracketed") or match.group("plain")
        try:
            address = ipaddress.ip_address(address_text)
        except ValueError:
            continue
        if not isinstance(address, ipaddress.IPv6Address):
            continue

        prefix = match.group("prefix")
        zone = match.group("bracket_zone") or match.group("plain_zone")
        if zone is not None:
            allowed = False
        elif prefix is None:
            allowed = any(address in network for network in allowed_ipv6_networks)
        else:
            try:
                network = ipaddress.ip_network(f"{address_text}/{prefix}", strict=False)
            except ValueError:
                allowed = False
            else:
                allowed = any(
                    network.subnet_of(allowed_network)
                    for allowed_network in allowed_ipv6_networks
                )
        if not allowed:
            findings.append(line)
            break

if findings:
    print("IPv6 address outside documentation ranges found:")
    print("\n".join(findings))
    sys.exit(1)
PY
