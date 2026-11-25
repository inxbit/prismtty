#!/usr/bin/env bash
set -euo pipefail

# Byte-wise matching. In a UTF-8 locale macOS gives up on a line that is not
# valid UTF-8, and a path or address on that line goes unreported.
export LC_ALL=C

path_pattern='(/Users|/home)/[^[:space:]"<>]+/(Desktop|Documents|Downloads)|(^|[^[:alnum:]_/.-])Desktop/[A-Za-z0-9_.-]+\.(txt|log|trace|pcap|pcapng|cfg|conf)'
host_pattern='(^|[^[:alnum:]_.-])[A-Za-z0-9][A-Za-z0-9.-]*\.(corp|internal|lab|lan|local|localdomain)(\.[A-Za-z0-9.-]+)?([^[:alnum:]_.-]|$)'
exclude=(
  ':!scripts/privacy-scan.sh'
)

if git grep --untracked --exclude-standard -n -E "$path_pattern|$host_pattern" -- . "${exclude[@]}"; then
  echo "Sensitive real-world capture marker found. Use synthetic fixtures instead."
  exit 1
fi

marker_pattern="$path_pattern|$host_pattern" python3 - <<'PY'
import bisect
import ipaddress
import os
import re
import stat
import struct
import subprocess
import sys
import tempfile
import zlib

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

ip_pattern = re.compile(r"(?<![0-9])(?:[0-9]{1,3}\.){3}[0-9]{1,3}(?![0-9])")


def outside_documentation_ipv4(line):
    for match in ip_pattern.finditer(line):
        try:
            address = ipaddress.ip_address(match.group(0))
        except ValueError:
            # Devices print zero-padded octets (10.001.2.3), which ipaddress
            # refuses. They name the same address.
            try:
                address = ipaddress.ip_address(
                    ".".join(str(int(octet)) for octet in match.group(0).split("."))
                )
            except ValueError:
                continue
        if (
            match.group(0) not in allowed_netmasks
            and match.group(0) not in allowed_wildcard_masks
            and not any(
                address in network for network in allowed_networks
            )
        ):
            return True
    return False


ipv6_pattern = re.compile(
    r"(?<![0-9A-Za-z_])"
    r"(?:\[(?P<bracketed>[0-9A-Fa-f:.]+)(?:%(?P<bracket_zone>[0-9A-Za-z_.-]+))?\]"
    r"|(?P<plain>[0-9A-Fa-f:.]*:[0-9A-Fa-f:.]+)"
    r"(?:%(?P<plain_zone>[0-9A-Za-z_.-]+))?)"
    r"(?:/(?P<prefix>[0-9]{1,3}))?"
    r"(?![0-9A-Za-z_])"
)


def outside_documentation_ipv6(*texts):
    for text in texts:
        for match in ipv6_pattern.finditer(text):
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
                return True
    return False


# Images are published with the text their tools put inside them, and
# CONTRIBUTING.md asks for generated PNGs to carry their prompt or capture
# notes in a text chunk. `git grep -I` skips binary files, so the text chunks
# of PNG files (tEXt, zTXt, iTXt) are read here and go through the same
# checks. Nothing else in a PNG is read, and a binary file of any other kind
# is not read at all: megabytes of compressed data or font tables spell
# addresses by chance.
#
# A PNG is read as untrusted input. A chunk that runs past the end of the
# file, compressed text that does not end where it should, and anything over
# the limits below fail the scan with the name of the file: text behind it
# would otherwise go unread. Of 23,902 PNG files on one system the most text
# in one file was 673,702 bytes in 9,414 pieces.
READ_LIMIT = 32 * 1024 * 1024
TEXT_LIMIT = 8 * 1024 * 1024
PIECES_LIMIT = 100_000
PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"


def unreadable(path, problem):
    print(f"{path}: {problem}; the privacy scan cannot check it.")
    sys.exit(1)


def inflated(path, stream):
    """The whole text of a zlib stream in a PNG chunk."""
    inflater = zlib.decompressobj()
    try:
        text = inflater.decompress(stream, TEXT_LIMIT)
    except zlib.error as error:
        unreadable(path, f"compressed text that does not inflate ({error})")
    if inflater.unconsumed_tail:
        unreadable(path, f"compressed text larger than {TEXT_LIMIT} bytes")
    if not inflater.eof:
        unreadable(path, "compressed text that is cut short")
    if inflater.unused_data:
        unreadable(path, "compressed text with more data behind it")
    return text


def png_text(path, data):
    """The fields of the text chunks of a PNG file, up to its IEND chunk."""
    at = len(PNG_SIGNATURE)
    while at + 8 <= len(data):
        (length,) = struct.unpack_from(">I", data, at)
        kind = data[at + 4 : at + 8]
        # The content and the checksum behind it.
        if at + 12 + length > len(data):
            unreadable(path, "a PNG chunk that runs past the end of the file")
        body = data[at + 8 : at + 8 + length]
        at += 12 + length
        if kind == b"IEND":
            return
        if kind == b"tEXt":
            yield body
        elif kind == b"zTXt":
            # Keyword, NUL, compression method, compressed text.
            keyword, _, rest = body.partition(b"\0")
            yield keyword
            yield inflated(path, rest[1:])
        elif kind == b"iTXt":
            # Keyword, NUL, compressed flag, compression method, language,
            # NUL, translated keyword, NUL, text. The flag is 0 or 1; a
            # reader in wide use inflates the text for any other value too.
            keyword, _, rest = body.partition(b"\0")
            if rest[:1] in (b"", b"\0"):
                yield body
                continue
            fields = rest[2:].split(b"\0", 2)
            if len(fields) != 3:
                unreadable(path, "a compressed text chunk that is cut short")
            yield keyword
            yield from fields[:2]
            yield inflated(path, fields[2])


line_pattern = re.compile(rb"[^\x00-\x1f\x7f]+")


def text_lines(field):
    """What the checks read from a field: the text between control
    characters, however short."""
    for match in line_pattern.finditer(field):
        yield match.group(0).decode("utf-8", "replace")


# No address is longer than 45 characters. The IPv6 pattern takes time
# quadratic in the length of a run of the characters an address is made of, so
# a longer run is taken out of PNG text, and of the name of the PNG file,
# before the pattern sees it.
overlong_address_pattern = re.compile(r"[0-9A-Fa-f:.]{65,}")

text_files = subprocess.run(
    ["git", "grep", "--untracked", "--exclude-standard", "-I", "-l", "-z", "-e", "^", "--", "."],
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=False,
)
if text_files.returncode not in (0, 1):
    sys.stderr.buffer.write(text_files.stderr)
    sys.exit(text_files.returncode)
# The files the greps below skip as binary.
binary_paths = sorted(
    set(tracked_or_untracked.stdout.split(b"\0"))
    - set(text_files.stdout.split(b"\0"))
    - {b"", b"scripts/privacy-scan.sh"}
)



def open_in_repository(root, raw_path):
    """A descriptor for the file at a path git lists, or None when the path
    cannot be opened as git grep would read it. No part of the path may be a
    symbolic link: each directory is opened from the one above it, so a link
    put in the place of a directory leads nowhere. No open waits on a pipe
    or a device, whichever part of the path one stands in for."""
    *parents, name = raw_path.split(b"/")
    unfollowed = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK
    directory = os.dup(root)
    try:
        for parent in parents:
            below = os.open(parent, unfollowed | os.O_DIRECTORY, dir_fd=directory)
            os.close(directory)
            directory = below
        return os.open(name, unfollowed, dir_fd=directory)
    except OSError:
        return None
    finally:
        os.close(directory)


# Each line of PNG text is checked as it is read, on its own: the name of the
# file is checked once, so what a line costs does not grow with the path.
# The path and host search numbers the lines it is sent, and `first_lines`
# says which file a number belongs to.
marker_matches = tempfile.TemporaryFile()
marker_search = subprocess.Popen(
    ["grep", "-a", "-n", "-E", os.environ["marker_pattern"]],
    stdin=subprocess.PIPE,
    stdout=marker_matches,
)
lines_sent = 0
first_lines = []
first_line_paths = []
embedded_ipv4_findings = []
embedded_ipv6_findings = []
repository = os.open(".", os.O_RDONLY | os.O_DIRECTORY)
for raw_path in binary_paths:
    path = os.fsdecode(raw_path)
    # Like git grep, this reads regular files only, and reaches them through
    # directories only: a link is never followed out of the repository.
    descriptor = open_in_repository(repository, raw_path)
    if descriptor is None:
        continue
    with os.fdopen(descriptor, "rb") as binary_file:
        status = os.fstat(descriptor)
        if not stat.S_ISREG(status.st_mode):
            continue
        if binary_file.read(len(PNG_SIGNATURE)) != PNG_SIGNATURE:
            continue
        if status.st_size > READ_LIMIT:
            unreadable(path, f"a PNG file larger than {READ_LIMIT} bytes")
        data = PNG_SIGNATURE + binary_file.read(READ_LIMIT)
    text_size = pieces = 0
    for field in png_text(path, data):
        text_size += len(field)
        if text_size > TEXT_LIMIT:
            unreadable(path, f"more than {TEXT_LIMIT} bytes of text")
        pieces += 1
        for text in text_lines(field):
            pieces += 1
            if pieces > PIECES_LIMIT:
                break
            if not first_line_paths or first_line_paths[-1] != path:
                first_lines.append(lines_sent + 1)
                first_line_paths.append(path)
                # A file with text is named in what is reported, so its name
                # is held to the same rule as the names of text files.
                if outside_documentation_ipv4(path):
                    embedded_ipv4_findings.append(f"{path}:embedded:")
                if outside_documentation_ipv6(overlong_address_pattern.sub(" ", path)):
                    embedded_ipv6_findings.append(f"{path}:embedded:")
            lines_sent += 1
            marker_search.stdin.write(text.encode("utf-8") + b"\n")
            if outside_documentation_ipv4(text):
                embedded_ipv4_findings.append(f"{path}:embedded:{text}")
            # As for the lines of text files, only text with a colon can
            # hold an address.
            if ":" in text and outside_documentation_ipv6(
                overlong_address_pattern.sub(" ", text)
            ):
                embedded_ipv6_findings.append(f"{path}:embedded:{text}")
        if pieces > PIECES_LIMIT:
            unreadable(path, f"text in more than {PIECES_LIMIT} pieces")
os.close(repository)

marker_search.stdin.close()
marker_status = marker_search.wait()
if marker_status not in (0, 1):
    sys.exit(marker_status)
if marker_status == 0:
    marker_matches.seek(0)
    for match in marker_matches.read().decode("utf-8", "replace").split("\n")[:-1]:
        number, _, text = match.partition(":")
        path = first_line_paths[bisect.bisect_right(first_lines, int(number)) - 1]
        print(f"{path}:embedded:{text}")
    print("Sensitive real-world capture marker found. Use synthetic fixtures instead.")
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
        r"(^|[^0-9])([0-9]{1,3}\.){3}[0-9]{1,3}([^0-9]|$)",
        "--",
        ".",
        *exclude,
    ],
    encoding="utf-8",
    errors="replace",
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=False,
)
if result.returncode not in (0, 1):
    sys.stderr.write(result.stderr)
    sys.exit(result.returncode)

findings = [
    line for line in result.stdout.splitlines() if outside_documentation_ipv4(line)
] + embedded_ipv4_findings

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
        # NUL after the path and the line number, so neither can run into an
        # address that begins the line.
        "-z",
        "-I",
        "-E",
        ":",
        "--",
        ".",
        *exclude,
    ],
    encoding="utf-8",
    errors="replace",
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=False,
)
if result.returncode not in (0, 1):
    sys.stderr.write(result.stderr)
    sys.exit(result.returncode)

findings = []
for record in result.stdout.split("\n"):
    path, _, rest = record.partition("\0")
    number, _, content = rest.partition("\0")
    if outside_documentation_ipv6(path, content):
        findings.append(f"{path}:{number}:{content}")
findings += embedded_ipv6_findings

if findings:
    print("IPv6 address outside documentation ranges found:")
    print("\n".join(findings))
    sys.exit(1)
PY
