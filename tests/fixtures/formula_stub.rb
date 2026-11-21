# A stand-in for Homebrew's Formula class, enough to run the install and
# caveats of a generated formula against a scratch prefix.
#   ruby formula_stub.rb <formula.rb> <prefix> <staged-archive-dir>
require "fileutils"
require "pathname"

class Pathname
  def install(*sources)
    mkpath
    sources.each do |source|
      if source.is_a?(Hash)
        source.each { |from, to| FileUtils.cp_r(from, self/to) }
      else
        FileUtils.cp_r(source, self/File.basename(source))
      end
    end
  end

  def install_symlink(links)
    mkpath
    links.each { |target, link_name| FileUtils.ln_sf(target.to_s, (self/link_name).to_s) }
  end
end

formula_path, prefix_root, staged = ARGV
HOMEBREW_PREFIX = Pathname(prefix_root)
HOMEBREW_CELLAR = HOMEBREW_PREFIX/"Cellar"

class Formula
  class << self
    def desc(*); end
    def homepage(*); end
    def license(*); end
    def depends_on(*); end
    def on_macos; end
    def on_linux; end
    def test; end
  end

  def name; "prismtty"; end
  def full_name; "inxbit/tap/prismtty"; end
  def version; "1.2.3"; end
  def prefix; HOMEBREW_CELLAR/name/version; end
  def bin; prefix/"bin"; end
  def libexec; prefix/"libexec"; end
  def opt_libexec; HOMEBREW_PREFIX/"opt"/name/"libexec"; end
  def bash_completion; prefix/"etc/bash_completion.d"; end
  def fish_completion; prefix/"share/fish/vendor_completions.d"; end
  def zsh_completion; prefix/"share/zsh/site-functions"; end
  def pkgshare; prefix/"share"/name; end

  # Like Homebrew's: an edit that changes nothing is an error.
  def inreplace(path, before, after)
    text = File.read(path)
    changed = text.gsub(before, after)
    raise "inreplace: #{before.inspect} not found in #{path}" if changed == text
    File.write(path, changed)
  end
end

load formula_path
formula = Prismtty.new
formula.prefix.mkpath
Dir.chdir(staged) { formula.install }
caveats = formula.respond_to?(:caveats) ? formula.caveats : nil
puts "caveats: #{caveats.inspect}"
