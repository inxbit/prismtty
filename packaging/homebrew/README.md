# Homebrew Formula

The release workflow generates `prismtty.rb` from the actual release artifact
checksums by running:

```sh
bash scripts/generate-homebrew-formula.sh artifacts/prismtty.rb artifacts
```

Do not commit a formula with placeholder checksums. Publish the generated formula
only after the matching release tarballs and `.sha256` files exist.
