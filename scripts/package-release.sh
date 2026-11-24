#!/usr/bin/env bash
set -euo pipefail

default_arch="$(uname -m)"
if [[ "${default_arch}" == "arm64" ]]; then
  default_arch="aarch64"
fi
target_name="${1:-$(uname -s | tr '[:upper:]' '[:lower:]')-${default_arch}}"
case "${target_name}" in
  darwin-aarch64|darwin-x86_64|linux-x86_64) ;;
  *)
    printf 'unsupported release target: %s\n' "${target_name}" >&2
    exit 1
    ;;
esac
package_id="$(cargo pkgid --locked)"
version="${package_id##*#}"
version="${version##*@}"
package="prismtty-${version}-${target_name}"
dist_dir="dist"
stage="${dist_dir}/${package}"
source_date_epoch="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct HEAD)}"
if [[ ! "${source_date_epoch}" =~ ^[0-9]+$ ]]; then
  printf 'invalid SOURCE_DATE_EPOCH: %s\n' "${source_date_epoch}" >&2
  exit 1
fi
export SOURCE_DATE_EPOCH="${source_date_epoch}"

cargo build --locked --release --bins

# Where the build above put the binaries: cargo builds in CARGO_TARGET_DIR
# when it is set.
release_dir="${CARGO_TARGET_DIR:-target}/release"

# The archive name promises an operating system and a CPU, so a binary that
# says otherwise must not ship under it. A universal Mach-O is refused under
# both macOS labels: each archive is for one CPU. Content that is neither ELF
# nor Mach-O is nothing cargo builds for these targets and is left alone.
#
# The binaries also have to load PCRE2 from the system, as the README says
# and the Homebrew formula's dependency assumes. When pkg-config does not
# find libpcre2-8, or PCRE2_SYS_STATIC is set, pcre2-sys compiles the copy it
# bundles into them and says nothing; such a build runs, and never gets a
# PCRE2 fix from the system. What a binary loads is read from what its loader
# reads: the DT_NEEDED entries of an ELF file, the LC_LOAD_DYLIB commands of
# a Mach-O file. A file that ends inside its own header is left alone: nothing
# can run it, so it holds no PCRE2 of either kind.
RELEASE_DIR="${release_dir}" TARGET_NAME="${target_name}" python3 - <<'PY'
import os
from pathlib import Path
import struct

EXPECTED = {
    "darwin-aarch64": "Mach-O arm64",
    "darwin-x86_64": "Mach-O x86_64",
    "linux-x86_64": "ELF x86_64",
}
ELF_MACHINES = {0x3E: "x86_64", 0xB7: "aarch64"}
MACH_O_CPUS = {0x01000007: "x86_64", 0x0100000C: "arm64"}
# Every Mach-O magic number, with its byte order and what sets it apart from
# the 64-bit little-endian kind the macOS targets use.
MACH_O_MAGICS = {
    b"\xcf\xfa\xed\xfe": ("<", ""),
    b"\xfe\xed\xfa\xcf": (">", " (big-endian)"),
    b"\xce\xfa\xed\xfe": ("<", " (32-bit)"),
    b"\xfe\xed\xfa\xce": (">", " (32-bit, big-endian)"),
}
UNIVERSAL_MAGICS = (
    b"\xca\xfe\xba\xbe",
    b"\xca\xfe\xba\xbf",
    b"\xbe\xba\xfe\xca",
    b"\xbf\xba\xfe\xca",
)


def describe(header):
    magic = header[:4]
    if magic == b"\x7fELF":
        if len(header) < 20:
            return "ELF (truncated)"
        byte_order = "<" if header[5] == 1 else ">"
        (machine,) = struct.unpack_from(f"{byte_order}H", header, 18)
        kind = f"ELF {ELF_MACHINES.get(machine, hex(machine))}"
        # The machine number is not enough: the x32 ABI and other systems
        # such as FreeBSD use the x86_64 number too. A Linux executable is
        # 64-bit, little-endian and marked for the System V or GNU/Linux ABI.
        if header[4] != 2:
            kind += " (not 64-bit)"
        if header[5] != 1:
            kind += " (big-endian)"
        if header[7] not in (0, 3):
            kind += f" (OS/ABI {header[7]})"
        return kind
    if magic in MACH_O_MAGICS:
        if len(header) < 8:
            return "Mach-O (truncated)"
        byte_order, variant = MACH_O_MAGICS[magic]
        (cpu,) = struct.unpack_from(f"{byte_order}I", header, 4)
        return f"Mach-O {MACH_O_CPUS.get(cpu, hex(cpu))}{variant}"
    if magic in UNIVERSAL_MAGICS:
        return "Mach-O universal"
    return None


ELF_HEADER_SIZE, MACH_O_HEADER_SIZE = 64, 32
PT_LOAD, PT_DYNAMIC = 1, 2
DT_NULL, DT_NEEDED, DT_STRTAB = 0, 1, 5
LC_LOAD_DYLIB = 0xC


def loaded_libraries(data):
    """The libraries the loader has to find before it starts the program."""
    if data[:4] == b"\x7fELF":
        (segment_table,) = struct.unpack_from("<Q", data, 0x20)
        entry_size, segment_count = struct.unpack_from("<HH", data, 0x36)
        # Type, flags, file offset, address, physical address, size in the file.
        segments = [
            struct.unpack_from("<IIQQQQ", data, segment_table + index * entry_size)
            for index in range(segment_count)
        ]
        dynamic = [segment for segment in segments if segment[0] == PT_DYNAMIC]
        if not dynamic:
            raise ValueError("no dynamic section")
        _, _, offset, _, _, size = dynamic[0]
        entries = []
        for at in range(offset, offset + size, 16):
            entries.append(struct.unpack_from("<qQ", data, at))
            if entries[-1][0] == DT_NULL:
                break
        # The string table is named by its address; the loadable segments say
        # where in the file that is.
        (strings_address,) = [value for tag, value in entries if tag == DT_STRTAB]
        (strings,) = [
            file_offset + strings_address - address
            for kind, _, file_offset, address, _, file_size in segments
            if kind == PT_LOAD and address <= strings_address < address + file_size
        ]
        names = []
        for tag, value in entries:
            if tag == DT_NEEDED:
                start = strings + value
                names.append(data[start : data.index(b"\0", start)].decode("ascii"))
        return names
    (command_count,) = struct.unpack_from("<I", data, 16)
    names = []
    at = 32
    for _ in range(command_count):
        command, size = struct.unpack_from("<II", data, at)
        if size < 8:
            raise ValueError("a load command smaller than its own header")
        if command == LC_LOAD_DYLIB:
            (name,) = struct.unpack_from("<I", data, at + 8)
            names.append(data[at + name : at + size].split(b"\0", 1)[0].decode("ascii"))
        at += size
    return names


target = os.environ["TARGET_NAME"]


def loads_pcre2(library):
    if target == "linux-x86_64":
        return library == "libpcre2-8.so.0"
    name = library.rsplit("/", 1)[-1]
    return name.startswith("libpcre2-8.") and name.endswith(".dylib")


for name in ("prismtty", "ptty", "ct"):
    path = Path(os.environ["RELEASE_DIR"]) / name
    with path.open("rb") as binary:
        found = describe(binary.read(20))
    if found is not None and found != EXPECTED[target]:
        raise SystemExit(f"{path} is {found}, but {target} needs {EXPECTED[target]}")

for name in ("prismtty", "ptty", "ct"):
    path = Path(os.environ["RELEASE_DIR"]) / name
    data = path.read_bytes()
    if describe(data[:20]) is None:
        continue
    if len(data) < (ELF_HEADER_SIZE if data[:4] == b"\x7fELF" else MACH_O_HEADER_SIZE):
        continue
    try:
        libraries = loaded_libraries(data)
    except (ValueError, IndexError, UnicodeDecodeError, struct.error) as error:
        raise SystemExit(f"{path}: cannot read the libraries it loads: {error}")
    if not any(loads_pcre2(library) for library in libraries):
        wanted = "libpcre2-8.so.0" if target == "linux-x86_64" else "libpcre2-8"
        raise SystemExit(
            f"{path} does not load {wanted} at run time, so it does not use the "
            f"system's PCRE2. It loads {', '.join(libraries) or 'no library'}."
        )
PY

rm -rf "${stage}"
mkdir -p "${stage}/completions" "${stage}/profiles"

cp "${release_dir}/prismtty" "${release_dir}/ptty" "${release_dir}/ct" "${stage}/"
cp LICENSE README.md "${stage}/"
cp completions/prismtty.bash completions/prismtty.fish completions/_prismtty "${stage}/completions/"
cp profiles/custom-router.example.yml "${stage}/profiles/"

archive="${dist_dir}/${package}.tar.gz"
STAGE="${stage}" PACKAGE="${package}" ARCHIVE="${archive}" \
  SOURCE_DATE_EPOCH="${source_date_epoch}" python3 - <<'PY'
import gzip
import os
from pathlib import Path
import tarfile

stage = Path(os.environ["STAGE"])
package = os.environ["PACKAGE"]
archive = Path(os.environ["ARCHIVE"])
source_date_epoch = int(os.environ["SOURCE_DATE_EPOCH"])
entries = [stage, *sorted(stage.rglob("*"), key=lambda path: path.relative_to(stage).as_posix())]

with archive.open("wb") as archive_file:
    with gzip.GzipFile(
        filename="",
        mode="wb",
        fileobj=archive_file,
        compresslevel=9,
        mtime=source_date_epoch,
    ) as compressed:
        with tarfile.open(
            fileobj=compressed,
            mode="w",
            format=tarfile.USTAR_FORMAT,
        ) as tar:
            for path in entries:
                relative = path.relative_to(stage)
                archive_name = Path(package) / relative
                info = tar.gettarinfo(str(path), archive_name.as_posix())
                info.uid = 0
                info.gid = 0
                info.uname = ""
                info.gname = ""
                info.mtime = source_date_epoch
                if info.isdir():
                    info.mode = 0o755
                    tar.addfile(info)
                elif info.isfile():
                    info.mode = 0o755 if path.stat().st_mode & 0o111 else 0o644
                    with path.open("rb") as source_file:
                        tar.addfile(info, source_file)
                else:
                    raise SystemExit(f"unsupported staged file type: {path}")
PY

(cd "${dist_dir}" && shasum -a 256 "${package}.tar.gz") > "${dist_dir}/${package}.tar.gz.sha256"

printf '%s\n' "${archive}"
