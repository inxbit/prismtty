//! End to end: leaving a nested device in a wrapped interactive session
//! returns to the previous device's colors. A scripted shell stands in for
//! the devices; it reads typed lines and prints what each device would.
#![cfg(unix)]

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const WAIT_LIMIT: Duration = Duration::from_secs(10);
const REPORT: &str = "prismtty: profiles selected: ";

/// `ptty --show-profile` wrapping `/bin/sh -c script` on a terminal of its
/// own, with a scratch home and an empty config.
struct Session {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    _home: tempfile::TempDir,
}

impl Session {
    fn start(script: &str) -> Self {
        let home = tempfile::tempdir().expect("home");
        let config = home.path().join("empty.yml");
        std::fs::write(&config, "rules: []\n").expect("config writes");
        let pair = native_pty_system()
            .openpty(PtySize::default())
            .expect("openpty");
        let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
        builder.env_clear();
        builder.env("HOME", home.path());
        builder.env("XDG_CONFIG_HOME", home.path().join("xdg"));
        builder.env("PRISMTTY_RUNTIME_DIR", home.path().join("runtime"));
        builder.env("PATH", "/usr/bin:/bin");
        builder.env("TERM", "xterm");
        builder.args(["--show-profile", "--config"]);
        builder.arg(&config);
        builder.args(["--", "/bin/sh", "-c", script]);
        let child = pair.slave.spawn_command(builder).expect("spawn ptty");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("reader");
        let writer = pair.master.take_writer().expect("writer");
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        thread::spawn(move || {
            let _master = pair.master;
            let mut buf = [0_u8; 4096];
            while let Ok(read) = reader.read(&mut buf) {
                if read == 0 {
                    break;
                }
                sink.lock().expect("output").extend_from_slice(&buf[..read]);
            }
        });
        Self {
            child,
            writer,
            output,
            _home: home,
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().expect("output")).into_owned()
    }

    /// Waits until `needle` has appeared `count` times in the output.
    fn wait_for(&self, needle: &str, count: usize) {
        let deadline = Instant::now() + WAIT_LIMIT;
        while self.text().matches(needle).count() < count {
            assert!(
                Instant::now() < deadline,
                "waiting for {needle:?} x{count}; output: {:?}",
                self.text()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Types `keys` a byte at a time, as a person does.
    fn type_keys(&mut self, keys: &[u8]) {
        for byte in keys {
            self.writer.write_all(&[*byte]).expect("type");
            self.writer.flush().expect("flush");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The profile reported last before `marker` first appears.
    fn profile_before(&self, marker: &str) -> String {
        let text = self.text();
        let end = text.find(marker).expect("marker in output");
        text[..end]
            .rmatch_indices(REPORT)
            .next()
            .map(|(at, _)| {
                text[at + REPORT.len()..]
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            })
            .expect("a report before the marker")
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn exit_from_a_nested_ssh_device_returns_to_the_parent_device_colors() {
    let mut session = Session::start(
        r#"printf 'user@laptop:~$ '
while IFS= read -r line; do
  case "$line" in
    'ssh sw1') printf 'Cisco IOS Software\nsw1#' ;;
    'ssh sw2') printf -- '--- JUNOS 22.4R3 Kernel 64-bit\nlab@sw2> ' ;;
    exit) printf '[Connection to sw2 closed by foreign host]\nsw1#' ;;
    'show clock') printf 'CLOCK 12:00 UTC\nsw1#' ;;
    quit) exit 0 ;;
  esac
done"#,
    );
    session.wait_for("user@laptop:~$ ", 1);
    session.type_keys(b"ssh sw1\r");
    session.wait_for("sw1#", 1);
    session.type_keys(b"ssh sw2\r");
    session.wait_for("lab@sw2> ", 1);
    assert_eq!(session.profile_before("lab@sw2> "), "generic, juniper");
    session.type_keys(b"exit\r");
    session.wait_for("sw1#", 2);
    session.type_keys(b"show clock\r");
    session.wait_for("CLOCK 12:00 UTC", 1);
    assert_eq!(
        session.profile_before("CLOCK 12:00 UTC"),
        "generic, cisco",
        "{:?}",
        session.text()
    );
    session.type_keys(b"quit\r");
}

#[test]
fn ctrl_bracket_quit_from_telnet_returns_to_the_local_colors() {
    let mut session = Session::start(
        r#"printf 'user@laptop:~$ '
quit="$(printf '\035')q"
while IFS= read -r line; do
  case "$line" in
    'telnet r1') printf "Trying 192.0.2.1...\nConnected to r1.\nEscape character is '^]'.\n--- JUNOS 22.4R3 Kernel 64-bit\nlab@r1> " ;;
    "$quit") printf '\ntelnet> q\nConnection closed.\nuser@laptop:~$ ' ;;
    pwd) printf 'PWD /home/user\nuser@laptop:~$ ' ;;
    quit) exit 0 ;;
  esac
done"#,
    );
    session.wait_for("user@laptop:~$ ", 1);
    session.type_keys(b"telnet r1\r");
    session.wait_for("lab@r1> ", 1);
    assert_eq!(session.profile_before("lab@r1> "), "generic, juniper");
    session.type_keys(b"\x1dq\r");
    session.wait_for("user@laptop:~$ ", 2);
    session.type_keys(b"pwd\r");
    session.wait_for("PWD /home/user", 1);
    assert_eq!(
        session.profile_before("PWD /home/user"),
        "generic, linux-unix",
        "{:?}",
        session.text()
    );
    session.type_keys(b"quit\r");
}
