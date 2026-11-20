#!/usr/bin/env bash
set -euo pipefail

# Usage: check-glibc-floor.sh FLOOR EXECUTABLE...
#
# Fails when an executable needs a glibc newer than FLOOR, or a CPU above
# baseline x86-64. Both answers are read from what the dynamic loader checks
# before it starts the program: the symbol versions it requires and the
# "x86 ISA needed" property. Nothing is executed, so the check runs on any
# host.
if [[ $# -lt 2 || ! "$1" =~ ^[0-9]+\.[0-9]+$ ]]; then
  printf 'usage: %s FLOOR EXECUTABLE...\n' "$0" >&2
  exit 2
fi

python3 - "$@" <<'PY'
import struct
import sys

SHT_GNU_VERNEED = 0x6FFFFFFE
PT_GNU_PROPERTY = 0x6474E553
NT_GNU_PROPERTY_TYPE_0 = 5
GNU_PROPERTY_X86_ISA_1_NEEDED = 0xC0008002
# Bit 0 of the property is the baseline every x86-64 CPU has.
ISA_LEVELS_ABOVE_BASELINE = ["x86-64-v2", "x86-64-v3", "x86-64-v4"]


def load(path):
    with open(path, "rb") as executable:
        data = executable.read()
    if data[:6] != b"\x7fELF\x02\x01":
        raise ValueError("not a little-endian 64-bit ELF file")
    return data


def isa_needed(data):
    """Returns the x86-64 levels the loader requires of the CPU, as bits."""
    (segment_table,) = struct.unpack_from("<Q", data, 0x20)
    entry_size, segment_count = struct.unpack_from("<HH", data, 0x36)
    if segment_count == 0:
        raise ValueError("no program headers")
    needed = 0
    for index in range(segment_count):
        kind, _, offset = struct.unpack_from("<IIQ", data, segment_table + index * entry_size)
        if kind != PT_GNU_PROPERTY:
            continue
        name_size, properties_size, note = struct.unpack_from("<III", data, offset)
        if note != NT_GNU_PROPERTY_TYPE_0:
            continue
        # Properties are padded to 8 bytes, and so is the name before them.
        at = (offset + 12 + name_size + 7) & ~7
        end = at + properties_size
        while at + 8 <= end:
            kind, size = struct.unpack_from("<II", data, at)
            if kind == GNU_PROPERTY_X86_ISA_1_NEEDED:
                needed |= struct.unpack_from("<I", data, at + 8)[0]
            at += (8 + size + 7) & ~7
    return needed


def version_requirements(data):
    """Yields (library, version name) for every version the loader checks."""
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
        data = load(path)
        names = [name for _, name in version_requirements(data) if name.startswith("GLIBC_")]
        needed = max((glibc_version(name) for name in names), default=None)
    except (OSError, ValueError, IndexError, struct.error) as error:
        print(f"{path}: cannot read its glibc requirement: {error}", file=sys.stderr)
        failed = True
        continue
    try:
        levels = isa_needed(data)
    except (ValueError, IndexError, struct.error) as error:
        print(f"{path}: cannot read its CPU requirement: {error}", file=sys.stderr)
        failed = True
        continue
    if levels >> 1:
        above = [
            ISA_LEVELS_ABOVE_BASELINE[bit] if bit < 3 else f"bit {bit + 1}"
            for bit in range((levels >> 1).bit_length())
            if levels >> (bit + 1) & 1
        ]
        print(f"{path} needs {', '.join(above)}, above baseline x86-64", file=sys.stderr)
        failed = True
    else:
        print(f"{path}: x86 ISA needed: {'baseline' if levels else 'none stated'}")
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
