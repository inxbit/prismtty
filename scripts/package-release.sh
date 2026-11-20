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
# says otherwise must not ship under it. Content that is neither ELF nor
# 64-bit Mach-O is nothing cargo builds for these targets and is left alone.
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


def describe(header):
    if header[:4] == b"\x7fELF" and len(header) >= 20:
        byte_order = "<" if header[5] == 1 else ">"
        (machine,) = struct.unpack_from(f"{byte_order}H", header, 18)
        return f"ELF {ELF_MACHINES.get(machine, hex(machine))}"
    if header[:4] == b"\xcf\xfa\xed\xfe" and len(header) >= 8:
        (cpu,) = struct.unpack_from("<I", header, 4)
        return f"Mach-O {MACH_O_CPUS.get(cpu, hex(cpu))}"
    if header[:4] in (b"\xca\xfe\xba\xbe", b"\xca\xfe\xba\xbf"):
        return "Mach-O universal"
    return None


target = os.environ["TARGET_NAME"]
for name in ("prismtty", "ptty", "ct"):
    path = Path(os.environ["RELEASE_DIR"]) / name
    with path.open("rb") as binary:
        found = describe(binary.read(20))
    if found is not None and found != EXPECTED[target]:
        raise SystemExit(f"{path} is {found}, but {target} needs {EXPECTED[target]}")
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
