#!/usr/bin/env bash
set -euo pipefail

path_pattern='(/Users|/home)/[^[:space:]"<>]+/(Desktop|Documents|Downloads)|(^|[^[:alnum:]_/.-])Desktop/[A-Za-z0-9_.-]+\.(txt|log|trace|pcap|pcapng|cfg|conf)'
host_pattern='(^|[^[:alnum:]_.-])[A-Za-z0-9][A-Za-z0-9.-]*\.(corp|internal|lab|lan|local|localdomain)(\.[A-Za-z0-9.-]+)?([^[:alnum:]_.-]|$)'
exclude=(
  ':!Cargo.lock'
  ':!.github/workflows/ci.yml'
  ':!scripts/privacy-scan.sh'
)

if git grep -n -E "$path_pattern|$host_pattern" -- . "${exclude[@]}"; then
  echo "Sensitive real-world capture marker found. Use synthetic fixtures instead."
  exit 1
fi

python3 - <<'PY'
import ipaddress
import re
import subprocess
import sys

allowed_networks = [
    ipaddress.ip_network("0.0.0.0/8"),
    ipaddress.ip_network("127.0.0.0/8"),
    ipaddress.ip_network("192.0.2.0/24"),
    ipaddress.ip_network("198.51.100.0/24"),
    ipaddress.ip_network("203.0.113.0/24"),
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
exclude = [
    ":!Cargo.lock",
    ":!scripts/privacy-scan.sh",
]
result = subprocess.run(
    [
        "git",
        "grep",
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
        if match.group(0) not in allowed_netmasks and not any(
            address in network for network in allowed_networks
        ):
            findings.append(line)
            break

if findings:
    print("IPv4 address outside documentation ranges found:")
    print("\n".join(findings))
    sys.exit(1)
PY
