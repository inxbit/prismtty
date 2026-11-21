#!/usr/bin/env bash
set -euo pipefail

# Builds the linux-x86_64 release archive, measures the glibc it needs and
# runs it. The pull request CI and the release workflow both call this script
# and nothing else, so a tag never builds the archive in a way that a pull
# request has not already run.
#
# A binary runs on the glibc it was linked against and on newer ones, so the
# build happens in an older userland than the CI runner's:
# docker.io/library/ubuntu:jammy-20261110, Ubuntu 22.04 with glibc 2.35. The
# digest names its multi-platform index. Ubuntu builds for every x86-64 CPU;
# a distribution built for x86-64-v2 would mark the binaries as needing it.
image="docker.io/library/ubuntu@sha256:9abcaffc92a5279fff3a6a41635c0800ea33e528425fb389658806476e7defde"
# The compiler and libraries come from the Ubuntu archive as it was at this
# moment, so a rebuild of the same commit installs the same packages.
snapshot="20261115T000000Z"
# The newest glibc the binaries may require. The check below measures what
# they do require; it can be lower than the glibc they were linked against.
glibc_floor="2.34"
target_name="linux-x86_64"
# The bare image has no CA certificates. apt and cargo check the hosts they
# download from against the host's.
ca_certificates="/etc/ssl/certs/ca-certificates.crt"

# The container borrows the host's Rust toolchain, so the host has to be able
# to run it and it has to be the version every release build uses.
if [[ "$(uname -s) $(uname -m)" != "Linux x86_64" ]]; then
  printf 'the %s archive is built on a Linux x86_64 host, not on %s %s\n' \
    "${target_name}" "$(uname -s)" "$(uname -m)" >&2
  exit 1
fi
rust_version="$(awk -F '"' '/^rust-version = / { print $2; exit }' Cargo.toml).0"
if [[ "$(rustc --version)" != "rustc ${rust_version} "* ]]; then
  printf 'release builds use Rust %s; the host has %s\n' "${rust_version}" "$(rustc --version)" >&2
  exit 1
fi
toolchain="$(rustc --print sysroot)"
version="$(awk -F '"' '/^version = / { print $2; exit }' Cargo.toml)"
package="prismtty-${version}-${target_name}"
source_date_epoch="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct HEAD)}"

# Every fetch is bounded and retried: a stalled registry or mirror costs
# minutes, not the job. A fetch that ignores the time limit is killed.
pulled=""
for attempt in 1 2 3; do
  if timeout --kill-after=30 600 docker pull --platform linux/amd64 "${image}"; then
    pulled="yes"
    break
  fi
  echo "docker pull attempt ${attempt} failed; retrying"
  sleep 5
done
if [[ -z "${pulled}" ]]; then
  echo "docker could not pull the build image" >&2
  exit 1
fi

# The scripts for the containers are read as they stand, not through a command
# substitution: Bash 3.2 misjudges where one ends when the text holds a `case`.
IFS= read -r -d '' build <<'CONTAINER' || true
set -euo pipefail
trap 'chown -R "${HOST_OWNER}" dist 2>/dev/null || true' EXIT

# Runs a download at most three times, ten minutes each.
fetch() {
  local attempt
  for attempt in 1 2 3; do
    if timeout --kill-after=30 600 "$@"; then
      return 0
    fi
    echo "attempt ${attempt} failed: $*"
    sleep 5
  done
  echo "giving up: $*" >&2
  return 1
}

cat > /etc/apt/sources.list <<SOURCES
deb https://snapshot.ubuntu.com/ubuntu/${SNAPSHOT} jammy main
deb https://snapshot.ubuntu.com/ubuntu/${SNAPSHOT} jammy-updates main
deb https://snapshot.ubuntu.com/ubuntu/${SNAPSHOT} jammy-security main
SOURCES
apt_options=(
  -o Acquire::https::CaInfo=/host/ca-certificates.crt
  -o Acquire::Retries=3
  -o Acquire::http::Timeout=30
  -o Acquire::https::Timeout=30
)
export DEBIAN_FRONTEND=noninteractive
fetch apt-get "${apt_options[@]}" update
fetch apt-get "${apt_options[@]}" install --yes --no-install-recommends \
  gcc libc6-dev pkg-config libpcre2-dev python3 perl

export PATH="/opt/rust/bin:${PATH}"
export CARGO_HTTP_CAINFO=/host/ca-certificates.crt
# cargo keeps its downloads and its output inside the container. The crates
# are downloaded first, so the build itself needs no network.
export CARGO_HOME=/tmp/cargo-home CARGO_TARGET_DIR=/tmp/cargo-target
fetch cargo fetch --locked --target x86_64-unknown-linux-gnu
export CARGO_NET_OFFLINE=true
bash scripts/package-release.sh "${TARGET_NAME}"
CONTAINER
# Package scripts and build scripts run as root in the container, and this
# script goes on to run more of the checkout afterwards. The container gets
# the checkout read-only; dist is the one place it can write.
mkdir -p dist
docker run --rm --platform linux/amd64 \
  --volume "${PWD}:/work:ro" --volume "${PWD}/dist:/work/dist" --workdir /work \
  --volume "${toolchain}:/opt/rust:ro" \
  --mount "type=bind,source=${ca_certificates},target=/host/ca-certificates.crt,readonly" \
  --env SNAPSHOT="${snapshot}" \
  --env SOURCE_DATE_EPOCH="${source_date_epoch}" \
  --env TARGET_NAME="${target_name}" \
  --env HOST_OWNER="$(id -u):$(id -g)" \
  "${image}" bash -c "${build}"

# What ships is the tarball, so everything below reads the executables out of
# it. A member that is missing, or was stored without its execute bit, fails
# here and not on a user's machine.
(cd dist && shasum -a 256 -c "${package}.tar.gz.sha256")
shipped="$(mktemp -d)"
trap 'rm -rf "${shipped}"' EXIT
tar -xzf "dist/${package}.tar.gz" -C "${shipped}"
executables=()
for name in prismtty ptty ct; do
  executable="${shipped}/${package}/${name}"
  if [[ -L "${executable}" || ! -f "${executable}" || ! -x "${executable}" ]]; then
    printf 'dist/%s.tar.gz does not hold an executable %s\n' "${package}" "${name}" >&2
    exit 1
  fi
  executables+=("${executable}")
done

bash scripts/check-glibc-floor.sh "${glibc_floor}" "${executables[@]}"

# The bare image, without the network: the binaries have to start on a stock
# system of that age with nothing added to it.
IFS= read -r -d '' smoke <<'CONTAINER' || true
set -euo pipefail

# Whether the SGR parameters in $1 set a foreground color. The parameters of a
# background color (48) are skipped: its palette index can look like one.
sets_foreground() {
  local -a parameters
  IFS=';' read -r -a parameters <<< "$1"
  local i=0
  while (( i < ${#parameters[@]} )); do
    case "${parameters[i]}" in
      3[0-7] | 9[0-7] | 38)
        return 0
        ;;
      48)
        if [[ "${parameters[i+1]:-}" == 5 ]]; then
          (( i += 2 ))
        else
          (( i += 4 ))
        fi
        ;;
    esac
    (( i += 1 ))
  done
  return 1
}

# The escape sequence directly in front of the address.
styled_address=$'\033''\[([0-9;]*)m192\.0\.2\.1'
for name in prismtty ptty ct; do
  if ! printed="$("/package/${name}" --version)" || [[ "${printed}" != "prismtty ${VERSION}" ]]; then
    echo "${name} --version failed or did not print prismtty ${VERSION}" >&2
    exit 1
  fi
  if ! colored="$(printf '192.0.2.1 down\n' | "/package/${name}" --profile generic)" \
    || [[ ! "${colored}" =~ ${styled_address} ]] || ! sets_foreground "${BASH_REMATCH[1]}"; then
    echo "${name} --profile generic failed or did not color the address" >&2
    exit 1
  fi
  status=0
  "/package/${name}" -- sh -c 'test -t 1 && exit 7' > /dev/null || status=$?
  if [[ "${status}" != 7 ]]; then
    echo "${name} did not run a command on a terminal and return its status" >&2
    exit 1
  fi
done
CONTAINER
docker run --rm --network none --platform linux/amd64 \
  --volume "${shipped}/${package}:/package:ro" \
  --env VERSION="${version}" \
  "${image}" bash -c "${smoke}"
