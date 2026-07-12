## Summary

- 

## Validation

- [ ] `cargo fmt --check`
- [ ] `cargo test --locked`
- [ ] `cargo clippy --locked --all-targets --all-features -- -D warnings`
- [ ] `bash scripts/privacy-scan.sh`
- [ ] `node --test tests/bash_completion.test.mjs tests/privacy_scan.test.mjs tests/release_security.test.mjs`
- [ ] `npm test`
- [ ] Regenerated completions if CLI flags or command help changed
- [ ] Updated `README.md` or `CHANGELOG.md` for user-facing behavior changes

## Data Handling

- [ ] I used synthetic fixtures only
- [ ] I did not include real terminal captures, credentials, customer names, personal filesystem paths, or trace files from sensitive sessions

## Release/Packaging

- [ ] Release, Homebrew, or workflow changes remain reproducible from checked-in scripts
