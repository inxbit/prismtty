//! Integration coverage for prismtty's PTY signal handling (AUDIT L14).
//!
//! These tests run the `ptty` binary *inside* a PTY (so it treats its stdin as
//! a terminal and installs its signal handlers), then deliver a real signal to
//! it and assert the wrapped child observes the forwarded signal rather than
//! being orphaned.
#![cfg(unix)]

use std::io::Read;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// AUDIT L14: a signal delivered to prismtty is forwarded to the wrapped
/// child's process group instead of orphaning it.
#[test]
fn external_signal_is_forwarded_to_wrapped_child() {
    let marker = tempfile::NamedTempFile::new().expect("marker file");
    let marker_path = marker.path().to_path_buf();
    // Start absent so its later presence proves the child's TERM trap ran.
    std::fs::remove_file(&marker_path).ok();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.env("PRISMTTY_SIGNAL_MARKER", marker_path.as_os_str());
    builder.arg("sh");
    builder.arg("-c");
    // Ignore SIGHUP (which PTY teardown delivers when prismtty exits) so that
    // writing the marker can only be the result of a *forwarded* SIGTERM.
    builder.arg(
        "trap '' HUP; trap 'echo forwarded > \"$PRISMTTY_SIGNAL_MARKER\" ; exit 0' TERM; echo READY; while true; do sleep 0.2; done",
    );

    let mut child = pair.slave.spawn_command(builder).expect("spawn ptty");
    drop(pair.slave);
    let ptty_pid = child.process_id().expect("ptty pid") as libc::pid_t;

    // Wait until the wrapped shell has installed its traps and printed READY
    // (relayed through prismtty to the outer master).
    let mut reader = pair.master.try_clone_reader().expect("clone reader");
    let (ready_tx, ready_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut acc = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    acc.extend_from_slice(&buf[..n]);
                    if acc.windows(5).any(|w| w == b"READY") {
                        let _ = ready_tx.send(());
                        break;
                    }
                }
            }
        }
    });
    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("wrapped child reached READY");

    // Deliver SIGTERM to prismtty; it must forward to the wrapped child group.
    assert_eq!(unsafe { libc::kill(ptty_pid, libc::SIGTERM) }, 0);

    // The child's TERM trap writes the marker; poll for it.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut forwarded = false;
    while Instant::now() < deadline {
        if std::fs::read_to_string(&marker_path)
            .map(|c| c.contains("forwarded"))
            .unwrap_or(false)
        {
            forwarded = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    let _ = child.wait();
    assert!(
        forwarded,
        "wrapped child never received the forwarded SIGTERM (it was orphaned)"
    );
}
