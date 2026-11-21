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

# The formula carries no explicit `version`, so Homebrew derives it from the
# download URLs. That only works for a plain MAJOR.MINOR.PATCH version: a
# hyphenated pre-release such as 2.0.0-rc1 makes Homebrew read the `x86_64` in
# the file name instead and report version 86.64, which would outrank every
# later release. Refuse to generate such a formula.
if [[ ! "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  printf 'refusing to generate a formula for non MAJOR.MINOR.PATCH version: %s\n' "${version}" >&2
  printf 'Homebrew derives the version from the download URL; add an explicit version line first.\n' >&2
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
    # Another package may already own ct in the prefix (chart-testing and
    # coreos-ct install one). Homebrew's link step fails on such a file and
    # then unlinks this whole formula, prismtty and ptty included, so ct and
    # everything that claims its name stay out of the linked directories.
    ct_taken = ct_owned_elsewhere?
    if ct_taken
      inreplace "completions/_prismtty", "#compdef prismtty ptty ct", "#compdef prismtty ptty"
      inreplace "completions/prismtty.bash", /^ +complete -F _ct .* ct\n/, "    :\n"
      inreplace "completions/prismtty.fish", /^complete -c ct .*\n/, ""
    end

    bin.install "prismtty", "ptty"
    bash_completion.install "completions/prismtty.bash" => "prismtty"
    # bash-completion loads a file on demand by command name; ptty and ct
    # share the prismtty one.
    bash_completion.install_symlink bash_completion/"prismtty" => "ptty"
    fish_completion.install "completions/prismtty.fish"
    # fish loads completions by command name; ptty and ct share prismtty.fish.
    fish_completion.install_symlink fish_completion/"prismtty.fish" => "ptty.fish"
    if ct_taken
      (libexec/"bin").install "ct"
    else
      bin.install "ct"
      bash_completion.install_symlink bash_completion/"prismtty" => "ct"
      fish_completion.install_symlink fish_completion/"prismtty.fish" => "ct.fish"
    end
    zsh_completion.install "completions/_prismtty"
    pkgshare.install "profiles"
  end

  # Whether bin/ct in the Homebrew prefix belongs to something other than this
  # formula. Homebrew unlinks the old keg before an upgrade or a reinstall, so
  # its own link is normally gone by now; the test for it covers the cases
  # where it is not.
  def ct_owned_elsewhere?
    target = HOMEBREW_PREFIX/"bin/ct"
    if target.symlink?
      destination = (target.dirname/target.readlink).cleanpath.to_s
      return false if destination.start_with?("#{HOMEBREW_CELLAR}/#{name}/")
    end
    target.stat
    true
  rescue Errno::ENOENT
    # Nothing there, or a dangling link, which the link step replaces.
    false
  rescue SystemCallError
    # Something is there that the install cannot inspect.
    true
  end

  def caveats
    return unless (libexec/"bin/ct").exist?

    <<~EOS
      Another package already provides ct in #{HOMEBREW_PREFIX}/bin, so
      PrismTTY's ct was not linked. prismtty and ptty work as usual.
      To run PrismTTY as ct, put this directory first in your PATH:
        #{opt_libexec}/bin
      or remove the other ct and run:
        brew reinstall #{full_name}
    EOS
  end

  test do
    assert_match "prismtty #{version}", shell_output("#{bin}/prismtty --version")
    assert_match "192.0.2.1", pipe_output("#{bin}/prismtty --profile generic", "192.0.2.1 down\n")
  end
end
FORMULA
