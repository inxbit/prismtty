# Homebrew Formula

The release workflow generates `prismtty.rb` from the actual release artifact
checksums by running:

```sh
bash scripts/generate-homebrew-formula.sh artifacts/prismtty.rb artifacts
```

Do not commit a formula with placeholder checksums. Publish the generated formula
only after the matching release tarballs and `.sha256` files exist.

The public Homebrew install path is:

```sh
brew install inxbit/tap/prismtty
```

The tap repository is `https://github.com/inxbit/homebrew-tap`, and its formula
lives at `Formula/prismtty.rb`. After each PrismTTY release, update the tap
formula from the generated `prismtty.rb` release artifact or regenerate it from
the same release artifact directory.
