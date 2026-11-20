#!/usr/bin/env bash
set -euo pipefail

# Builds the linux-x86_64 release archive, measures the glibc it needs and
# runs it. The pull request CI and the release workflow both call this script
# and nothing else, so a tag never builds the archive in a way that a pull
# request has not already run.
#
# A binary runs on the glibc it was linked against and on newer ones, so the
# build happens in an older userland than the CI runner's:
# docker.io/library/almalinux:9.9-20261116, glibc 2.34. The digest names its
# multi-platform index.
image="docker.io/library/almalinux@sha256:02df05719ef287ebd0df0a1eedb9172a00bd70703aed7b25660e426b8e0d6047"
glibc_floor="2.34"
target_name="linux-x86_64"

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
# minutes, not the job.
pulled=""
for attempt in 1 2 3; do
  if timeout 600 docker pull --platform linux/amd64 "${image}"; then
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

build="$(
  cat <<'CONTAINER'
set -euo pipefail
trap 'chown -R "${HOST_OWNER}" dist 2>/dev/null || true' EXIT

# Runs a download at most three times, ten minutes each.
fetch() {
  local attempt
  for attempt in 1 2 3; do
    if timeout 600 "$@"; then
      return 0
    fi
    echo "attempt ${attempt} failed: $*"
    sleep 5
  done
  echo "giving up: $*" >&2
  return 1
}

fetch dnf --assumeyes --setopt=timeout=30 --setopt=retries=3 install \
  gcc pcre2-devel pkgconf-pkg-config python3 perl-Digest-SHA

export PATH="/opt/rust/bin:${PATH}"
# cargo keeps its downloads and its output inside the container; only dist/
# is written to the checkout. The crates are downloaded first, so the build
# itself needs no network.
export CARGO_HOME=/tmp/cargo-home CARGO_TARGET_DIR=/tmp/cargo-target
fetch cargo fetch --locked
export CARGO_NET_OFFLINE=true
bash scripts/package-release.sh "${TARGET_NAME}"
CONTAINER
)"
docker run --rm --platform linux/amd64 \
  --volume "${PWD}:/work" --workdir /work \
  --volume "${toolchain}:/opt/rust:ro" \
  --env SOURCE_DATE_EPOCH="${source_date_epoch}" \
  --env TARGET_NAME="${target_name}" \
  --env HOST_OWNER="$(id -u):$(id -g)" \
  "${image}" bash -c "${build}"

bash scripts/check-glibc-floor.sh "${glibc_floor}" \
  "dist/${package}/prismtty" "dist/${package}/ptty" "dist/${package}/ct"

# The bare image, without the network: the binaries have to start on a stock
# system of that age with nothing added to it.
smoke="$(
  cat <<'CONTAINER'
set -euo pipefail
for name in prismtty ptty ct; do
  if [[ "$("/package/${name}" --version)" != "prismtty ${VERSION}" ]]; then
    echo "${name} --version did not print prismtty ${VERSION}" >&2
    exit 1
  fi
  colored="$(printf '192.0.2.1 down\n' | "/package/${name}" --profile generic)"
  if [[ "${colored}" != *$'\033['*"192.0.2.1"* ]]; then
    echo "${name} --profile generic did not color its input" >&2
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
)"
docker run --rm --network none --platform linux/amd64 \
  --volume "${PWD}/dist/${package}:/package:ro" \
  --env VERSION="${version}" \
  "${image}" bash -c "${smoke}"
