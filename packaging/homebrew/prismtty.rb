class Prismtty < Formula
  desc "Fast terminal highlighter focused on network devices and Unix administration"
  homepage "https://github.com/cdassy/prismtty"
  version "0.1.0"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/cdassy/prismtty/releases/download/v#{version}/prismtty-#{version}-darwin-aarch64.tar.gz"
      sha256 "REPLACE_WITH_DARWIN_AARCH64_SHA256"
    else
      url "https://github.com/cdassy/prismtty/releases/download/v#{version}/prismtty-#{version}-darwin-x86_64.tar.gz"
      sha256 "REPLACE_WITH_DARWIN_X86_64_SHA256"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/cdassy/prismtty/releases/download/v#{version}/prismtty-#{version}-linux-aarch64.tar.gz"
      sha256 "REPLACE_WITH_LINUX_AARCH64_SHA256"
    else
      url "https://github.com/cdassy/prismtty/releases/download/v#{version}/prismtty-#{version}-linux-x86_64.tar.gz"
      sha256 "REPLACE_WITH_LINUX_X86_64_SHA256"
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
