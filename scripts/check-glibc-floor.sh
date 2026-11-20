#!/usr/bin/env bash
set -euo pipefail

# Usage: check-glibc-floor.sh FLOOR EXECUTABLE...
#
# Fails when an executable needs a glibc newer than FLOOR. The answer is read
# from the symbol versions the dynamic loader checks before it starts the
# program (the ELF version requirements), so nothing is executed and the check
# runs on any host.
if [[ $# -lt 2 || ! "$1" =~ ^[0-9]+\.[0-9]+$ ]]; then
  printf 'usage: %s FLOOR EXECUTABLE...\n' "$0" >&2
  exit 2
fi

python3 - "$@" <<'PY'
import struct
import sys

SHT_GNU_VERNEED = 0x6FFFFFFE


def version_requirements(path):
    """Yields (library, version name) for every version the loader checks."""
    with open(path, "rb") as executable:
        data = executable.read()
    if data[:6] != b"\x7fELF\x02\x01":
        raise ValueError("not a little-endian 64-bit ELF file")
    (section_table,) = struct.unpack_from("<Q", data, 0x28)
    entry_size, section_count = struct.unpack_from("<HH", data, 0x3A)
    sections = [
        struct.unpack_from("<IIQQQQII", data, section_table + index * entry_size)
        for index in range(section_count)
    ]
    for _, kind, _, _, offset, _, link, entries in sections:
        if kind != SHT_GNU_VERNEED:
            continue
        strings = sections[link][4]

        def text(at):
            start = strings + at
            return data[start : data.index(b"\0", start)].decode("ascii")

        need = offset
        for _ in range(entries):
            _, count, library, first, next_need = struct.unpack_from("<HHIII", data, need)
            version = need + first
            for _ in range(count):
                _, _, _, name, next_version = struct.unpack_from("<IHHII", data, version)
                yield text(library), text(name)
                version += next_version
            need += next_need


def parse(version):
    return tuple(int(part) for part in version.split("."))


def glibc_version(name):
    version = name[len("GLIBC_") :]
    if not all(part.isdigit() for part in version.split(".")):
        raise ValueError(f"{name} is not a glibc release")
    return parse(version)


floor = sys.argv[1]
failed = False
for path in sys.argv[2:]:
    try:
        names = [name for _, name in version_requirements(path) if name.startswith("GLIBC_")]
        needed = max((glibc_version(name) for name in names), default=None)
    except (OSError, ValueError, IndexError, struct.error) as error:
        print(f"{path}: cannot read its glibc requirement: {error}", file=sys.stderr)
        failed = True
        continue
    if needed is None:
        print(f"{path}: no glibc requirement found", file=sys.stderr)
        failed = True
    elif needed > parse(floor):
        version = ".".join(str(part) for part in needed)
        print(f"{path} needs glibc {version}, above the floor {floor}", file=sys.stderr)
        failed = True
    else:
        version = ".".join(str(part) for part in needed)
        print(f"{path}: needs glibc {version}, floor {floor}")
sys.exit(1 if failed else 0)
PY
