//! Integration coverage for interactive input echo (the pasted-command bug).
//!
//! prismtty buffers a trailing partial token of interactive echo so a token
//! split across reads still highlights as a unit. A pasted line echoes back in
//! a single large read, so the buffered trailing token used to stay invisible
//! until the next keystroke surfaced it (reported against `nsupdate`, whose bare
//! `> ` prompt is not recognized, so echo is not passed through). prismtty must
//! instead surface it once the child goes idle.
#![cfg(unix)]

use std::io::{Read, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use prismtty::highlight::strip_ansi;

/// A pasted command (no trailing newline) must become fully visible without any
/// further input. `cat` keeps the wrapped PTY in canonical echo mode, so the
/// tty line discipline echoes the paste exactly as `nsupdate`'s prompt would.
#[test]
fn pasted_line_is_fully_visible_without_extra_input() {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.arg("sh");
    builder.arg("-c");
    builder.arg("printf 'READY\\n'; exec cat");

    let mut child = pair.slave.spawn_command(builder).expect("spawn ptty cat");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("clone reader");
    let mut writer = pair.master.take_writer().expect("take writer");

    // Stream echoed output from a thread so a blocking read on the buggy path
    // (token never flushed) cannot hang the test; the main thread bounds the
    // wait. Each read publishes the current visible (ANSI-stripped) text.
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let mut acc = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    acc.extend_from_slice(&buf[..n]);
                    let visible = String::from_utf8_lossy(&strip_ansi(&acc)).into_owned();
                    if tx.send(visible).is_err() {
                        break;
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    // Wait until prismtty is forwarding child output before sending the paste.
    let ready_deadline = Instant::now() + Duration::from_secs(5);
    let mut visible = String::new();
    while Instant::now() < ready_deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(latest) => {
                visible = latest;
                if visible.contains("READY") {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    assert!(
        visible.contains("READY"),
        "wrapped command was not ready before paste; saw: {visible:?}"
    );

    // A multi-word line whose final, delimiter-less token ("192.0.2.1") is the
    // piece prismtty buffers. No trailing newline: the child stays at the line,
    // exactly like a paste awaiting Enter.
    let paste = b"update add test.example.com 3600 A 192.0.2.1";
    writer.write_all(paste).expect("write paste");
    writer.flush().expect("flush paste");

    // Wait for the full line to surface. Crucially we send NO further bytes, so
    // the trailing token can only appear via prismtty's idle flush.
    let target = "update add test.example.com 3600 A 192.0.2.1";
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(latest) => {
                visible = latest;
                if visible.contains(target) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        visible.contains(target),
        "pasted line never fully surfaced without extra input; saw: {visible:?}"
    );
}

fn count_subslice(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

fn contains_sgr_span(haystack: &[u8], token: &[u8]) -> bool {
    let mut rest = haystack;
    while let Some(esc_idx) = rest.iter().position(|byte| *byte == 0x1b) {
        let candidate = &rest[esc_idx..];
        let Some(m_idx) = candidate.iter().position(|byte| *byte == b'm') else {
            return false;
        };
        if candidate[m_idx + 1..].starts_with(token) {
            return true;
        }
        rest = &candidate[1..];
    }
    false
}

/// The mirror invariant: a token split across reads in pure PROGRAM output (no
/// input echo) must keep its cross-read highlighting. The idle flush surfaces
/// input echo, so it must NOT fire for buffered program-output tokens — there is
/// no pending input echo. Regression guard for the bulk-output highlighting that
/// an unconditional idle flush would break (cisco "Vlan1191" split as
/// "...Vlan11" + "91" across two reads with an inter-write gap).
#[test]
fn split_program_output_token_keeps_single_highlight_span() {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.arg("-p");
    builder.arg("cisco");
    builder.arg("sh");
    builder.arg("-c");
    // First write is >8 bytes and ends mid-token; the gap lets prismtty read it
    // (and go idle) before the rest arrives, so the token genuinely spans reads.
    builder
        .arg("printf 'show: Vlan11'; sleep 0.25; printf '91 New TZ GW to Internal\\n'; sleep 0.3");

    let mut child = pair.slave.spawn_command(builder).expect("spawn ptty");
    drop(pair.slave);

    // Pure program output: we never write to the master, so no input echo is
    // pending and the idle flush must leave the buffered token alone.
    let mut reader = pair.master.try_clone_reader().expect("clone reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut acc = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    acc.extend_from_slice(&buf[..n]);
                    if tx.send(acc.clone()).is_err() {
                        break;
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut out = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(latest) => {
                out = latest;
                if contains_sgr_span(&out, b"Vlan1191") {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        contains_sgr_span(&out, b"Vlan1191"),
        "split program-output token lost its single highlight span; saw: {:?}",
        String::from_utf8_lossy(&out)
    );
}

/// Runs an echo-off child that streams the cisco token "Vlan1191" split across
/// two writes per iteration, while a thread types `typed` bytes throughout, and
/// returns the captured output. With echo off the typed bytes never echo back,
/// so the buffered token is always program output and its span must stay intact
/// regardless of what is typed.
fn echo_off_split_stream_while_typing(typed: &'static [u8]) -> Vec<u8> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut builder = CommandBuilder::new(env!("CARGO_BIN_EXE_ptty"));
    builder.arg("-p");
    builder.arg("cisco");
    builder.arg("sh");
    builder.arg("-c");
    builder.arg(
        "stty -echo; i=0; while [ $i -lt 12 ]; do printf 'aaaaaaaa Vlan11'; \
         sleep 0.12; printf '91 bbbb\\n'; sleep 0.12; i=$((i+1)); done",
    );

    let mut child = pair.slave.spawn_command(builder).expect("spawn ptty");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("clone reader");
    let mut writer = pair.master.take_writer().expect("take writer");

    // Type throughout, then stop explicitly. Linux PTYs can keep accepting
    // master writes briefly after the child exits, so do not rely on write
    // failure as the only thread-exit signal.
    let stop_typing = Arc::new(AtomicBool::new(false));
    let typer_stop = Arc::clone(&stop_typing);
    let typer = thread::spawn(move || {
        while !typer_stop.load(Ordering::Relaxed) {
            if writer.write_all(typed).is_err() || writer.flush().is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(30));
        }
    });

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut acc = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    acc.extend_from_slice(&buf[..n]);
                    if tx.send(acc.clone()).is_err() {
                        break;
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut out = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(latest) => out = latest,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    stop_typing.store(true, Ordering::Relaxed);
    let _ = typer.join();
    out
}

fn assert_spans_intact(out: &[u8]) {
    let broken = count_subslice(out, b"mVlan11\x1b[39m91");
    assert_eq!(
        broken,
        0,
        "concurrent input split {broken} program-output token span(s); saw: {:?}",
        String::from_utf8_lossy(out)
    );
    assert!(
        contains_sgr_span(out, b"Vlan1191"),
        "expected at least one intact Vlan1191 span; saw: {:?}",
        String::from_utf8_lossy(out)
    );
}

/// Non-matching type-ahead must not split a streamed program token. A coarse
/// "input happened" signal would wrongly flush the buffered token; the suffix
/// match leaves it buffered because "x" is not the token.
#[test]
fn split_program_output_token_survives_concurrent_nonechoing_input() {
    assert_spans_intact(&echo_off_split_stream_while_typing(b"x"));
}

/// The adversarial case the suffix match alone cannot catch: with echo off the
/// user types the EXACT bytes of the streamed token. Content matching would see
/// recent input ending in those bytes and flush the *program* token, splitting
/// it. The ECHO-state gate closes this: with echo off there is no input echo at
/// all, so nothing is surfaced and the span stays intact.
#[test]
fn split_program_output_token_survives_concurrent_input_matching_the_token() {
    assert_spans_intact(&echo_off_split_stream_while_typing(b"Vlan11"));
}
