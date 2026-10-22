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

// An inherited SIGCHLD=SIG_IGN makes the kernel reap the wrapped child on its
// own, which must not cost the wrapper the child's exit status.
#[test]
#[cfg(unix)]
fn wrapped_exit_code_survives_an_inherited_ignored_sigchld() {
    use std::os::unix::process::CommandExt;

    let mut command = StdCommand::new(assert_cmd::cargo::cargo_bin("ptty"));
    command.args(["sh", "-c", "exit 3"]).stdin(Stdio::null());
    // SAFETY: signal is async-signal-safe and runs in the forked child before exec.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGCHLD, libc::SIG_IGN);
            Ok(())
        });
    }
    let output = command.output().expect("ptty runs");
    assert_eq!(
        output.status.code(),
        Some(3),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// A command that cannot be started exits as a shell would, 127 when nothing by
// that name exists and 126 when it exists but cannot be executed, so callers
// can tell a missing tool from a tool that ran and failed.
#[test]
#[cfg(unix)]
fn wrapped_command_spawn_failures_use_shell_exit_codes() {
    use std::os::unix::fs::PermissionsExt;

    let work = tempfile::tempdir().expect("work directory");
    let script = work.path().join("script.sh");
    fs::write(&script, "#!/bin/sh\necho ran\n").expect("write script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o644))
        .expect("script is not executable");
    fs::create_dir(work.path().join("subdir")).expect("directory");

    let cases = [
        ("prismtty-no-such-command", 127, "No viable candidates"),
        ("", 127, "Unable to spawn"),
        ("./missing.sh", 127, "does not exist"),
        ("script.sh", 126, "not executable"),
        ("./script.sh", 126, "not executable"),
        ("./subdir", 126, "is a directory"),
    ];
    for (program, code, message) in cases {
        let output = Command::cargo_bin("ptty")
            .expect("ptty binary exists")
            .current_dir(work.path())
            .env("PATH", work.path())
            .arg(program)
            .output()
            .expect("ptty runs");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(code),
            "{program:?} exit code, stderr: {stderr}"
        );
        assert!(
            stderr.contains("prismtty: PTY error: Unable to spawn") && stderr.contains(message),
            "{program:?} stderr: {stderr}"
        );
    }
}

// A script whose `#!` interpreter cannot be run only fails in execve, after
// the fork, where portable-pty has closed std's exec-error pipe: the child
// aborted with "fatal runtime error" (exit 134) drawn into the output. It must
// get the shell's 127 or 126 and a message on stderr instead. A CRLF line end
// makes the interpreter `/bin/sh\r`, the common case.
#[test]
#[cfg(unix)]
fn wrapped_script_with_unusable_interpreter_uses_shell_exit_codes() {
    use std::os::unix::fs::PermissionsExt;

    let work = tempfile::tempdir().expect("work directory");
    let write = |name: &str, body: &[u8], mode: u32| {
        let path = work.path().join(name);
        fs::write(&path, body).expect("write file");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("set mode");
        path
    };
    let crlf = write("crlf.sh", b"#!/bin/sh\r\necho ran\r\n", 0o755);
    write(
        "missing.sh",
        b"#!/nonexistent/prismtty-interp\necho ran\n",
        0o755,
    );
    let interp = write("interp", b"#!/bin/sh\n", 0o644);
    let mut noexec = b"#!".to_vec();
    noexec.extend_from_slice(interp.to_str().expect("utf-8 path").as_bytes());
    noexec.extend_from_slice(b" -x\necho ran\n");
    write("noexec.sh", &noexec, 0o755);
    write("ok.sh", b"#! /bin/sh -e\necho scriptran\n", 0o755);

    let crlf = crlf.to_str().expect("utf-8 path").to_owned();
    let cases: [(&str, i32); 6] = [
        ("./crlf.sh", 127),
        ("crlf.sh", 127),
        (&crlf, 127),
        ("./missing.sh", 127),
        ("./noexec.sh", 126),
        ("./ok.sh", 0),
    ];
    for (program, code) in cases {
        let output = Command::cargo_bin("ptty")
            .expect("ptty binary exists")
            .current_dir(work.path())
            .env("PATH", work.path())
            .arg(program)
            .timeout(Duration::from_secs(10))
            .output()
            .expect("ptty runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(code),
            "{program:?} exit code, stdout: {stdout:?} stderr: {stderr:?}"
        );
        // Not "fatal runtime error": the highlighter colors the word "error".
        assert!(
            !stdout.contains("output.write(&bytes)") && !stderr.contains("output.write(&bytes)"),
            "{program:?} stdout: {stdout:?} stderr: {stderr:?}"
        );
        if code == 0 {
            assert!(
                stdout.contains("scriptran"),
                "{program:?} stdout: {stdout:?}"
            );
        } else {
            assert!(
                stderr.contains("prismtty: PTY error: Unable to spawn")
                    && stderr.contains("bad interpreter"),
                "{program:?} stderr: {stderr:?}"
            );
        }
    }
}

// With no terminal on stdin the wrapped command reads the caller's stdin
// itself, so it gets end of input as with `cmd < /dev/null` instead of
// blocking forever on its PTY, and nothing shows up in the output: no stray
// newline or `^D`.
#[test]
#[cfg(unix)]
fn wrapped_command_gets_end_of_input_when_stdin_is_not_a_terminal() {
    Command::cargo_bin("ptty")
        .expect("ptty binary exists")
        .args([
            "sh",
            "-c",
            "if read line; then echo \"got:$line\"; else echo stdin-eof; fi",
        ])
        .write_stdin("")
        .timeout(Duration::from_secs(10))
        .assert()
        .success()
        .stdout(predicate::str::contains("stdin-eof"));

    let output = Command::cargo_bin("ptty")
        .expect("ptty binary exists")
        .arg("cat")
        .write_stdin("")
        .timeout(Duration::from_secs(10))
        .output()
        .expect("ptty runs");
    assert!(
        output.status.success(),
        "cat never saw end of input: {output:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "",
        "the end of input showed up in the output"
    );
}

// Every read of a non-terminal stdin sees end of input, as with
// `cmd < /dev/null`: a second `read` or a second reader must not block
// forever on the child's PTY.
#[test]
#[cfg(unix)]
fn wrapped_command_sees_end_of_input_on_every_read_when_stdin_is_not_a_terminal() {
    let output = Command::cargo_bin("ptty")
        .expect("ptty binary exists")
        .args([
            "sh",
            "-c",
            "read a; echo first=$?; read b; echo second=$?; cat; echo cat=$?",
        ])
        .write_stdin("")
        .timeout(Duration::from_secs(10))
        .output()
        .expect("ptty runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success()
            && stdout.contains("first=1")
            && stdout.contains("second=1")
            && stdout.contains("cat=0"),
        "a later read of stdin did not see end of input: {output:?}"
    );
}

// A child that puts its stdin in raw mode must not read a keystroke nobody
// typed: an end of input queued on the PTY reads as NUL (Linux) or ^D
// (macOS) once the line discipline is not canonical.
#[test]
#[cfg(unix)]
fn wrapped_command_in_raw_mode_reads_no_fabricated_input_when_stdin_is_not_a_terminal() {
    let output = Command::cargo_bin("ptty")
        .expect("ptty binary exists")
        .args([
            "sh",
            "-c",
            "stty raw 2>/dev/null; head -c1 | od -An -tx1; echo end",
        ])
        .write_stdin("")
        .timeout(Duration::from_secs(10))
        .output()
        .expect("ptty runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("end"),
        "a raw-mode reader blocked: {output:?}"
    );
    assert!(
        !stdout.contains(" 00") && !stdout.contains(" 04"),
        "a raw-mode reader got a fabricated keystroke: {stdout:?}"
    );
}

// Piped input reaches the wrapped command, as it does without the wrapper,
// followed by end of input.
#[test]
#[cfg(unix)]
fn wrapped_command_reads_piped_stdin() {
    let output = Command::cargo_bin("ptty")
        .expect("ptty binary exists")
        .args([
            "sh",
            "-c",
            "read a; read b; echo \"got:$a,$b\"; read c; echo third=$?",
        ])
        .write_stdin("first-line\nsecond-line\n")
        .timeout(Duration::from_secs(10))
        .output()
        .expect("ptty runs");
    let stdout = strip_csi(&output.stdout);
    assert!(
        output.status.success()
            && stdout.contains("got:first-line,second-line")
            && stdout.contains("third=1"),
        "piped input did not reach the wrapped command: {output:?}"
    );
    assert!(
        !stdout.contains("first-line\r\n"),
        "piped input was echoed as if typed: {stdout:?}"
    );
}

// A script whose interpreter is itself a script with a missing interpreter
// passes the `#!` check and only fails in execve. The spawn must still report
// it, with the shell's 127, instead of the child aborting.
#[test]
#[cfg(target_os = "linux")]
fn wrapped_command_exec_failure_after_the_fork_uses_shell_exit_codes() {
    use std::os::unix::fs::PermissionsExt;

    let work = tempfile::tempdir().expect("work directory");
    let inner = work.path().join("inner");
    fs::write(&inner, "#!/nonexistent/prismtty-interp\n").expect("write inner");
    fs::set_permissions(&inner, fs::Permissions::from_mode(0o755)).expect("inner is executable");
    let outer = work.path().join("outer.sh");
    fs::write(&outer, format!("#!{}\necho ran\n", inner.display())).expect("write outer");
    fs::set_permissions(&outer, fs::Permissions::from_mode(0o755)).expect("outer is executable");

    let output = Command::cargo_bin("ptty")
        .expect("ptty binary exists")
        .current_dir(work.path())
        .arg("./outer.sh")
        .write_stdin("")
        .timeout(Duration::from_secs(10))
        .output()
        .expect("ptty runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(127),
        "stdout: {stdout:?} stderr: {stderr:?}"
    );
    assert!(
        stderr.contains("prismtty: PTY error: Unable to spawn"),
        "stderr: {stderr:?}"
    );
}

// Like portable-pty's spawn, the one for a non-terminal stdin keeps
// descriptors the caller leaked to prismtty out of the wrapped command.
#[test]
#[cfg(unix)]
fn wrapped_command_does_not_inherit_the_callers_extra_descriptors() {
    use std::os::unix::process::CommandExt;

    let mut command = StdCommand::new(assert_cmd::cargo::cargo_bin("ptty"));
    command
        .args([
            "sh",
            "-c",
            "if true 2>/dev/null >&7; then echo fd7-open; else echo fd7-closed; fi",
        ])
        .stdin(Stdio::null());
    // SAFETY: dup2 is async-signal-safe and runs in the forked child before exec;
    // dup2 clears close-on-exec on the copy, so prismtty inherits fd 7.
    unsafe {
        command.pre_exec(|| {
            if libc::dup2(2, 7) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output().expect("ptty runs");
    let stdout = strip_csi(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("fd7-closed"),
        "the wrapped command inherited fd 7: {output:?}"
    );
}

/// Drops CSI sequences so assertions see the text a terminal would show.
fn strip_csi(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let mut plain = String::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.next() == Some('[') {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        plain.push(ch);
    }
    plain
}

// The wrapped command starts in the caller's directory, not $HOME, and a
// relative program path resolves against it the way execvp does.
#[test]
#[cfg(unix)]
fn wrapped_command_runs_in_callers_working_directory() {
    use std::os::unix::fs::PermissionsExt;

    let work = tempfile::tempdir().expect("work directory");
    let home = tempfile::tempdir().expect("home directory");
    let work_path = work
        .path()
        .canonicalize()
        .expect("canonical work directory");
    fs::create_dir(work_path.join("bin")).expect("bin directory");
    for script in ["probe.sh", "bin/probe.sh"] {
        let path = work_path.join(script);
        fs::write(&path, "#!/bin/sh\necho \"marker-$(pwd -P)\"\n").expect("write probe");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("probe is executable");
    }
    let expected = format!("marker-{}\r\n", work_path.display());

    let commands: [&[&str]; 3] = [
        &["sh", "-c", "echo \"marker-$(pwd -P)\""],
        &["./probe.sh"],
        &["bin/probe.sh"],
    ];
    for command in commands {
        let output = Command::cargo_bin("ptty")
            .expect("ptty binary exists")
            .current_dir(&work_path)
            .env("HOME", home.path())
            .args(command)
            .output()
            .expect("ptty runs");
        assert!(
            output.status.success(),
            "{command:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            strip_csi(&output.stdout),
            expected,
            "{command:?} did not run in the caller's directory"
        );
    }
}

// Started from a deleted working directory, the wrapped command stays there,
// as exec would leave it, instead of falling back to $HOME, where a relative
// path (`rm -rf build`, `./tool`) would act on the wrong files.
#[test]
#[cfg(unix)]
fn wrapped_command_in_deleted_directory_does_not_run_in_home() {
    use std::os::unix::fs::PermissionsExt;

    let base = tempfile::tempdir().expect("base directory");
    let home = base.path().join("home");
    fs::create_dir_all(home.join("build")).expect("home build directory");
    let tool = home.join("tool");
    fs::write(&tool, "#!/bin/sh\necho ran-home-tool\n").expect("write tool");
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).expect("tool is executable");
    let ptty = assert_cmd::cargo::cargo_bin("ptty");

    let run = |command: &[&str]| {
        let work = base.path().join("work");
        fs::create_dir(&work).expect("work directory");
        StdCommand::new("sh")
            .arg("-c")
            .arg(r#"cd "$1" && rmdir "$1" && shift && exec "$@""#)
            .arg("sh")
            .arg(&work)
            .arg(&ptty)
            .args(command)
            .env("HOME", &home)
            .stdin(Stdio::null())
            .output()
            .expect("ptty runs")
    };

    let output = run(&["sh", "-c", "rm -rf build"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        home.join("build").is_dir(),
        "`rm -rf build` ran in $HOME: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !stderr.contains("cannot enter"),
        "the deleted directory was not entered: {stderr}"
    );

    let output = run(&["./tool"]);
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("ran-home-tool"),
        "`./tool` resolved against $HOME"
    );
    assert_eq!(
        output.status.code(),
        Some(127),
        "a missing ./tool reports 127: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// On Linux a process can sit in a directory it cannot search, e.g. after
// `sudo -u postgres prismtty psql` from a private home: getcwd() still works
// and a shell still runs commands there. The wrapped command must start (in
// $HOME, as before, and saying so) instead of failing on chdir with
// "Permission denied".
#[test]
#[cfg(unix)]
fn wrapped_command_starts_when_the_working_directory_cannot_be_searched() {
    use std::os::unix::fs::PermissionsExt;

    let base = tempfile::tempdir().expect("base directory");
    let home = tempfile::tempdir().expect("home directory");
    let locked = base
        .path()
        .canonicalize()
        .expect("canonical base directory")
        .join("locked");
    fs::create_dir(&locked).expect("locked directory");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o600)).expect("lock");
    let can_search_anyway = fs::metadata(locked.join(".")).is_ok();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("unlock");
    if can_search_anyway {
        eprintln!("skipped: this user (root?) can search a mode 0600 directory");
        return;
    }

    // Enter the directory first, then take away search permission.
    let output = StdCommand::new("sh")
        .arg("-c")
        .arg(r#"cd "$1" && chmod 0600 . && exec "$2" sh -c 'echo "marker-$(pwd -P)"'"#)
        .arg("sh")
        .arg(&locked)
        .arg(assert_cmd::cargo::cargo_bin("ptty"))
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .output()
        .expect("ptty runs");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("unlock");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "exit {:?}, stderr: {stderr}",
        output.status
    );
    let home_path = home.path().canonicalize().expect("canonical home");
    assert!(
        strip_csi(&output.stdout).contains(&format!("marker-{}", home_path.display())),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    // Elsewhere getcwd() itself can fail there, and the fallback is the same.
    if cfg!(target_os = "linux") {
        assert!(
            stderr.contains("cannot enter the current directory"),
            "the fallback is not reported: {stderr}"
        );
    }
}

// When the $HOME fallback above cannot be entered either, the spawn fails on
// chdir, not on the program. That must name the directory, not report the
// program as missing (127) or not executable (126).
#[test]
#[cfg(unix)]
fn wrapped_command_reports_a_home_fallback_it_cannot_enter() {
    use std::os::unix::fs::PermissionsExt;

    let base = tempfile::tempdir().expect("base directory");
    let locked = base
        .path()
        .canonicalize()
        .expect("canonical base directory")
        .join("locked");
    fs::create_dir(&locked).expect("locked directory");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o600)).expect("lock");
    let can_search_anyway = fs::metadata(locked.join(".")).is_ok();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("unlock");
    if can_search_anyway {
        eprintln!("skipped: this user (root?) can search a mode 0600 directory");
        return;
    }
    let missing_home = base.path().join("missing-home");

    let output = StdCommand::new("sh")
        .arg("-c")
        .arg(r#"cd "$1" && chmod 0600 . && exec "$2" sh -c 'echo ran'"#)
        .arg("sh")
        .arg(&locked)
        .arg(assert_cmd::cargo::cargo_bin("ptty"))
        .env("HOME", &missing_home)
        .stdin(Stdio::null())
        .output()
        .expect("ptty runs");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("unlock");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains(&format!("Unable to enter {}", missing_home.display())),
        "stderr: {stderr}"
    );
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

// A terminal ends a string control at an ESC that does not start ST, so a
// stray OSC introducer must not hold back or discard the output after it.
#[test]
fn escape_ended_string_controls_do_not_hide_later_output() {
    let mut empty_config = tempfile::NamedTempFile::new().expect("empty config");
    writeln!(empty_config, "rules: []").expect("write empty config");
    let mut input = b"x\x1b]0;t\x1b[0m hello world\nstart\x1b]".to_vec();
    let mut sanitized = b"x\x1b[0m hello world\nstart".to_vec();
    for line in 0..2000 {
        let colored = format!("\x1b[32mline {line}\x1b[0m\n");
        input.extend_from_slice(colored.as_bytes());
        sanitized.extend_from_slice(colored.as_bytes());
    }

    for (args, expected) in [
        (vec![], input.as_slice()),
        (vec!["--sanitize"], sanitized.as_slice()),
    ] {
        let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
        cmd.args(&args)
            .arg("--config")
            .arg(empty_config.path())
            .write_stdin(input.clone());
        let output = cmd.output().expect("command runs");

        assert!(output.status.success(), "{output:?}");
        assert!(
            output.stdout == expected,
            "{args:?} changed the output: {} of {} bytes",
            output.stdout.len(),
            expected.len()
        );
    }
}

// xterm reads a character of U+00FF or below after ESC from the Latin-1 half
// of its escape table, which mirrors ASCII, so ESC U+00DD opens an OSC and ESC
// U+00D0 a DCS; a Latin-1 xterm reads the raw bytes D0-DF the same way.
// Escape-state controls and a second ESC leave it in the escape state.
// --sanitize drops such ESCs and keeps the rest visible.
#[test]
fn sanitize_drops_escapes_that_latin1_characters_would_complete() {
    let mut empty_config = tempfile::NamedTempFile::new().expect("empty config");
    writeln!(empty_config, "rules: []").expect("write empty config");
    let input = b"a\x1b\xc3\x9d0;title\x07b\x1b\x00\xc3\x9d0;t\x07c\x1b\x1b\xc3\x9d2;t\x07\
        d\x1b\xdd52;c;U0VDUkVU\x07e\x1b\xc3\x90q\x1b\\f\x1b[1\x1b\xc3\x9e0;t\x07g\n";
    let expected = b"a\xc3\x9d0;title\x07b\x00\xc3\x9d0;t\x07c\xc3\x9d2;t\x07\
        d\xdd52;c;U0VDUkVU\x07e\xc3\x90q\x1b\\f\x1b[1\xc3\x9e0;t\x07g\n";

    let mut cmd = Command::cargo_bin("ct")
        .unwrap_or_else(|_| Command::cargo_bin("prismtty").expect("binary exists"));
    cmd.arg("--sanitize")
        .arg("--config")
        .arg(empty_config.path())
        .write_stdin(input.as_slice());
    let output = cmd.output().expect("command runs");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, expected, "{:02x?}", output.stdout);

    // The same holds for a command's output read from its PTY.
    let output = Command::cargo_bin("ptty")
        .expect("ptty binary exists")
        .arg("--sanitize")
        .arg("--config")
        .arg(empty_config.path())
        .arg("printf")
        .arg("a\\033\\303\\2350;title\\007b\\033\\000\\335c")
        .timeout(Duration::from_secs(10))
        .output()
        .expect("ptty runs");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        output.stdout, b"a\xc3\x9d0;title\x07b\x00\xddc",
        "{:02x?}",
        output.stdout
    );
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

/// Writes `chunks` to `prismtty --show-profile` one pipe write at a time, with
/// a pause between them so each tends to arrive as its own read, and returns
/// what it reported on stderr.
fn show_profile_for_paced_input(chunks: &[&[u8]]) -> String {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut child = StdCommand::new(assert_cmd::cargo::cargo_bin("prismtty"))
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path().join("xdg"))
        .env("PRISMTTY_RUNTIME_DIR", temp.path().join("runtime"))
        .arg("--show-profile")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn prismtty");

    let mut stdin = child.stdin.take().expect("stdin pipe");
    for chunk in chunks {
        stdin.write_all(chunk).expect("write chunk");
        stdin.flush().expect("flush chunk");
        thread::sleep(Duration::from_millis(10));
    }
    drop(stdin);

    let output = child.wait_with_output().expect("prismtty output");
    assert!(output.status.success());
    String::from_utf8(output.stderr).expect("stderr is UTF-8")
}

// Detection waits for a line end, a prompt-like ending or 4 KiB of output
// rather than rescanning on every read. A prompt has no line end, so its last
// byte must still trigger the pass that selects the profile.
#[test]
fn byte_paced_prompt_selects_its_profile() {
    let prompt = b"Router1#";
    let stderr = show_profile_for_paced_input(&prompt.chunks(1).collect::<Vec<_>>());

    assert!(
        stderr.contains("prismtty: profiles selected: generic, cisco"),
        "{stderr:?}"
    );
}

#[test]
fn byte_paced_banner_line_selects_its_profile() {
    let banner = b"Cisco IOS XE Software\n";
    let stderr = show_profile_for_paced_input(&banner.chunks(1).collect::<Vec<_>>());

    assert!(
        stderr.contains("prismtty: profiles selected: generic, cisco"),
        "{stderr:?}"
    );
}

// The read that brings the signal can be the last one. No later read brings
// the line end that would trigger a pass, so the end of input must.
#[test]
fn unterminated_last_line_selects_its_profile() {
    let stderr = show_profile_for_paced_input(&[b"hello\n", b"Cisco IOS XE Software"]);

    assert!(
        stderr.contains("prismtty: profiles selected: generic, cisco"),
        "{stderr:?}"
    );
}

#[test]
fn long_unterminated_line_still_selects_its_profile() {
    let padding = b"x ".repeat(2500);
    let mut chunks: Vec<&[u8]> = vec![b"\r\n", b"Cisco IOS XE Software "];
    chunks.extend(padding.chunks(500));
    let stderr = show_profile_for_paced_input(&chunks);

    assert!(
        stderr.contains("prismtty: profiles selected: generic, cisco"),
        "{stderr:?}"
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

/// A command whose config and runtime lookups stay inside `dir`, so neither
/// the user's nor the machine's PrismTTY state can leak into the test.
fn isolated(bin: &str, dir: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin(bin).expect("binary exists");
    cmd.env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("xdg"))
        .env("PRISMTTY_RUNTIME_DIR", dir.join("runtime"));
    cmd
}

const WRITE_MARKER: &str = "echo ran > \"$PRISMTTY_TEST_MARKER\"";

#[test]
fn unknown_forced_profile_is_rejected_before_wrapped_command_runs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let marker = temp.path().join("marker");

    isolated("ptty", temp.path())
        .env("PRISMTTY_TEST_MARKER", &marker)
        .args(["-p", "nosuch", "sh", "-c", WRITE_MARKER])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown profile 'nosuch'"));
    assert!(!marker.exists(), "the wrapped command ran before the check");
}

#[test]
fn broken_config_is_rejected_before_wrapped_command_runs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let marker = temp.path().join("marker");
    let config = temp.path().join("bad.yml");
    fs::write(
        &config,
        "rules:\n  - description: bad\n    regex: \"(\"\n    color: f#ff0000\n",
    )
    .expect("config writes");

    isolated("ptty", temp.path())
        .env("PRISMTTY_TEST_MARKER", &marker)
        .arg("--config")
        .arg(&config)
        .args(["sh", "-c", WRITE_MARKER])
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to compile"));
    assert!(!marker.exists(), "the wrapped command ran before the check");
}

#[test]
fn malformed_profiles_d_is_rejected_before_wrapped_command_runs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let marker = temp.path().join("marker");
    let profiles_dir = temp.path().join("xdg").join("prismtty").join("profiles.d");
    fs::create_dir_all(&profiles_dir).expect("profiles.d creates");
    fs::write(profiles_dir.join("bad.yml"), "profile: [unterminated\n").expect("profile writes");

    isolated("ptty", temp.path())
        .env("PRISMTTY_TEST_MARKER", &marker)
        .args(["sh", "-c", WRITE_MARKER])
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to parse YAML"));
    assert!(!marker.exists(), "the wrapped command ran before the check");
}

#[test]
fn trace_path_is_rejected_before_wrapped_command_runs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let marker = temp.path().join("marker");

    isolated("ptty", temp.path())
        .env("PRISMTTY_TEST_MARKER", &marker)
        .arg("--trace-io")
        .arg(temp.path().join("missing-dir").join("trace"))
        .args(["sh", "-c", WRITE_MARKER])
        .assert()
        .failure();
    assert!(!marker.exists(), "the wrapped command ran before the check");
}

#[test]
fn bad_config_is_reported_in_stdin_mode_without_input() {
    let temp = tempfile::tempdir().expect("tempdir");

    isolated("prismtty", temp.path())
        .arg("--config")
        .arg(temp.path().join("missing.yml"))
        .write_stdin("")
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to read"));
    isolated("prismtty", temp.path())
        .args(["-p", "nosuch"])
        .write_stdin("")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown profile 'nosuch'"));
}

#[test]
fn broken_profile_detected_in_first_chunk_keeps_last_good_rules() {
    let temp = tempfile::tempdir().expect("tempdir");
    let profiles_dir = temp.path().join("xdg").join("prismtty").join("profiles.d");
    fs::create_dir_all(&profiles_dir).expect("profiles.d creates");
    fs::write(
        profiles_dir.join("broken.yml"),
        "profile:\n  name: brokenos\n  inherits: [generic]\n  detection:\n    - BrokenOS\nrules:\n  - description: broken\n    regex: '('\n    color: f#ff0000\n",
    )
    .expect("profile writes");

    isolated("prismtty", temp.path())
        .write_stdin("BrokenOS version 1\nline2\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("BrokenOS version 1"))
        .stdout(predicate::str::contains("line2"))
        .stderr(predicate::str::contains("automatic profile switch ignored"));
}

/// An output stream whose reader is already gone, so every write fails with
/// EPIPE at once instead of racing a `head` that exits. A socket pair is used
/// rather than a pipe because std creates it close-on-exec, so no concurrently
/// spawned test child can inherit the read end and keep it open.
#[cfg(unix)]
fn output_with_no_reader() -> Stdio {
    let (writer, reader) = std::os::unix::net::UnixStream::pair().expect("socket pair");
    drop(reader);
    Stdio::from(std::os::fd::OwnedFd::from(writer))
}

#[cfg(unix)]
fn run_with_closed_stdout(bin: &str, dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    StdCommand::new(assert_cmd::cargo::cargo_bin(bin))
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("xdg"))
        .env("PRISMTTY_RUNTIME_DIR", dir.join("runtime"))
        .args(args)
        .stdin(fs::File::open(dir.join("input")).expect("input opens"))
        .stdout(output_with_no_reader())
        .stderr(Stdio::piped())
        .output()
        .expect("command runs")
}

// A reader that quits early (`| head`, `less` then `q`) is not an error: exit
// quietly with the status a SIGPIPE-killed filter reports, 128 + 13, and never
// panic in the informational commands.
#[cfg(unix)]
#[test]
fn closed_stdout_exits_quietly_with_sigpipe_status() {
    let temp = tempfile::tempdir().expect("tempdir");
    fs::write(
        temp.path().join("input"),
        "interface Gi0/1 192.0.2.1 up\n".repeat(4096),
    )
    .expect("input writes");
    let profile = temp.path().join("profile.yml");
    fs::write(
        &profile,
        "profile:\n  name: custom\nrules:\n  - description: word\n    regex: up\n    color: f#00ff00\n",
    )
    .expect("profile writes");
    let profile = profile.to_str().expect("utf-8 path");
    let fixture = temp.path().join("input");
    let fixture = fixture.to_str().expect("utf-8 path");
    // Output that ends mid-line sits in stdout's buffer until it is flushed.
    let unterminated = temp.path().join("unterminated");
    fs::write(&unterminated, "interface Gi0/1 192.0.2.1 up").expect("fixture writes");
    let unterminated = unterminated.to_str().expect("utf-8 path");

    let cases: &[(&str, &[&str])] = &[
        ("prismtty", &["--profile", "cisco"]),
        ("ptty", &["--profile", "cisco", "printf", "192.0.2.1 up\\n"]),
        ("prismtty", &["--version"]),
        ("prismtty", &["--help"]),
        ("prismtty", &["--reload"]),
        ("prismtty", &["profiles", "list"]),
        ("prismtty", &["profiles", "show", "cisco"]),
        ("prismtty", &["profiles", "validate", profile]),
        ("prismtty", &["profiles", "test", "cisco", fixture]),
        ("prismtty", &["profiles", "test", "cisco", unterminated]),
    ];
    for (bin, args) in cases {
        let output = run_with_closed_stdout(bin, temp.path(), args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(141),
            "{bin} {args:?} exited {:?}; stderr: {stderr:?}",
            output.status
        );
        assert!(
            stderr.is_empty(),
            "{bin} {args:?} wrote to stderr: {stderr:?}"
        );
    }
}

// Diagnostics are best effort: a dead stderr must not cost the highlighted
// output or panic the process.
#[cfg(unix)]
#[test]
fn closed_stderr_does_not_stop_highlighting() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut child = StdCommand::new(assert_cmd::cargo::cargo_bin("prismtty"))
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path().join("xdg"))
        .env("PRISMTTY_RUNTIME_DIR", temp.path().join("runtime"))
        .args(["-b", "--show-profile", "-p", "cisco"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(output_with_no_reader())
        .spawn()
        .expect("prismtty spawns");
    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(b"GigabitEthernet0/0 192.0.2.1 YES up up\n")
        .expect("input writes");
    let output = child.wait_with_output().expect("prismtty exits");

    assert!(output.status.success(), "exited {:?}", output.status);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("GigabitEthernet0/0"),
        "highlighted output was lost: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

// Registration only serves `--reload`. A runtime path another user squatted
// (here a plain file, which needs no second account to set up) must disable
// reload for the session, not stop the highlighting.
#[test]
fn unusable_runtime_dir_only_disables_reload() {
    let temp = tempfile::tempdir().expect("tempdir");
    let squatted = temp.path().join("runtime-is-a-file");
    fs::write(&squatted, "").expect("plain file writes");

    isolated("prismtty", temp.path())
        .env("PRISMTTY_RUNTIME_DIR", &squatted)
        .args(["--profile", "generic"])
        .write_stdin("192.0.2.44 down\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("192.0.2.44"))
        .stdout(predicate::str::contains("\u{1b}["))
        .stderr(predicate::str::contains("reload disabled"));
    isolated("ptty", temp.path())
        .env("PRISMTTY_RUNTIME_DIR", &squatted)
        .args(["--profile", "generic", "printf", "192.0.2.44 down\\n"])
        .assert()
        .success()
        .stdout(predicate::str::contains("192.0.2.44"))
        .stderr(predicate::str::contains("reload disabled"));
    // An explicit --reload still reports the problem.
    isolated("prismtty", temp.path())
        .env("PRISMTTY_RUNTIME_DIR", &squatted)
        .arg("--reload")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not a directory"));
    assert_eq!(fs::read(&squatted).expect("file reads"), b"");
}

#[cfg(unix)]
fn mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    fs::metadata(path)
        .expect("metadata reads")
        .permissions()
        .mode()
        & 0o7777
}

// PRISMTTY_RUNTIME_DIR names a directory the user already has, such as their
// home or a shared one. Tightening is for PrismTTY's own directory only: an
// existing override keeps its mode, and one others can write to is refused
// rather than silently stripped of its sticky and world-write bits.
#[cfg(unix)]
#[test]
fn runtime_dir_override_keeps_its_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let readable = temp.path().join("readable");
    let shared = temp.path().join("shared");
    fs::create_dir(&readable).expect("dir creates");
    fs::create_dir(&shared).expect("dir creates");
    fs::set_permissions(&readable, fs::Permissions::from_mode(0o755)).expect("mode sets");
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o1777)).expect("mode sets");

    isolated("prismtty", temp.path())
        .env("PRISMTTY_RUNTIME_DIR", &readable)
        .write_stdin("192.0.2.44 down\n")
        .assert()
        .success()
        .stderr(predicate::str::contains("reload disabled").not());
    isolated("prismtty", temp.path())
        .env("PRISMTTY_RUNTIME_DIR", &readable)
        .arg("--reload")
        .assert()
        .success();
    assert_eq!(mode(&readable), 0o755);
    assert_eq!(mode(&readable.join("pids.d")), 0o700);

    isolated("prismtty", temp.path())
        .env("PRISMTTY_RUNTIME_DIR", &shared)
        .write_stdin("192.0.2.44 down\n")
        .assert()
        .success()
        .stderr(predicate::str::contains("writable by other users"));
    isolated("prismtty", temp.path())
        .env("PRISMTTY_RUNTIME_DIR", &shared)
        .arg("--reload")
        .assert()
        .failure();
    assert_eq!(mode(&shared), 0o1777);
    assert!(!shared.join("pids.d").exists());
    assert!(!shared.join("reload").exists());

    let missing = temp.path().join("new").join("runtime");
    isolated("prismtty", temp.path())
        .env("PRISMTTY_RUNTIME_DIR", &missing)
        .write_stdin("192.0.2.44 down\n")
        .assert()
        .success();
    assert_eq!(mode(&missing), 0o700);
}

// A session killed outright, or ended by a signal in pipe mode, never removes
// its pid file. Registration prunes the leftovers, so pids.d stays bounded by
// the sessions still alive instead of growing until someone runs --reload.
#[cfg(unix)]
#[test]
fn registration_prunes_pid_files_of_dead_sessions() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let pid_dir = temp.path().join("runtime").join("pids.d");
    fs::create_dir_all(&pid_dir).expect("pids.d creates");
    for dir in [temp.path().join("runtime"), pid_dir.clone()] {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).expect("mode sets");
    }
    // Above any pid_max, so no live process can have this id.
    fs::write(pid_dir.join("999999999"), "").expect("stale pid file writes");
    fs::write(pid_dir.join("not-a-pid"), "").expect("stray file writes");

    isolated("prismtty", temp.path())
        .write_stdin("192.0.2.44 down\n")
        .assert()
        .success();

    let left: Vec<_> = fs::read_dir(&pid_dir)
        .expect("pids.d reads")
        .map(|entry| entry.expect("entry reads").file_name())
        .collect();
    assert!(left.is_empty(), "stale pid files survived: {left:?}");
}

// The XDG Base Directory spec says a relative XDG_CONFIG_HOME is invalid and
// must be ignored. Honouring it loaded profiles and rules from whatever
// directory prismtty happened to run in, such as an untrusted checkout.
#[test]
fn relative_xdg_config_home_is_ignored() {
    let temp = tempfile::tempdir().expect("tempdir");
    let checkout = temp.path().join("checkout");
    let planted = checkout.join("rel").join("prismtty");
    fs::create_dir_all(planted.join("profiles.d")).expect("planted dirs create");
    fs::write(
        planted.join("profiles.d").join("planted.yml"),
        "profile:\n  name: planted\n  inherits: [generic]\nrules: []\n",
    )
    .expect("planted profile writes");
    fs::write(
        planted.join("config.yml"),
        "rules:\n  - description: planted\n    regex: world\n    color: f#00ff00\n",
    )
    .expect("planted config writes");
    let home = temp.path().join("home");
    fs::create_dir(&home).expect("home creates");
    let relative_xdg = |args: &[&str]| {
        let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
        cmd.current_dir(&checkout)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", "rel")
            .env("PRISMTTY_RUNTIME_DIR", temp.path().join("runtime"))
            .args(args);
        cmd
    };

    relative_xdg(&["profiles", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("planted").not());
    relative_xdg(&[])
        .write_stdin("hello world\n")
        .assert()
        .success()
        .stdout("hello world\n");
}
