//! The website shows real PrismTTY output: `scripts/site-output.mjs` pipes
//! each `fixtures/site/<slug>.<profile>.txt` through the binary and keeps the
//! result as `<slug>.<profile>.ansi`, which the page is rendered from. This
//! test re-runs the binary and fails when a snapshot no longer matches what
//! PrismTTY prints, so a profile change cannot leave the site showing stale
//! colors. Fix a failure by re-running `node scripts/site-output.mjs`.

use assert_cmd::Command;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

#[test]
fn site_output_snapshots_match_the_binary() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/site");
    let mut fixtures: Vec<_> = fs::read_dir(&dir)
        .expect("fixtures/site exists")
        .map(|entry| entry.expect("readable entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .collect();
    fixtures.sort();
    assert!(!fixtures.is_empty(), "fixtures/site has no .txt fixtures");

    for txt in fixtures {
        let stem = txt
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("utf-8 name");
        let profile = stem
            .split_once('.')
            .map(|(_, profile)| profile)
            .unwrap_or_else(|| panic!("{stem}: expected <slug>.<profile>.txt"));
        let input = fs::read(&txt).expect("readable fixture");
        let expected = fs::read_to_string(txt.with_extension("ansi"))
            .unwrap_or_else(|err| panic!("{stem}.ansi: {err}; run node scripts/site-output.mjs"));

        // Hermetic like the generator: no user or system config may leak in.
        let home = tempfile::tempdir().expect("temp home");
        let runtime = home.path().join("run");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&runtime)
            .expect("runtime dir");
        let output = Command::cargo_bin("prismtty")
            .expect("prismtty binary")
            .env_clear()
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join("xdg"))
            .env("PRISMTTY_RUNTIME_DIR", &runtime)
            .args(["-R", "-p", profile])
            .write_stdin(input)
            .output()
            .expect("prismtty runs");
        assert!(output.status.success(), "{stem}: prismtty failed");
        assert_eq!(
            String::from_utf8(output.stdout).expect("utf-8 output"),
            expected,
            "{stem}.ansi is stale; run node scripts/site-output.mjs"
        );
    }
}
