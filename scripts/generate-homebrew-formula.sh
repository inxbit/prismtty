#!/usr/bin/env bash
set -euo pipefail

output="${1:-packaging/homebrew/prismtty.rb}"
dist_dir="${2:-dist}"
repo_url="${PRISMTTY_REPO_URL:-https://github.com/inxbit/prismtty}"
version="${PRISMTTY_VERSION:-$(awk -F '"' '/^version = / { print $2; exit }' Cargo.toml)}"

if [[ -z "${version}" ]]; then
  printf 'failed to determine package version from Cargo.toml\n' >&2
  exit 1
fi

sha_for() {
  local target_name="$1"
  local checksum_file="${dist_dir}/prismtty-${version}-${target_name}.tar.gz.sha256"

  if [[ ! -f "${checksum_file}" ]]; then
    printf 'missing checksum file: %s\n' "${checksum_file}" >&2
    exit 1
  fi

  local checksum
  read -r checksum _ < "${checksum_file}"
  if [[ ! "${checksum}" =~ ^[[:xdigit:]]{64}$ ]]; then
    printf 'invalid sha256 in %s: %s\n' "${checksum_file}" "${checksum}" >&2
    exit 1
  fi

  printf '%s' "${checksum}"
}

darwin_aarch64_sha="$(sha_for darwin-aarch64)"
darwin_x86_64_sha="$(sha_for darwin-x86_64)"
linux_x86_64_sha="$(sha_for linux-x86_64)"

mkdir -p "$(dirname "${output}")"

# The release version is written literally into each URL so Homebrew derives
# the formula version from it; an explicit `version` line would be flagged as
# redundant by `brew audit --strict`.
cat > "${output}" <<FORMULA
class Prismtty < Formula
  desc "Fast terminal highlighter focused on network devices and Unix administration"
  homepage "${repo_url}"
  license "MIT"

  depends_on "pcre2"

  on_macos do
    if Hardware::CPU.arm?
      url "${repo_url}/releases/download/v${version}/prismtty-${version}-darwin-aarch64.tar.gz"
      sha256 "${darwin_aarch64_sha}"
    else
      url "${repo_url}/releases/download/v${version}/prismtty-${version}-darwin-x86_64.tar.gz"
      sha256 "${darwin_x86_64_sha}"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "${repo_url}/releases/download/v${version}/prismtty-${version}-linux-x86_64.tar.gz"
      sha256 "${linux_x86_64_sha}"
    else
      odie "Linux ARM release artifacts are not published for PrismTTY ${version}"
    end
  end

  def install
    bin.install "prismtty", "ptty", "ct"
    bash_completion.install "completions/prismtty.bash" => "prismtty"
    fish_completion.install "completions/prismtty.fish"
    zsh_completion.install "completions/_prismtty"
    pkgshare.install "profiles"
  end

  test do
    assert_match "prismtty #{version}", shell_output("#{bin}/prismtty --version")
    assert_match "192.0.2.1", pipe_output("#{bin}/prismtty --profile generic", "192.0.2.1 down\n")
  end
end
FORMULA
