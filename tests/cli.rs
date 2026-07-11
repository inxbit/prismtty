use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::io::Write;
use std::process::{Command as StdCommand, Stdio};
use std::thread;
use std::time::Duration;

#[test]
fn generated_help_lists_core_flags_and_profile_commands() {
    let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
    cmd.arg("--help");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Usage:"))
        .stdout(predicate::str::contains("--profile"))
        .stdout(predicate::str::contains("--no-dynamic-profile"))
        .stdout(predicate::str::contains("profiles show <PROFILE>"));
}

#[test]
fn checked_in_completion_files_cover_command_names_and_core_flags() {
    let bash = include_str!("../completions/prismtty.bash");
    let fish = include_str!("../completions/prismtty.fish");
    let zsh = include_str!("../completions/_prismtty");

    for (name, completion) in [("bash", bash), ("fish", fish), ("zsh", zsh)] {
        for needle in ["prismtty", "ptty", "ct", "profiles"] {
            assert!(
                completion.contains(needle),
                "{name} completion did not include {needle:?}"
            );
        }
    }

    for needle in ["--profile", "--version", "--reload"] {
        assert!(
            bash.contains(needle),
            "bash completion did not include {needle:?}"
        );
        assert!(
            zsh.contains(needle),
            "zsh completion did not include {needle:?}"
        );
    }

    for needle in ["-l profile", "-l version", "-l reload"] {
        assert!(
            fish.contains(needle),
            "fish completion did not include {needle:?}"
        );
    }
}

#[test]
fn cli_usage_errors_include_context_without_pinning_clap_wording() {
    let cases: &[(&[&str], &str)] = &[
        (&["--profile"], "--profile"),
        (&["--config"], "--config"),
        (&["--trace-io"], "--trace-io"),
        (&["--not-a-real-flag"], "--not-a-real-flag"),
        (&["profiles", "show"], "profiles show"),
    ];
    let expected_words = ["value", "required", "expected", "unexpected", "unknown"];

    for (args, context) in cases {
        let output = Command::cargo_bin("prismtty")
            .expect("binary exists")
            .args(*args)
            .output()
            .expect("command runs");
        assert!(
            !output.status.success(),
            "expected {args:?} to fail, stdout: {:?}, stderr: {:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(context),
            "stderr for {args:?} did not include context {context:?}: {stderr:?}"
        );
        assert!(
            expected_words.iter().any(|word| stderr.contains(word)),
            "stderr for {args:?} did not include any expected error word: {stderr:?}"
        );
    }
}

#[test]
fn cli_usage_errors_preserve_lines_and_prefix_untrusted_continuations() {
    let output = Command::cargo_bin("prismtty")
        .expect("binary exists")
        .arg("--not-a-real-flag\nforged diagnostic")
        .output()
        .expect("command runs");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains('\n'),
        "diagnostic was flattened: {stderr:?}"
    );
    assert!(
        stderr.contains("Usage:"),
        "usage line is missing: {stderr:?}"
    );
    assert!(
        !stderr.contains("\\nUsage:"),
        "usage newline was rendered as text: {stderr:?}"
    );
    assert!(
        stderr.lines().all(|line| line.starts_with("prismtty:")),
        "an untrusted continuation escaped the diagnostic prefix: {stderr:?}"
    );
}

#[test]
fn stdin_mode_highlights_with_forced_profile() {
    let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
    cmd.arg("--profile")
        .arg("generic")
        .write_stdin("192.0.2.1 down\n");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("192.0.2.1"))
        .stdout(predicate::str::contains("down"))
        .stdout(predicate::str::contains("\u{1b}["));
}

#[test]
fn wrapped_command_can_trace_pty_output_bytes() {
    let trace = tempfile::NamedTempFile::new().expect("trace file");

    let mut cmd = Command::cargo_bin("ptty").expect("ptty binary exists");
    cmd.arg("--trace-io")
        .arg(trace.path())
        .arg("printf")
        .arg("echo-trace\n");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("echo-trace"));

    let trace_output = fs::read_to_string(trace.path()).expect("trace output");
    assert!(
        trace_output.contains("OUT 65 63 68 6f 2d 74 72 61 63 65"),
        "trace output did not include hex-encoded PTY output: {trace_output:?}"
    );
    assert!(
        trace_output.contains("RENDER 65 63 68 6f 2d 74 72 61 63 65"),
        "trace output did not include hex-encoded rendered output: {trace_output:?}"
    );
}

// AUDIT L15: a wrapped child killed by a signal is reported as 128+signal,
// not portable-pty's generic exit code 1.
#[test]
#[cfg(unix)]
fn wrapped_child_killed_by_signal_reports_128_plus_signal() {
    let mut cmd = Command::cargo_bin("ptty").expect("ptty binary exists");
    cmd.arg("sh").arg("-c").arg("kill -TERM $$");
    cmd.assert().code(143);
}

// AUDIT M6/L15: a normal non-zero exit code survives the reaping path intact.
#[test]
#[cfg(unix)]
fn wrapped_child_nonzero_exit_code_is_preserved() {
    let mut cmd = Command::cargo_bin("ptty").expect("ptty binary exists");
    cmd.arg("sh").arg("-c").arg("exit 7");
    cmd.assert().code(7);
}

#[test]
fn accepts_chromaterm_cli_compatibility_flags() {
    let mut config = tempfile::NamedTempFile::new().expect("temp config");
    writeln!(
        config,
        r##"
rules:
  - description: ip
    regex: \b192\.0\.2\.55\b
    color: f#00ffff
"##
    )
    .expect("write temp config");

    let mut cmd = Command::cargo_bin("ct")
        .unwrap_or_else(|_| Command::cargo_bin("prismtty").expect("binary exists"));
    cmd.arg("-R")
        .arg("--pcre")
        .arg("-c")
        .arg(config.path())
        .write_stdin("192.0.2.55\n");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("192.0.2.55"))
        .stdout(predicate::str::contains("\u{1b}[38;2;0;255;255m"));
}

#[test]
fn sanitize_preserves_trailing_partial_utf8_as_binary_output() {
    let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
    cmd.arg("--sanitize").write_stdin(vec![b'a', 0xe2]);

    let output = cmd.output().expect("command runs");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, vec![b'a', 0xe2]);
}

#[test]
fn filtered_modes_preserve_invalid_binary_and_all_partial_utf8_prefixes_at_eof() {
    let mut empty_config = tempfile::NamedTempFile::new().expect("empty config");
    writeln!(empty_config, "rules: []").expect("write empty config");
    let inputs = [
        vec![b'a', 0xff, 0xc0, 0xc2],
        vec![b'a', 0xff, 0xe2, 0x9d],
        vec![b'a', 0xff, 0xf0, 0x90, 0x80],
    ];

    for mode in ["--sanitize", "--strip-ansi"] {
        for input in &inputs {
            let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
            cmd.arg(mode)
                .arg("--config")
                .arg(empty_config.path())
                .write_stdin(input.clone());

            let output = cmd.output().expect("command runs");
            assert!(output.status.success(), "{mode}: {output:?}");
            assert_eq!(
                output.stdout.as_slice(),
                input.as_slice(),
                "{mode} changed {input:02x?}"
            );
        }
    }
}

#[test]
fn raw_c1_filtering_contract_matches_strip_and_sanitize_modes() {
    let mut empty_config = tempfile::NamedTempFile::new().expect("empty config");
    writeln!(empty_config, "rules: []").expect("write empty config");
    let input = b"safe\x9d0;hidden\x07after\x9b31mred\x9b0m\n";

    for (args, expected) in [
        (vec!["--strip-ansi"], b"safeafterred\n".as_slice()),
        (
            vec!["--sanitize"],
            b"safeafter\x9b31mred\x9b0m\n".as_slice(),
        ),
        (
            vec!["--strip-ansi", "--sanitize"],
            b"safeafterred\n".as_slice(),
        ),
    ] {
        let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
        cmd.args(args)
            .arg("--config")
            .arg(empty_config.path())
            .write_stdin(input.as_slice());
        let output = cmd.output().expect("command runs");

        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, expected);
    }
}

#[test]
fn profiles_list_includes_v1_builtin_profiles() {
    let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
    cmd.arg("profiles").arg("list");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("juniper"))
        .stdout(predicate::str::contains("cisco"))
        .stdout(predicate::str::contains("versa"))
        .stdout(predicate::str::contains("arubacx"))
        .stdout(predicate::str::contains("arista"))
        .stdout(predicate::str::contains("fortinet"))
        .stdout(predicate::str::contains("palo-alto"))
        .stdout(predicate::str::contains("linux-unix"));
}

#[test]
fn profiles_show_describes_a_builtin_profile() {
    let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
    cmd.arg("profiles").arg("show").arg("cisco");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("profile: cisco"))
        .stdout(predicate::str::contains("inherits: generic"))
        .stdout(predicate::str::contains("Cisco interface"));
}

#[test]
fn profiles_d_profiles_are_listable_selectable_and_autodetected() {
    let xdg = tempfile::tempdir().expect("temp xdg config");
    let profiles_dir = xdg.path().join("prismtty").join("profiles.d");
    fs::create_dir_all(&profiles_dir).expect("create profiles.d");
    fs::write(
        profiles_dir.join("custom-router.yml"),
        r##"
profile:
  name: custom-router
  inherits: [generic]
  detection:
    - CustomOS
rules:
  - description: custom-router interface
    regex: \bcust\d+/\d+\b
    color: f#ff00ff bold
"##,
    )
    .expect("write custom profile");
    let mut empty_config = tempfile::NamedTempFile::new().expect("empty config");
    writeln!(empty_config, "rules: []").expect("write empty config");

    let mut list = Command::cargo_bin("prismtty").expect("binary exists");
    list.env("XDG_CONFIG_HOME", xdg.path())
        .arg("profiles")
        .arg("list");
    list.assert()
        .success()
        .stdout(predicate::str::contains("custom-router"));

    let mut show = Command::cargo_bin("prismtty").expect("binary exists");
    show.env("XDG_CONFIG_HOME", xdg.path())
        .arg("profiles")
        .arg("show")
        .arg("custom-router");
    show.assert()
        .success()
        .stdout(predicate::str::contains("profile: custom-router"))
        .stdout(predicate::str::contains("inherits: generic"))
        .stdout(predicate::str::contains("custom-router interface"));

    let mut forced = Command::cargo_bin("prismtty").expect("binary exists");
    forced
        .env("XDG_CONFIG_HOME", xdg.path())
        .arg("-R")
        .arg("--profile")
        .arg("custom-router")
        .arg("--config")
        .arg(empty_config.path())
        .write_stdin("cust1/2 is up\n");
    forced
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{1b}[1;38;2;255;0;255mcust1/2"));

    let mut detected = Command::cargo_bin("prismtty").expect("binary exists");
    detected
        .env("XDG_CONFIG_HOME", xdg.path())
        .arg("-R")
        .arg("--config")
        .arg(empty_config.path())
        .write_stdin("CustomOS cust1/2 is up\n");
    detected
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{1b}[1;38;2;255;0;255mcust1/2"));
}

#[test]
fn profiles_d_profiles_can_inherit_other_profiles_d_profiles() {
    let xdg = tempfile::tempdir().expect("temp xdg config");
    let profiles_dir = xdg.path().join("prismtty").join("profiles.d");
    fs::create_dir_all(&profiles_dir).expect("create profiles.d");
    fs::write(
        profiles_dir.join("parent.yml"),
        r##"
profile:
  name: parent-os
rules:
  - description: parent token
    regex: parent-token
    color: f#00ffff
"##,
    )
    .expect("write parent profile");
    fs::write(
        profiles_dir.join("child.yml"),
        r##"
profile:
  name: child-os
  inherits: [parent-os]
rules:
  - description: child token
    regex: child-token
    color: f#ff00ff
"##,
    )
    .expect("write child profile");
    let mut empty_config = tempfile::NamedTempFile::new().expect("empty config");
    writeln!(empty_config, "rules: []").expect("write empty config");

    let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
    cmd.env("XDG_CONFIG_HOME", xdg.path())
        .arg("-R")
        .arg("--profile")
        .arg("child-os")
        .arg("--config")
        .arg(empty_config.path())
        .write_stdin("parent-token child-token\n");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains(
            "\u{1b}[38;2;0;255;255mparent-token",
        ))
        .stdout(predicate::str::contains(
            "\u{1b}[38;2;255;0;255mchild-token",
        ));
}

#[test]
fn validates_profile_files() {
    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    writeln!(
        file,
        r##"
profile:
  name: custom-router
  inherits: [generic]
  detection:
    - "CustomOS"
rules:
  - description: custom interface
    regex: \bcust\d+/\d+\b
    color: f#00ffff bold
"##
    )
    .expect("write temp profile");

    let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
    cmd.arg("profiles").arg("validate").arg(file.path());

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("custom-router"))
        .stdout(predicate::str::contains("valid"));
}

#[test]
fn profiles_validate_rejects_one_detection_hint_over_the_metadata_limit() {
    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    writeln!(file, "profile:\n  name: bounded-router\n  detection:").expect("write profile header");
    for index in 0..33 {
        writeln!(file, "    - hint-{index}").expect("write detection hint");
    }
    writeln!(file, "rules: []").expect("write profile rules");

    let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
    cmd.arg("profiles").arg("validate").arg(file.path());

    cmd.assert().failure().stderr(predicate::str::contains(
        "profile detection hint count is 33; limit is 32",
    ));
}

#[test]
fn profiles_validate_rejects_self_inheritance_cycle() {
    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    writeln!(
        file,
        r##"
profile:
  name: loop-os
  inherits: [loop-os]
rules:
  - description: loop token
    regex: loop-token
    color: f#00ffff
"##
    )
    .expect("write temp profile");

    let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
    cmd.arg("profiles").arg("validate").arg(file.path());

    cmd.assert().failure().stderr(predicate::str::contains(
        "cyclic profile inheritance: loop-os -> loop-os",
    ));
}

#[test]
fn profiles_validate_rejects_indirect_inheritance_cycle() {
    let xdg = tempfile::tempdir().expect("temp xdg config");
    let profiles_dir = xdg.path().join("prismtty").join("profiles.d");
    fs::create_dir_all(&profiles_dir).expect("create profiles.d");
    fs::write(
        profiles_dir.join("parent.yml"),
        r##"
profile:
  name: parent-os
  inherits: [child-os]
rules:
  - description: parent token
    regex: parent-token
    color: f#00ffff
"##,
    )
    .expect("write parent profile");
    let mut child = tempfile::NamedTempFile::new().expect("temp profile");
    writeln!(
        child,
        r##"
profile:
  name: child-os
  inherits: [parent-os]
rules:
  - description: child token
    regex: child-token
    color: f#ff00ff
"##
    )
    .expect("write child profile");

    let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
    cmd.env("XDG_CONFIG_HOME", xdg.path())
        .arg("profiles")
        .arg("validate")
        .arg(child.path());

    cmd.assert().failure().stderr(predicate::str::contains(
        "cyclic profile inheritance: child-os -> parent-os -> child-os",
    ));
}

#[test]
fn profiles_test_highlights_a_fixture_file() {
    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    writeln!(file, "Gi0/1 is down 192.0.2.1").expect("write fixture");

    let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
    cmd.arg("profiles")
        .arg("test")
        .arg("cisco")
        .arg(file.path());

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Gi0/1"))
        .stdout(predicate::str::contains("192.0.2.1"))
        .stdout(predicate::str::contains("\u{1b}["));
}

#[test]
fn profiles_test_neutralizes_partial_terminal_control_at_eof() {
    let fixture = tempfile::NamedTempFile::new().expect("temp fixture");
    fs::write(fixture.path(), b"safe\x1b[31").expect("fixture writes");

    let output = Command::cargo_bin("prismtty")
        .expect("binary exists")
        .arg("profiles")
        .arg("test")
        .arg("generic")
        .arg(fixture.path())
        .output()
        .expect("profiles test runs");

    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stdout.starts_with(b"safe"));
    assert!(
        !output.stdout.windows(5).any(|window| window == b"\x1b[31"),
        "profiles test emitted an active incomplete CSI: {:?}",
        output.stdout
    );
}

#[test]
fn profiles_test_rejects_oversized_fixture_files() {
    let fixture = tempfile::NamedTempFile::new().expect("temp fixture");
    fixture
        .as_file()
        .set_len(16 * 1024 * 1024 + 1)
        .expect("sparse fixture size sets");

    let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
    cmd.arg("profiles")
        .arg("test")
        .arg("generic")
        .arg(fixture.path());

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("fixture file is too large"));
}

#[test]
fn profiles_test_reports_bounded_regex_match_failures_at_eof() {
    let xdg = tempfile::tempdir().expect("temp xdg config");
    let profiles_dir = xdg.path().join("prismtty").join("profiles.d");
    fs::create_dir_all(&profiles_dir).expect("create profiles.d");
    fs::write(
        profiles_dir.join("pathological.yml"),
        r##"
profile:
  name: pathological
  inherits: []
rules:
  - description: bounded pathological rule
    regex: '^(a+)+$'
    color: f#ff0000
"##,
    )
    .expect("write profile");
    let fixture = tempfile::NamedTempFile::new().expect("fixture");
    let mut input = vec![b'a'; 511];
    input.push(b'b');
    fs::write(fixture.path(), input).expect("write fixture");

    let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
    cmd.env("XDG_CONFIG_HOME", xdg.path())
        .arg("profiles")
        .arg("test")
        .arg("pathological")
        .arg(fixture.path());

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("bounded pathological rule"))
        .stderr(predicate::str::contains("match failed"));
}

#[test]
fn ptty_alias_supports_stdin_mode() {
    let mut cmd = Command::cargo_bin("ptty").expect("ptty binary exists");
    cmd.arg("--profile")
        .arg("generic")
        .write_stdin("198.51.100.7 up\n");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("198.51.100.7"))
        .stdout(predicate::str::contains("\u{1b}["));
}

#[test]
fn no_dynamic_profile_flag_is_accepted() {
    let mut cmd = Command::cargo_bin("ptty").expect("ptty binary exists");
    cmd.arg("--no-dynamic-profile")
        .arg("--profile")
        .arg("generic")
        .write_stdin("198.51.100.8 up\n");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("198.51.100.8"))
        .stdout(predicate::str::contains("\u{1b}["));
}

#[test]
fn benchmark_reports_per_rule_timings() {
    let mut empty_config = tempfile::NamedTempFile::new().expect("empty config");
    writeln!(empty_config, "rules: []").expect("write empty config");

    let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
    cmd.arg("--benchmark")
        .arg("--profile")
        .arg("generic")
        .arg("--config")
        .arg(empty_config.path())
        .write_stdin("192.0.2.1 down\n");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("192.0.2.1"))
        .stderr(predicate::str::contains(
            "Benchmark results (time spent, match count):",
        ))
        .stderr(predicate::str::contains("IPv4 address"))
        .stderr(predicate::str::contains("bad operational state"));
}

#[test]
fn benchmark_reports_raw_bytes_read_before_sanitization() {
    let mut empty_config = tempfile::NamedTempFile::new().expect("empty config");
    writeln!(empty_config, "rules: []").expect("write empty config");
    let input = b"safe\x1b]0;hidden-title\x07after\n";

    let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
    cmd.arg("--benchmark")
        .arg("--sanitize")
        .arg("--profile")
        .arg("generic")
        .arg("--config")
        .arg(empty_config.path())
        .write_stdin(input.as_slice());

    cmd.assert()
        .success()
        .stderr(predicate::str::contains(format!(
            "Processed {} bytes",
            input.len()
        )));
}

#[test]
fn show_profile_reports_selected_profiles_to_stderr() {
    let mut empty_config = tempfile::NamedTempFile::new().expect("empty config");
    writeln!(empty_config, "rules: []").expect("write empty config");

    let mut cmd = Command::cargo_bin("ptty")
        .unwrap_or_else(|_| Command::cargo_bin("prismtty").expect("binary exists"));
    cmd.arg("--show-profile")
        .arg("--config")
        .arg(empty_config.path())
        .write_stdin("admin@mx480> show route\n");

    cmd.assert().success().stderr(predicate::str::contains(
        "prismtty: profiles selected: generic, juniper",
    ));
}

#[test]
fn raw_c1_string_payload_is_not_used_for_profile_detection() {
    let mut empty_config = tempfile::NamedTempFile::new().expect("empty config");
    writeln!(empty_config, "rules: []").expect("write empty config");

    let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
    cmd.arg("--show-profile")
        .arg("--config")
        .arg(empty_config.path())
        .write_stdin(b"\x9d0;JUNOS 23.4\x07plain output\n".as_slice());
    let output = cmd.output().expect("command runs");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");

    assert!(output.status.success());
    assert!(!stderr.contains("juniper"), "{stderr:?}");
}

#[test]
fn show_profile_reports_linux_cisco_and_fortinet_detections() {
    let mut empty_config = tempfile::NamedTempFile::new().expect("empty config");
    writeln!(empty_config, "rules: []").expect("write empty config");

    for (sample, expected) in [
        (
            "OS: Ubuntu 24.04.4 LTS x86_64\nKernel: Linux 6.8.0-110-generic\nTerminal: /dev/pts/0\n",
            "prismtty: profiles selected: generic, linux-unix",
        ),
        (
            "CORE-SW01#show version\nCisco IOS XE Software\n",
            "prismtty: profiles selected: generic, cisco",
        ),
        (
            "FGVM04TM22000000 # get system status\nVersion: FortiGate-VM64\n",
            "prismtty: profiles selected: generic, fortinet",
        ),
        (
            "\x1b[1;32mN9K-CORE-01#\x1b[0m\n",
            "prismtty: profiles selected: generic, cisco",
        ),
        (
            "\x1b[1;32mFW-EDGE (global) #\x1b[0m\n",
            "prismtty: profiles selected: generic, fortinet",
        ),
    ] {
        let mut cmd = Command::cargo_bin("ptty")
            .unwrap_or_else(|_| Command::cargo_bin("prismtty").expect("binary exists"));
        cmd.arg("--show-profile")
            .arg("--config")
            .arg(empty_config.path())
            .write_stdin(sample);

        cmd.assert()
            .success()
            .stderr(predicate::str::contains(expected));
    }
}

#[test]
fn show_profile_promotes_after_delayed_nexus_banner() {
    let mut empty_config = tempfile::NamedTempFile::new().expect("empty config");
    writeln!(empty_config, "rules: []").expect("write empty config");

    let mut child = StdCommand::new(assert_cmd::cargo::cargo_bin("ptty"))
        .arg("--show-profile")
        .arg("--config")
        .arg(empty_config.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ptty");

    let mut stdin = child.stdin.take().expect("stdin pipe");
    stdin.write_all(b"\r\n").expect("write initial chunk");
    stdin.flush().expect("flush initial chunk");
    thread::sleep(Duration::from_millis(50));
    stdin
        .write_all(b"Cisco Nexus Operating System (NX-OS) Software\n")
        .expect("write delayed banner");
    drop(stdin);

    let output = child.wait_with_output().expect("ptty output");
    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(
        stderr.contains("prismtty: profiles selected: generic, cisco"),
        "stderr did not report promoted profile: {stderr:?}"
    );
    assert!(
        !stderr.contains("prismtty: profiles selected: generic\n"),
        "stderr reported generic before promotion: {stderr:?}"
    );
}

#[test]
fn reload_flag_requests_reload_without_reading_stdin() {
    let runtime = tempfile::tempdir().expect("temp runtime");

    let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
    cmd.env("PRISMTTY_RUNTIME_DIR", runtime.path())
        .arg("--reload");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Processes reloaded: 0"));
}

#[test]
fn wrapped_command_output_is_highlighted() {
    let mut cmd = Command::cargo_bin("ptty").expect("ptty binary exists");
    cmd.arg("--profile")
        .arg("generic")
        .arg("printf")
        .arg("192.0.2.44 down\n");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("192.0.2.44"))
        .stdout(predicate::str::contains("down"))
        .stdout(predicate::str::contains("\u{1b}["));
}

#[test]
fn wrapped_command_preserves_utf8_terminal_glyphs() {
    let mut cmd = Command::cargo_bin("ptty").expect("binary exists");
    cmd.arg("printf").arg("CPU: ━━━━━━━ ã é 🚀 \n");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("CPU: ━━━━━━━ ã é 🚀 "))
        .stdout(predicate::str::contains("^D").not());
}

#[test]
fn profiles_d_override_of_a_builtin_warns_on_stderr() {
    let xdg = tempfile::tempdir().expect("temp xdg config");
    let profiles_dir = xdg.path().join("prismtty").join("profiles.d");
    fs::create_dir_all(&profiles_dir).expect("create profiles.d");
    fs::write(
        profiles_dir.join("cisco.yml"),
        "profile:\n  name: cisco\n  inherits: [generic]\nrules: []\n",
    )
    .expect("write shadowing profile");

    let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
    cmd.env("XDG_CONFIG_HOME", xdg.path())
        .write_stdin("router up\n");

    cmd.assert().success().stderr(predicate::str::contains(
        "user profile 'cisco' from profiles.d overrides the built-in",
    ));
}
