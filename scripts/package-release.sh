#!/usr/bin/env bash
set -euo pipefail

target_name="${1:-$(uname -s | tr '[:upper:]' '[:lower:]')-$(uname -m)}"
case "${target_name}" in
  darwin-aarch64|darwin-x86_64|linux-x86_64) ;;
  *)
    printf 'unsupported release target: %s\n' "${target_name}" >&2
    exit 1
    ;;
esac
version="$(cargo pkgid | sed 's/.*#//')"
package="prismtty-${version}-${target_name}"
dist_dir="dist"
stage="${dist_dir}/${package}"

cargo build --release --bins

rm -rf "${stage}"
mkdir -p "${stage}/completions" "${stage}/profiles"

cp target/release/prismtty target/release/ptty target/release/ct "${stage}/"
cp LICENSE README.md "${stage}/"
cp completions/prismtty.bash completions/prismtty.fish completions/_prismtty "${stage}/completions/"
cp profiles/custom-router.example.yml "${stage}/profiles/"

tar -C "${dist_dir}" -czf "${dist_dir}/${package}.tar.gz" "${package}"
(cd "${dist_dir}" && shasum -a 256 "${package}.tar.gz") > "${dist_dir}/${package}.tar.gz.sha256"

printf '%s\n' "${dist_dir}/${package}.tar.gz"
