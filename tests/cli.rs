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

// A profiles subcommand takes a fixed number of arguments. A surplus one used
// to be dropped without a word, so `profiles validate good.yml broken.yml`
// reported success having read only the first file.
#[test]
fn profiles_subcommands_reject_surplus_arguments() {
    let cases: &[(&[&str], &str)] = &[
        (&["profiles", "list", "surplus-arg"], "surplus-arg"),
        (&["profiles", "show", "cisco", "surplus-arg"], "surplus-arg"),
        (
            &[
                "profiles",
                "validate",
                "profiles/custom-router.example.yml",
                "missing-profile.yml",
            ],
            "missing-profile.yml",
        ),
        (
            &[
                "profiles",
                "test",
                "cisco",
                "fixtures/cisco.txt",
                "surplus-arg",
            ],
            "surplus-arg",
        ),
    ];

    for (args, surplus) in cases {
        let output = Command::cargo_bin("prismtty")
            .expect("binary exists")
            .args(*args)
            .output()
            .expect("command runs");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "expected {args:?} to fail, stdout: {:?}, stderr: {stderr:?}",
            String::from_utf8_lossy(&output.stdout),
        );
        assert!(
            stderr.contains("unexpected") && stderr.contains(surplus),
            "stderr for {args:?} did not name the surplus argument: {stderr:?}"
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

// Another program can leave `O_NONBLOCK` set on the pipe or terminal prismtty
// inherits as stdin. A read that finds it empty means "no input yet": the
// filter waits for it instead of failing with EAGAIN.
#[test]
#[cfg(unix)]
fn stdin_mode_waits_for_input_on_a_nonblocking_stdin() {
    use std::os::fd::{FromRawFd, OwnedFd};

    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors `pipe` writes; each is
    // then owned by exactly one `OwnedFd`. The fcntl calls only set flags on
    // those descriptors: O_NONBLOCK on the read end, and close-on-exec on
    // the write end so the filter does not inherit it and wait on itself for
    // end of input.
    let (read_end, write_end) = unsafe {
        assert_eq!(libc::pipe(fds.as_mut_ptr()), 0, "pipe");
        let flags = libc::fcntl(fds[0], libc::F_GETFL);
        assert_eq!(
            libc::fcntl(fds[0], libc::F_SETFL, flags | libc::O_NONBLOCK),
            0,
            "could not set O_NONBLOCK"
        );
        assert_eq!(
            libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC),
            0,
            "could not set FD_CLOEXEC"
        );
        (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))
    };
    let mut empty_config = tempfile::NamedTempFile::new().expect("empty config");
    writeln!(empty_config, "rules: []").expect("write empty config");

    let child = StdCommand::new(assert_cmd::cargo::cargo_bin("prismtty"))
        .args(["--profile", "generic", "--config"])
        .arg(empty_config.path())
        .stdin(Stdio::from(read_end))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("prismtty starts");
    // Long enough for the filter to reach its first read of the empty pipe.
    thread::sleep(Duration::from_millis(300));
    // A filter that already gave up closed the pipe; its exit status below
    // is the failure worth reporting, not this write.
    let mut write_end = fs::File::from(write_end);
    let _ = write_end.write_all(b"late input\n");
    drop(write_end);

    let output = child.wait_with_output().expect("prismtty exits");
    assert!(
        output.status.success(),
        "stderr: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "late input\n");
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

// A descriptor the caller hands the wrapped command on purpose (`cmd 3<file`,
// `--passphrase-fd 3`, a process substitution) reaches it as it does without
// the wrapper, and nothing prismtty opens itself joins it: the command lists
// the same descriptors as when the same shell runs it directly, with
// `--trace-io` on as well. One shell starts all three runs, so whatever else
// it inherited is in every list.
#[test]
#[cfg(unix)]
fn wrapped_command_inherits_exactly_the_callers_extra_descriptors() {
    let work = tempfile::tempdir().expect("work directory");
    let config = work.path().join("empty.yml");
    fs::write(&config, "rules: []\n").expect("write config");
    let data = work.path().join("data.txt");
    fs::write(&data, "caller-data\n").expect("write data");

    let output = StdCommand::new("/bin/sh")
        .args([
            "-c",
            "list='read line <&7; echo \"fd7:$line\"; echo fd-list; exec ls /dev/fd'; \
             echo direct-start; /bin/sh -c \"$list\" 7<\"$DATA\"; echo direct-end; \
             echo plain-start; \"$PTTY\" --config \"$CONFIG\" -- /bin/sh -c \"$list\" 7<\"$DATA\"; \
             echo plain-end=$?; \
             echo traced-start; \"$PTTY\" --config \"$CONFIG\" --trace-io \"$TRACE\" -- \
             /bin/sh -c \"$list\" 7<\"$DATA\"; echo traced-end=$?",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", work.path())
        .env("PRISMTTY_RUNTIME_DIR", work.path().join("runtime"))
        .env("PTTY", assert_cmd::cargo::cargo_bin("ptty"))
        .env("CONFIG", &config)
        .env("TRACE", work.path().join("trace.log"))
        .env("DATA", &data)
        .stdin(Stdio::null())
        .output()
        .expect("shell runs");
    let text = strip_csi(&output.stdout).replace('\r', "");
    assert!(output.status.success(), "{output:?}");
    let run = |start: &str, end: &str| -> Vec<u32> {
        let run = text
            .split(start)
            .nth(1)
            .and_then(|rest| rest.split(end).next())
            .unwrap_or_default();
        assert!(
            run.contains("fd7:caller-data"),
            "{start}: fd 7 did not reach the command: {text:?}"
        );
        run.split("fd-list")
            .nth(1)
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|word| word.parse().ok())
            .collect()
    };
    let direct = run("direct-start", "direct-end");
    assert!(direct.contains(&7), "direct: {text:?}");
    assert_eq!(run("plain-start", "plain-end=0"), direct, "{text:?}");
    assert_eq!(run("traced-start", "traced-end=0"), direct, "{text:?}");
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

// A pipe or a file is not a terminal, so nothing there needs a CR in front of
// each LF. The saved bytes are the ones the command wrote, as they are
// without the wrapper.
#[test]
#[cfg(unix)]
fn wrapped_output_into_a_pipe_or_file_keeps_the_commands_bytes() {
    let home = tempfile::tempdir().expect("home directory");
    let config = home.path().join("empty.yml");
    fs::write(&config, "rules: []\n").expect("config writes");
    let saved = home.path().join("saved.out");
    let ptty = |script: &str| {
        let mut command = StdCommand::new(assert_cmd::cargo::cargo_bin("ptty"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", home.path())
            .env("PRISMTTY_RUNTIME_DIR", home.path().join("runtime"))
            .arg("--config")
            .arg(&config)
            .args(["--", "/bin/sh", "-c", script])
            .stdin(Stdio::null());
        command
    };
    let numbers: String = (1..=20000).map(|n| format!("{n}\n")).collect();
    let cases: [(&str, &[u8]); 3] = [
        ("printf 'a\\tb\\nc\\n'", b"a\tb\nc\n"),
        // A CR the command wrote itself stays, alone or in front of a LF.
        ("printf 'x\\r\\ny\\rz\\n'", b"x\r\ny\rz\n"),
        // Long enough to cross many reads from the PTY.
        ("seq 1 20000", numbers.as_bytes()),
    ];

    for (script, expected) in cases {
        let piped = ptty(script).output().expect("ptty runs into a pipe");
        assert!(piped.status.success(), "{script}: {piped:?}");
        assert!(
            piped.stdout == expected,
            "{script} into a pipe: {:?}",
            String::from_utf8_lossy(&piped.stdout[..piped.stdout.len().min(200)])
        );

        let file = fs::File::create(&saved).expect("output file");
        let status = ptty(script)
            .stdout(file)
            .status()
            .expect("ptty runs into a file");
        assert!(status.success(), "{script}: {status:?}");
        let written = fs::read(&saved).expect("saved output");
        assert!(
            written == expected,
            "{script} into a file: {:?}",
            String::from_utf8_lossy(&written[..written.len().min(200)])
        );
    }
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
    let expected = format!("marker-{}\n", work_path.display());

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

// A file is read in blocks of 8192 bytes. Lines that put a run of controls,
// each ending in a cut-off character, at the end of three blocks in a row
// must not take the other lines of those blocks with them.
#[test]
fn reads_ending_in_runs_of_cut_off_characters_do_not_hide_their_text() {
    let mut empty_config = tempfile::NamedTempFile::new().expect("empty config");
    writeln!(empty_config, "rules: []").expect("write empty config");
    let mut ending = b"\x1b[1\xe2\x80".repeat(7);
    ending.extend_from_slice(b"\x1b[");
    let mut input = Vec::new();
    let mut lines = 0;
    for block in 0..3 {
        let end = input.len() + 8192 - ending.len();
        loop {
            let line = format!("LINE-{block}-{lines:04} visible text that the user should see\n");
            if input.len() + line.len() > end {
                break;
            }
            input.extend_from_slice(line.as_bytes());
            lines += 1;
        }
        input.resize(end, b' ');
        input.extend_from_slice(&ending);
    }
    input.extend_from_slice(b"tail line\n");
    let mut file = tempfile::NamedTempFile::new().expect("input file");
    file.write_all(&input).expect("write input");

    for mode in [None, Some("--sanitize"), Some("--strip-ansi")] {
        let output = StdCommand::new(assert_cmd::cargo::cargo_bin("prismtty"))
            .args(mode)
            .args(["--profile", "generic", "--config"])
            .arg(empty_config.path())
            .stdin(fs::File::open(file.path()).expect("open input"))
            .output()
            .expect("prismtty runs");

        assert!(output.status.success(), "{mode:?}: {output:?}");
        // The unfinished CSI at the end of a block takes the first byte of
        // the next line as its final byte, so count the rest of each line.
        let written = output
            .stdout
            .windows(13)
            .filter(|window| window == b" visible text")
            .count();
        assert_eq!(
            written,
            lines,
            "{mode:?} wrote {} of {} bytes",
            output.stdout.len(),
            input.len()
        );
        assert!(output.stdout.ends_with(b"ail line\n"), "{mode:?}");
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
        d\xdd52;c;U0VDUkVU\x07e\xc3\x90q\x1b\\f\xc3\x9e0;t\x07g\n";

    for (input, expected) in [
        (input.as_slice(), expected.as_slice()),
        // A character cut off by the end of the input stays whole.
        (b"a\x1b\xe2\x9d", b"a\xe2\x9d"),
    ] {
        let mut cmd = Command::cargo_bin("ct")
            .unwrap_or_else(|_| Command::cargo_bin("prismtty").expect("binary exists"));
        cmd.arg("--sanitize")
            .arg("--config")
            .arg(empty_config.path())
            .write_stdin(input);
        let output = cmd.output().expect("command runs");

        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, expected, "{:02x?}", output.stdout);
    }

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

// In UTF-8 mode xterm ignores a C1 control in the escape state, encoded as
// UTF-8 or as a lone raw byte, and Alacritty ignores any byte from 80 up
// there, so ESC C2 80 ] and ESC 80 ] open an OSC. --sanitize drops the ESC
// and reads the control on its own. A string control is still dropped with
// its string, and with the chain that reached it.
#[test]
fn sanitize_drops_escapes_that_c1_controls_follow() {
    let mut empty_config = tempfile::NamedTempFile::new().expect("empty config");
    writeln!(empty_config, "rules: []").expect("write empty config");
    let input = b"a\x1b\xc2\x80]0;title\x07b\x1b\x00\xc2\x9b]0;t\x07c\x1b\xc2\x9c]0;t\x07\
        d\x1b\xc2\x8e]52;c;U0VDUkVU\x07e\x1b\xc2\x80\xc2\x81]0;t\x07f\x1b\xc2\x80\xc3\x9d0;t\x07\
        g\x1b\xc2\x9d0;t\x07h\x1b\x80i\x1b\x80]0;title\x07j\x1b\x9b]0;t\x07\
        k\x1b\x00\x8e]52;c;U0VDUkVU\x07l\x1b[1\x1b\xc2\x9b\x1b]0;t\x07m\x1b(\x1b\x8e\x1b]0;t\x07\
        n\x1b[1\x1b\x9b1mo\n";
    let expected = b"a\xc2\x80]0;title\x07b\x00\xc2\x9b]0;t\x07c\xc2\x9c]0;t\x07\
        d\xc2\x8e]52;c;U0VDUkVU\x07e\xc2\x80\xc2\x81]0;t\x07f\xc2\x80\xc3\x9d0;t\x07\
        gh\x80i\x80]0;title\x07j\x9b]0;t\x07\
        k\x00\x8e]52;c;U0VDUkVU\x07lm\
        n\x9b1mo\n";

    for (input, expected) in [
        (input.as_slice(), expected.as_slice()),
        // A chain cut off by the end of the input is dropped whole too.
        (b"a\x1b[1\x1b\xc2\x9b\x1b]0;t\x07", b"a"),
    ] {
        let mut cmd = Command::cargo_bin("ct")
            .unwrap_or_else(|_| Command::cargo_bin("prismtty").expect("binary exists"));
        cmd.arg("--sanitize")
            .arg("--config")
            .arg(empty_config.path())
            .write_stdin(input);
        let output = cmd.output().expect("command runs");

        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, expected, "{:02x?}", output.stdout);
    }
}

// DOCS selects how xterm decodes its input, unless its utf8 resource is
// "always": after ESC % @ it reads U+201D (E2 80 9D) byte by byte, and the 9D
// opens an OSC. --sanitize drops every DOCS; without it, DOCS passes through.
#[test]
fn sanitize_drops_docs_sequences() {
    let mut empty_config = tempfile::NamedTempFile::new().expect("empty config");
    writeln!(empty_config, "rules: []").expect("write empty config");
    let input = "a\x1b%@\u{201d}0;title\x07b\x1b%Gc\x1b%/Gd\x1b\x00%@e\x1b%\u{c0}f\n";
    let expected = "a\u{201d}0;title\x07bcde\u{c0}f\n";

    for (sanitize, expected) in [(true, expected), (false, input)] {
        let mut cmd = Command::cargo_bin("ct")
            .unwrap_or_else(|_| Command::cargo_bin("prismtty").expect("binary exists"));
        if sanitize {
            cmd.arg("--sanitize");
        }
        cmd.arg("--config")
            .arg(empty_config.path())
            .write_stdin(input);
        let output = cmd.output().expect("command runs");

        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, expected.as_bytes(), "{:02x?}", output.stdout);
    }
}

// In UTF-8 mode xterm ignores a C1 control inside a CSI and reads a Latin-1
// character there as the ASCII byte it mirrors, so ESC [ ? 4 U+00B2 h and
// ESC [ ? 4 C2 80 2 h are DECSET 42 (DECNRCM) to it, which can take it out of
// UTF-8, and then the 9D in U+201D opens an OSC. --sanitize drops such CSIs,
// a control that a dropped ESC cancelled, and every change to mode 42;
// without it, they pass through.
#[test]
fn sanitize_drops_csis_that_could_switch_xterm_out_of_utf8() {
    let mut empty_config = tempfile::NamedTempFile::new().expect("empty config");
    writeln!(empty_config, "rules: []").expect("write empty config");
    let input = b"a\x1b[?4\xc2\xb2h\xe2\x80\x9d0;t\x07b\x1b[?4\xc2\x802h\xe2\x80\x9dc\
        \x1b[?4\x1b\xc2\xb2h\xe2\x80\x9dd\x1b[1\x1b\xc3\x9ee\x1b[?42h\xe2\x80\x9df\
        \x1b[?1;042l\xe2\x80\x9dg\xc2\x9b?42r\x9b?1:42hh\x1b[1;31mi\x1b[0m\n";
    let expected = b"a\xe2\x80\x9d0;t\x07b\xc2\x802h\xe2\x80\x9dc\xc2\xb2h\xe2\x80\x9dd\
        \xc3\x9ee\xe2\x80\x9df\xe2\x80\x9dgh\x1b[1;31mi\x1b[0m\n";

    for (sanitize, expected) in [(true, expected.as_slice()), (false, input.as_slice())] {
        let mut cmd = Command::cargo_bin("ct")
            .unwrap_or_else(|_| Command::cargo_bin("prismtty").expect("binary exists"));
        if sanitize {
            cmd.arg("--sanitize");
        }
        cmd.arg("--config")
            .arg(empty_config.path())
            .write_stdin(input.as_slice());
        let output = cmd.output().expect("command runs");

        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, expected, "{:02x?}", output.stdout);
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

// `profiles test` prints a captured fixture to the terminal, so the two flags
// that remove escapes from program output apply to it as well: with
// `--sanitize` an OSC 52 clipboard write in the capture must not reach stdout.
#[test]
fn profiles_test_applies_sanitize_and_strip_ansi_to_the_fixture() {
    let mut fixture = tempfile::NamedTempFile::new().expect("fixture");
    fixture
        .write_all(b"before\x1b]52;c;U0VDUkVU\x07after \x1b[31mred\x1b[0m plain\n")
        .expect("write fixture");

    let run = |flag: &str| {
        let output = Command::cargo_bin("prismtty")
            .expect("binary exists")
            .arg(flag)
            .args(["profiles", "test", "generic"])
            .arg(fixture.path())
            .output()
            .expect("command runs");
        assert!(output.status.success(), "{flag}: {output:?}");
        output.stdout
    };

    let sanitized = run("--sanitize");
    assert_eq!(
        String::from_utf8_lossy(&sanitized),
        "beforeafter \u{1b}[31mred\u{1b}[0m plain\n",
        "--sanitize keeps colors and drops the string escape"
    );

    let stripped = run("--strip-ansi");
    assert_eq!(
        String::from_utf8_lossy(&stripped),
        "beforeafter red plain\n",
        "--strip-ansi drops every source escape"
    );
}

// `--show-profile` and `--benchmark` ask for a report, and an input that ends
// before its first byte is still a run: both reports are written.
#[test]
fn empty_input_still_reports_the_profile_and_the_benchmark() {
    let mut empty_config = tempfile::NamedTempFile::new().expect("empty config");
    writeln!(empty_config, "rules: []").expect("write empty config");

    let output = Command::cargo_bin("prismtty")
        .expect("binary exists")
        .args(["--show-profile", "--benchmark", "--profile", "generic"])
        .arg("--config")
        .arg(empty_config.path())
        .write_stdin("")
        .output()
        .expect("command runs");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "{stderr:?}");
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    assert!(
        stderr.contains("prismtty: profiles selected: generic"),
        "the profile report is missing: {stderr:?}"
    );
    assert!(
        stderr.contains("Benchmark results (time spent, match count):"),
        "the benchmark report is missing: {stderr:?}"
    );
}

// A vendor's strong signal counts where it begins a word, not inside one:
// `rhel` in `ServerHello` is not Red Hat. A model or version glued on after
// it (`Nexus9000`, `rhel9`) still counts.
#[test]
fn strong_signals_match_only_where_they_begin_a_word() {
    let mut empty_config = tempfile::NamedTempFile::new().expect("empty config");
    writeln!(empty_config, "rules: []").expect("write empty config");
    let selected = |line: &str| {
        let output = Command::cargo_bin("prismtty")
            .expect("binary exists")
            .arg("--show-profile")
            .arg("--config")
            .arg(empty_config.path())
            .write_stdin(format!("{line}\n"))
            .output()
            .expect("command runs");
        assert!(output.status.success(), "{output:?}");
        String::from_utf8_lossy(&output.stderr).into_owned()
    };

    for line in [
        "TLS handshake: ServerHello received",
        "route via japan-osaka-1 gateway",
        "BIOS XE firmware table",
        "NASA version 2 dataset",
    ] {
        // Auto-detection that stays on generic reports nothing.
        assert_eq!(selected(line), "", "{line:?}");
    }
    for (line, profile) in [
        ("Cisco IOS XE Software, Version 17.9.4", "cisco"),
        (": ASA Version 9.8(2)", "cisco"),
        ("sw-version: PAN-OS 10.1.6", "palo-alto"),
        ("Red Hat Enterprise Linux release 9.3 (RHEL)", "linux-unix"),
        ("--- JUNOS 22.4R3 Kernel 64-bit", "juniper"),
        ("cisco Nexus9000 C93180YC-EX chassis", "cisco"),
        ("Linux build01 5.14.0-362.el9.x86_64 rhel9", "linux-unix"),
        (
            "Welcome to Ubuntu2204 (GNU/Linux 5.15.0-91-generic x86_64)",
            "linux-unix",
        ),
        ("Version: FortiGateRugged-60F v7.2.5,build1517", "fortinet"),
        ("Version: FortiGate-VM64 v7.4.1", "fortinet"),
        ("Arista Networks EOS shell", "arista"),
        ("Software image version: 4.31.1F (Arista cEOSLab)", "arista"),
        ("ArubaOS-CX Version 10.13.1000", "arubacx"),
        ("Versa Networks FlexVNF software", "versa"),
        ("versa-flexvnf release 22.1.3", "versa"),
        ("Model: PA-VM", "palo-alto"),
    ] {
        assert!(
            selected(line).contains(&format!("generic, {profile}")),
            "{line:?} should select {profile}: {}",
            selected(line)
        );
    }
}

/// Runs the filter over `input` with exactly the given locale variables.
fn filter_with_locale(
    config: &std::path::Path,
    flags: &[&str],
    locale: &[(&str, &str)],
    input: &[u8],
) -> Vec<u8> {
    let mut command = Command::cargo_bin("prismtty").expect("binary exists");
    command
        .args(["--rgb", "--no-auto-detect", "--config"])
        .arg(config)
        .args(flags)
        .env_remove("LC_ALL")
        .env_remove("LC_CTYPE")
        .env_remove("LANG");
    for (name, value) in locale {
        command.env(name, value);
    }
    let output = command
        .write_stdin(input.to_vec())
        .output()
        .expect("command runs");
    assert!(output.status.success(), "{output:?}");
    output.stdout
}

fn config_that_paints_up_green() -> tempfile::NamedTempFile {
    let mut config = tempfile::NamedTempFile::new().expect("config");
    writeln!(
        config,
        "rules:\n  - description: up\n    regex: \\bup\\b\n    color: f#00ff00"
    )
    .expect("write config");
    config
}

// A raw byte from 90, 98, 9D, 9E or 9F opens a string control only on a
// terminal that is not in UTF-8 mode. On a UTF-8 terminal it is an invalid
// byte, and line noise, a legacy code page or a UTF-8 character cut in half
// all produce one with no terminator to follow: it used to swallow every
// later byte of the session. When the locale says UTF-8, such a string ends
// at the end of its line, so one stray byte costs at most the rest of that
// line.
#[test]
fn in_a_utf8_locale_a_stray_raw_c1_string_opener_costs_one_line_at_most() {
    let config = config_that_paints_up_green();
    let highlighted_up = "\u{1b}[38;2;0;255;0mup\u{1b}[0m";

    for opener in *b"\x90\x98\x9d\x9e\x9f" {
        let mut input = b"line1\nfoo ".to_vec();
        input.push(opener);
        input.extend_from_slice(b" bar\nGi0/1 is up\nline4\n");

        for locale in [
            &[("LC_ALL", "en_US.UTF-8")][..],
            &[("LANG", "C.UTF-8")][..],
            &[("LC_CTYPE", "UTF-8"), ("LANG", "C")][..],
        ] {
            // Default mode is transparent: every byte arrives, in order, and
            // the lines after the stray byte are highlighted again.
            let mut expected = b"line1\nfoo ".to_vec();
            expected.push(opener);
            expected.extend_from_slice(b" bar\nGi0/1 is ");
            expected.extend_from_slice(highlighted_up.as_bytes());
            expected.extend_from_slice(b"\nline4\n");
            assert_eq!(
                filter_with_locale(config.path(), &[], locale, &input),
                expected,
                "default mode, opener {opener:#x}, {locale:?}"
            );

            // `--sanitize` still removes the opener and what it may hide,
            // but only to the end of that line.
            assert_eq!(
                String::from_utf8_lossy(&filter_with_locale(
                    config.path(),
                    &["--sanitize"],
                    locale,
                    &input
                )),
                format!("line1\nfoo \nGi0/1 is {highlighted_up}\nline4\n"),
                "--sanitize, opener {opener:#x}, {locale:?}"
            );
        }
    }
}

// Without a UTF-8 locale nothing changes: the byte is the control an 8-bit
// terminal reads, and the string runs to its terminator. `LC_ALL` wins over
// `LANG`, and no locale at all is not a UTF-8 one.
#[test]
fn outside_a_utf8_locale_a_raw_c1_string_still_runs_to_its_terminator() {
    let config = config_that_paints_up_green();
    let input = b"line1\nfoo \x9f bar\nGi0/1 is up\nline4\n";

    for locale in [
        &[][..],
        &[("LC_ALL", "C")][..],
        &[("LANG", "en_US.ISO-8859-1")][..],
        &[("LC_ALL", "C"), ("LANG", "en_US.UTF-8")][..],
    ] {
        for flags in [&[][..], &["--sanitize"][..]] {
            assert_eq!(
                String::from_utf8_lossy(&filter_with_locale(config.path(), flags, locale, input)),
                "line1\nfoo ",
                "{flags:?}, {locale:?}"
            );
        }
    }
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

// An empty PRISMTTY_RUNTIME_DIR was taken as the path "", which cannot be
// created: the session ran without live reload, as it still does, but the
// reason given was a bare "No such file or directory (os error 2)" that
// named neither a path nor the variable.
#[test]
fn empty_runtime_dir_variable_is_named_as_the_reason_reload_is_off() {
    let temp = tempfile::tempdir().expect("tempdir");
    // The default runtime directory would be made under TMPDIR. An empty
    // override is not taken as unset, so nothing may appear there either.
    let system_temp = tempfile::tempdir().expect("tempdir");

    isolated("prismtty", temp.path())
        .current_dir(temp.path())
        .env("TMPDIR", system_temp.path())
        .env("PRISMTTY_RUNTIME_DIR", "")
        .arg("--no-auto-detect")
        .write_stdin("hello\n")
        .assert()
        .success()
        .stdout("hello\n")
        .stderr(
            "prismtty: live reload disabled for this session: PRISMTTY_RUNTIME_DIR is set but empty (unset it or set it to a directory)\n",
        );
    for dir in [temp.path(), system_temp.path()] {
        let left: Vec<_> = fs::read_dir(dir)
            .expect("directory reads")
            .map(|entry| entry.expect("entry reads").file_name())
            .collect();
        assert!(left.is_empty(), "something was created: {left:?}");
    }
}

// A relative HOME names a directory under wherever prismtty happens to run,
// just as a relative XDG_CONFIG_HOME does, and was honoured all the same:
// rules and profiles were loaded from the current directory. It is ignored
// for these lookups now, as silently as a relative XDG_CONFIG_HOME is. An
// absolute XDG_CONFIG_HOME still applies beside it.
#[test]
fn relative_home_is_ignored_for_config_and_profile_lookup() {
    let temp = tempfile::tempdir().expect("tempdir");
    let checkout = temp.path().join("checkout");
    let planted_home = checkout.join("rel");
    let planted = planted_home.join(".config").join("prismtty");
    fs::create_dir_all(planted.join("profiles.d")).expect("planted dirs create");
    fs::write(
        planted.join("profiles.d").join("planted.yml"),
        "profile:\n  name: planted\n  inherits: [generic]\nrules: []\n",
    )
    .expect("planted profile writes");
    for config in [
        planted_home.join(".chromaterm.yml"),
        planted.join("config.yml"),
    ] {
        fs::write(
            config,
            "rules:\n  - description: planted\n    regex: world\n    color: f#00ff00\n",
        )
        .expect("planted config writes");
    }
    let xdg = temp.path().join("xdg");
    fs::create_dir_all(xdg.join("prismtty").join("profiles.d")).expect("xdg dirs create");
    fs::write(
        xdg.join("prismtty").join("profiles.d").join("kept.yml"),
        "profile:\n  name: kept\n  inherits: [generic]\nrules: []\n",
    )
    .expect("xdg profile writes");
    let relative_home = |args: &[&str]| {
        let mut cmd = Command::cargo_bin("prismtty").expect("binary exists");
        cmd.current_dir(&checkout)
            .env("HOME", "rel")
            .env_remove("XDG_CONFIG_HOME")
            .env("PRISMTTY_RUNTIME_DIR", temp.path().join("runtime"))
            .args(args);
        cmd
    };

    relative_home(&["profiles", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("planted").not())
        .stderr("");
    relative_home(&[])
        .write_stdin("hello world\n")
        .assert()
        .success()
        .stdout("hello world\n")
        .stderr("");
    relative_home(&["profiles", "list"])
        .env("XDG_CONFIG_HOME", &xdg)
        .assert()
        .success()
        .stdout(predicate::str::contains("kept"))
        .stdout(predicate::str::contains("planted").not())
        .stderr("");
}

// RIS (`ESC c`) resets the terminal, the alternate screen included. Output
// after it is on the normal screen again, so it is highlighted again.
#[test]
fn a_full_reset_inside_the_alternate_screen_brings_highlighting_back() {
    let config = config_that_paints_up_green();

    let output = filter_with_locale(
        config.path(),
        &[],
        &[],
        b"\x1b[?1049hframe up\x1bcGi0/1 is up\n",
    );

    assert_eq!(
        String::from_utf8_lossy(&output),
        "\u{1b}[?1049hframe up\u{1b}cGi0/1 is \u{1b}[38;2;0;255;0mup\u{1b}[0m\n"
    );
}

// 4294967295 is no process id. As a `pid_t` it is -1, and asking about -1
// asks about every process, so the file counted as a live session for good.
#[cfg(unix)]
#[test]
fn registration_prunes_a_pid_file_named_above_any_process_id() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let pid_dir = temp.path().join("runtime").join("pids.d");
    fs::create_dir_all(&pid_dir).expect("pids.d creates");
    for dir in [temp.path().join("runtime"), pid_dir.clone()] {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).expect("mode sets");
    }
    fs::write(pid_dir.join("4294967295"), "").expect("pid file writes");

    isolated("prismtty", temp.path())
        .write_stdin("192.0.2.44 down\n")
        .assert()
        .success();

    let left: Vec<_> = fs::read_dir(&pid_dir)
        .expect("pids.d reads")
        .map(|entry| entry.expect("entry reads").file_name())
        .collect();
    assert!(left.is_empty(), "the pid file survived: {left:?}");
}

// A rule that can match nothing is searched again one step further on. One
// byte further is inside a multi-byte character, where a rule in UTF mode
// cannot start: PCRE2 reports a bad offset, which was taken for a runaway
// match, reported, and cost the rest of the word.
#[test]
fn a_utf_rule_that_can_match_nothing_survives_multibyte_text() {
    let mut config = tempfile::NamedTempFile::new().expect("config");
    writeln!(
        config,
        "rules:\n  - description: xs\n    regex: (*UTF)x*\n    color: f#00ff00"
    )
    .expect("write config");

    let output = Command::cargo_bin("prismtty")
        .expect("binary exists")
        .args(["--rgb", "--no-auto-detect", "--config"])
        .arg(config.path())
        .write_stdin("caf\u{e9}xx \u{4e2d}\u{6587}x\n")
        .output()
        .expect("command runs");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "caf\u{e9}\u{1b}[38;2;0;255;0mxx\u{1b}[0m \u{4e2d}\u{6587}\u{1b}[38;2;0;255;0mx\u{1b}[0m\n"
    );
}

// PCRE2 refuses to search text that is not UTF-8 with a rule in UTF mode.
// A stray byte in the output was taken for a runaway match: the rule was
// reported and switched off. It is neither: nothing is reported, and the
// rule colors what stands behind the byte.
#[test]
fn a_utf_rule_is_not_reported_for_output_that_is_not_utf8() {
    let mut config = tempfile::NamedTempFile::new().expect("config");
    writeln!(
        config,
        "rules:\n  - description: umlauts\n    regex: (*UTF)[\u{e4}\u{f6}\u{fc}]\n    color: f#00ff00"
    )
    .expect("write config");

    let output = Command::cargo_bin("prismtty")
        .expect("binary exists")
        .args(["--rgb", "--no-auto-detect", "--config"])
        .arg(config.path())
        .write_stdin(b"\xc3\xb6 caf\xe9 \xc3\xa4\nnoise \xff\n\xc3\xbc\n".as_slice())
        .output()
        .expect("command runs");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    assert_eq!(
        output.stdout,
        b"\xc3\xb6 caf\xe9 \xc3\xa4\nnoise \xff\n\x1b[38;2;0;255;0m\xc3\xbc\x1b[0m\n"
    );
}

// Nor is such output counted against the rule: `--benchmark` lists the rule
// with its matches and no match error.
#[test]
fn benchmark_counts_no_match_error_for_output_that_is_not_utf8() {
    let mut config = tempfile::NamedTempFile::new().expect("config");
    writeln!(
        config,
        "rules:\n  - description: umlauts\n    regex: (*UTF)[\u{e4}\u{f6}\u{fc}]\n    color: f#00ff00"
    )
    .expect("write config");

    let output = Command::cargo_bin("prismtty")
        .expect("binary exists")
        .args(["--benchmark", "--no-auto-detect", "--config"])
        .arg(config.path())
        .write_stdin(b"caf\xe9 \xc3\xa4\nnoise \xff\n\xc3\xbc\n".as_slice())
        .output()
        .expect("command runs");

    assert!(output.status.success(), "{output:?}");
    let report = String::from_utf8_lossy(&output.stderr);
    assert!(report.contains("umlauts"), "{report}");
    assert!(!report.contains("match error"), "{report}");
}

// PrismTTY compiles each rule with its match limits in front. PCRE2 counted
// those 40 characters into the offset of a compile error, so the offset
// pointed nowhere in the rule the user wrote.
#[test]
fn a_rule_compile_error_reports_the_offset_in_the_rule_as_written() {
    let mut config = tempfile::NamedTempFile::new().expect("config");
    writeln!(
        config,
        "rules:\n  - description: bad\n    regex: ab(cd\n    color: f#00ff00"
    )
    .expect("write config");

    Command::cargo_bin("prismtty")
        .expect("binary exists")
        .args(["--no-auto-detect", "--config"])
        .arg(config.path())
        .write_stdin("x\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("rule 'bad' failed to compile"))
        .stderr(predicate::str::contains("offset 5:"));
}

// A configuration refused for a reason that is no rule's compile error says
// so in its own words. It used to be shown as a rule named after the message
// that "failed to compile", followed by a PCRE2 error at a made-up offset.
#[test]
fn a_refused_configuration_is_not_reported_as_a_rule_that_failed_to_compile() {
    let temp = tempfile::tempdir().expect("tempdir");
    let missing_capture = temp.path().join("missing-capture.yml");
    fs::write(
        &missing_capture,
        "rules:\n  - regex: x\n    color:\n      1: bold\n",
    )
    .expect("config writes");
    let too_much_pattern = temp.path().join("too-much-pattern.yml");
    fs::write(
        &too_much_pattern,
        format!(
            "rules:\n  - regex: '{}'\n    color: bold\n",
            "a".repeat(300_000)
        ),
    )
    .expect("config writes");

    let output = isolated("prismtty", temp.path())
        .args(["--no-auto-detect", "--config"])
        .arg(&missing_capture)
        .write_stdin("x\n")
        .output()
        .expect("command runs");
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "prismtty: rule 1 references capture '1' that does not exist\n"
    );

    isolated("prismtty", temp.path())
        .args(["--no-auto-detect", "--config"])
        .arg(&too_much_pattern)
        .write_stdin("x\n")
        .assert()
        .failure()
        .stderr(predicate::str::starts_with(
            "prismtty: configuration contains ",
        ))
        .stderr(predicate::str::ends_with(
            " pattern bytes; limits are 512 rules and 262144 bytes\n",
        ))
        .stderr(predicate::str::contains("failed to compile").not())
        .stderr(predicate::str::contains("PCRE2").not());
}

#[test]
fn profiles_test_names_the_fixture_it_cannot_read() {
    let temp = tempfile::tempdir().expect("tempdir");
    let missing = temp.path().join("missing-fixture.txt");

    Command::cargo_bin("prismtty")
        .expect("binary exists")
        .args(["profiles", "test", "generic"])
        .arg(&missing)
        .assert()
        .failure()
        .stderr(predicate::str::contains("missing-fixture.txt"));
}

// A rule that is not in UTF mode may match a single byte of a multi-byte
// character, also right after an empty match. The step that keeps UTF-mode
// rules out of a character must not take such a match away.
#[test]
fn a_byte_mode_rule_still_matches_inside_a_character_after_an_empty_match() {
    let mut config = tempfile::NamedTempFile::new().expect("config");
    writeln!(
        config,
        "rules:\n  - description: mid\n    regex: '(?<=a)|\\xA9'\n    color: f#ff0000"
    )
    .expect("write config");

    let output = Command::cargo_bin("prismtty")
        .expect("binary exists")
        .args(["--rgb", "--no-auto-detect", "--config"])
        .arg(config.path())
        .write_stdin("a\u{e9}\n")
        .output()
        .expect("command runs");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "a\u{1b}[38;2;255;0;0m\u{e9}\u{1b}[0m\n"
    );
}

// `profiles test` hands the fixture to the highlighter 64 KiB at a time. A
// phrase that straddled that boundary was matched in two halves, so
// `not connected` came out as a plain `not` and a green `connected`.
#[test]
fn profiles_test_colors_a_phrase_that_spans_the_64k_chunk_boundary() {
    let mut fixture = b"ok line\n".repeat(8190);
    fixture.extend_from_slice(b"pad\nport is not ");
    assert_eq!(fixture.len(), 64 * 1024);
    fixture.extend_from_slice(b"connected\n");
    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    file.write_all(&fixture).expect("write fixture");

    let output = Command::cargo_bin("prismtty")
        .expect("binary exists")
        .args(["--rgb", "profiles", "test", "generic"])
        .arg(file.path())
        .output()
        .expect("command runs");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.ends_with("port is \u{1b}[1;38;2;255;0;0mnot connected\u{1b}[0m\n"),
        "{:?}",
        &stdout[stdout.len().saturating_sub(80)..]
    );
}

// GNU screen and tmux read `ESC k title ESC \` as the window title, which is
// what --sanitize is there to keep a remote device from setting.
#[test]
fn sanitize_drops_the_screen_and_tmux_title_string() {
    let config = config_that_paints_up_green();
    let input = b"one \x1bktitle\x1b\\two\n";

    for flags in [&["--sanitize"][..], &["--sanitize", "--strip-ansi"]] {
        assert_eq!(
            String::from_utf8_lossy(&filter_with_locale(config.path(), flags, &[], input)),
            "one two\n",
            "{flags:?}"
        );
    }
    // Without --sanitize the wrapper stays transparent.
    assert_eq!(filter_with_locale(config.path(), &[], &[], input), input);
}

// `profiles test` prints a fixture as the stream would. With both flags the
// stream drops the title string whole; the fixture kept the title's text.
#[test]
fn profiles_test_drops_the_title_string_with_both_flags() {
    let mut fixture = tempfile::NamedTempFile::new().expect("fixture");
    fixture
        .write_all(b"one \x1bktitle\x1b\\two\n")
        .expect("write fixture");

    let output = Command::cargo_bin("prismtty")
        .expect("binary exists")
        .args(["--sanitize", "--strip-ansi", "profiles", "test", "generic"])
        .arg(fixture.path())
        .output()
        .expect("command runs");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "one two\n");
}
