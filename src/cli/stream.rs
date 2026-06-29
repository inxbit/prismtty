use std::cmp::Reverse;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::fd::RawFd;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use nix::libc;

use crate::highlight::{
    AnsiChunk, BenchmarkReport, Highlighter, MAX_INCOMPLETE_ESCAPE_BYTES, StreamingHighlighter,
    incomplete_escape_start, strip_ansi,
};
use crate::profile_runtime::ProfileRuntime;
use crate::profiles::ProfileStore;

use super::CliError;
use super::args::Options;
use super::profile_selection::{
    ProfileReporter, auto_detect_enabled, build_highlighter_for_profiles_with_store,
    dynamic_profile_enabled, profile_store, select_profile_names_with_store,
    should_continue_auto_detect,
};
use super::runtime::ReloadWatcher;
use super::trace::IoTrace;

const AUTO_DETECT_SAMPLE_LIMIT: usize = 64 * 1024;

/// Describes the stream's input source for interactive echo handling. It groups
/// the facts the read loop needs to surface buffered echo promptly without
/// disturbing program-output highlighting:
/// - `interactive`: whether this is an interactive terminal session (echo present),
/// - `pty_fd`: the PTY master to poll so an echo burst's buffered trailing token
///   is flushed once the child goes idle (a paste echoes back in one large read),
/// - `recent_input`: the bytes the stdin forwarder recently sent to the child.
///   The loop flushes a buffered trailing token only when it is a byte-for-byte
///   suffix of this (a heuristic for genuine input echo). The match is byte
///   equality, not provenance: in the rare case bulk program output coincides
///   with the user's exact type-ahead a program token may surface, but only the
///   child's own output bytes are ever emitted, never `recent_input`.
pub(super) struct InputSource {
    pub(super) interactive: bool,
    pub(super) pty_fd: Option<RawFd>,
    pub(super) recent_input: Option<Arc<Mutex<Vec<u8>>>>,
}

pub(super) fn highlight_stream<R: Read, W: Write>(
    mut reader: R,
    writer: &mut W,
    options: &Options,
    input: InputSource,
    mut reload_watcher: Option<ReloadWatcher>,
    trace: IoTrace,
    profile_input_rx: Option<mpsc::Receiver<Vec<u8>>>,
) -> Result<(), CliError> {
    let started = Instant::now();
    let mut input_bytes = 0usize;
    let mut buffer = [0_u8; 8192];
    let mut strip_carry: Vec<u8> = Vec::new();
    let read = reader.read(&mut buffer)?;
    if read == 0 {
        return Ok(());
    }

    trace.log("OUT", &buffer[..read]);
    let first_chunk = prepare_chunk(&buffer[..read], options.strip_ansi, &mut strip_carry);
    let mut detection_sample = first_chunk.bytes().to_vec();
    input_bytes += first_chunk.bytes().len();
    let store = profile_store()?;
    let profile_names = select_profile_names_with_store(options, &store, &detection_sample)?;
    let mut session = HighlightSession::new(options, &store, input.interactive, profile_names)?;
    session.report_current();
    let dynamic_profiles =
        dynamic_profile_enabled(options, input.interactive) && profile_input_rx.is_some();
    let mut profile_runtime = if dynamic_profiles {
        Some(ProfileRuntime::new(session.profile_names().to_vec()))
    } else {
        None
    };
    let mut auto_detect_pending =
        !dynamic_profiles && should_continue_auto_detect(options, session.profile_names());
    if let Some(next_profile_names) = observe_dynamic_profile(
        &mut profile_runtime,
        profile_input_rx.as_ref(),
        dynamic_profiles.then_some(&store),
        &first_chunk,
    ) {
        session.switch_profiles(writer, &trace, next_profile_names)?;
    }
    session.push(writer, &trace, &first_chunk)?;
    if let Some(reason) = should_flush_input_echo(
        input.interactive,
        input.pty_fd,
        session.buffered_echo(),
        session.buffered_prompt_echo(),
        input.recent_input.as_deref(),
    ) {
        trace.log("FLUSH", reason.as_bytes());
        session.flush_input_echo(writer, &trace)?;
    }
    writer.flush()?;

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        trace.log("OUT", &buffer[..read]);
        let chunk = prepare_chunk(&buffer[..read], options.strip_ansi, &mut strip_carry);
        input_bytes += chunk.bytes().len();
        if let Some(next_profile_names) = observe_dynamic_profile(
            &mut profile_runtime,
            profile_input_rx.as_ref(),
            dynamic_profiles.then_some(&store),
            &chunk,
        ) {
            session.switch_profiles(writer, &trace, next_profile_names)?;
        }
        if auto_detect_pending && detection_sample.len() < AUTO_DETECT_SAMPLE_LIMIT {
            detection_sample.extend_from_slice(chunk.bytes());
            let next_profile_names =
                select_profile_names_with_store(options, &store, &detection_sample)?;
            if next_profile_names.as_slice() != session.profile_names() {
                session.switch_profiles(writer, &trace, next_profile_names)?;
                auto_detect_pending = should_continue_auto_detect(options, session.profile_names());
            } else if detection_sample.len() >= AUTO_DETECT_SAMPLE_LIMIT {
                auto_detect_pending = false;
                session.report_current();
            }
        }
        if reload_watcher
            .as_mut()
            .is_some_and(ReloadWatcher::reload_requested)
        {
            session.reload(writer, &trace)?;
        }
        session.push(writer, &trace, &chunk)?;
        if let Some(reason) = should_flush_input_echo(
            input.interactive,
            input.pty_fd,
            session.buffered_echo(),
            session.buffered_prompt_echo(),
            input.recent_input.as_deref(),
        ) {
            trace.log("FLUSH", reason.as_bytes());
            session.flush_input_echo(writer, &trace)?;
        }
        writer.flush()?;
    }

    session.finish(writer, &trace)?;
    writer.flush()?;
    session.report_current();

    if options.benchmark {
        print_benchmark_report(
            session.benchmark_report(),
            input_bytes,
            started.elapsed().as_secs_f64(),
        );
    }

    Ok(())
}

/// Bounds the number of distinct compiled-highlighter sets retained so that
/// adversarial profile flapping cannot grow memory without limit.
const HIGHLIGHTER_CACHE_LIMIT: usize = 32;

/// Caches compiled highlighters by profile-name set. Recompiling (and
/// JIT-compiling) every regex on each dynamic profile switch lets untrusted
/// device output force unbounded recompilation; cloning a cached highlighter
/// shares the already-compiled `pcre2` code via `Arc` instead.
#[derive(Default)]
struct HighlighterCache {
    entries: HashMap<Vec<String>, Highlighter>,
}

impl HighlighterCache {
    fn get_or_build<E>(
        &mut self,
        profile_names: &[String],
        build: impl FnOnce() -> Result<Highlighter, E>,
    ) -> Result<Highlighter, E> {
        if let Some(cached) = self.entries.get(profile_names) {
            return Ok(cached.clone());
        }
        let highlighter = build()?;
        if self.entries.len() >= HIGHLIGHTER_CACHE_LIMIT {
            self.entries.clear();
        }
        self.entries
            .insert(profile_names.to_vec(), highlighter.clone());
        Ok(highlighter)
    }
}

struct HighlightSession<'a> {
    options: &'a Options,
    store: &'a ProfileStore,
    interactive: bool,
    profile_names: Vec<String>,
    streaming: StreamingHighlighter,
    reporter: ProfileReporter,
    highlighter_cache: HighlighterCache,
}

impl<'a> HighlightSession<'a> {
    fn new(
        options: &'a Options,
        store: &'a ProfileStore,
        interactive: bool,
        profile_names: Vec<String>,
    ) -> Result<Self, CliError> {
        let streaming = Self::streaming_for(options, store, &profile_names, interactive)?;
        let reporter = ProfileReporter::new(options.show_profile, auto_detect_enabled(options));
        Ok(Self {
            options,
            store,
            interactive,
            profile_names,
            streaming,
            reporter,
            highlighter_cache: HighlighterCache::default(),
        })
    }

    fn profile_names(&self) -> &[String] {
        &self.profile_names
    }

    fn report_current(&mut self) {
        self.reporter.report(&self.profile_names);
    }

    fn switch_profiles<W: Write>(
        &mut self,
        writer: &mut W,
        trace: &IoTrace,
        profile_names: Vec<String>,
    ) -> Result<(), CliError> {
        if profile_names == self.profile_names {
            return Ok(());
        }
        self.rebuild(writer, trace, profile_names, true)
    }

    fn reload<W: Write>(&mut self, writer: &mut W, trace: &IoTrace) -> Result<(), CliError> {
        self.rebuild(writer, trace, self.profile_names.clone(), false)
    }

    fn push<W: Write>(
        &mut self,
        writer: &mut W,
        trace: &IoTrace,
        chunk: &AnsiChunk,
    ) -> Result<(), CliError> {
        write_rendered(writer, trace, self.streaming.push_chunk(chunk))?;
        Ok(())
    }

    fn flush_input_echo<W: Write>(
        &mut self,
        writer: &mut W,
        trace: &IoTrace,
    ) -> Result<(), CliError> {
        write_rendered(writer, trace, self.streaming.flush_buffered_echo())?;
        Ok(())
    }

    /// The buffered trailing token the next echo flush would surface, for the
    /// read loop to match against recent input before deciding to flush it.
    fn buffered_echo(&self) -> &[u8] {
        self.streaming.buffered_echo()
    }

    /// Whether the buffered tail was split off a prompt-echo redraw remainder and
    /// the visible line is a `prompt# command` line, a redraw the read loop
    /// surfaces on idle even without a recent-input byte match. See
    /// `StreamingHighlighter::buffered_echo_completes_prompt_line`.
    fn buffered_prompt_echo(&self) -> bool {
        self.streaming.buffered_echo_completes_prompt_line()
    }

    fn finish<W: Write>(&mut self, writer: &mut W, trace: &IoTrace) -> Result<(), CliError> {
        write_rendered(writer, trace, self.streaming.finish())?;
        Ok(())
    }

    fn benchmark_report(&self) -> Option<&BenchmarkReport> {
        self.streaming.benchmark_report()
    }

    fn rebuild<W: Write>(
        &mut self,
        _writer: &mut W,
        _trace: &IoTrace,
        profile_names: Vec<String>,
        report: bool,
    ) -> Result<(), CliError> {
        self.profile_names = profile_names;
        let options = self.options;
        let store = self.store;
        let interactive = self.interactive;
        let names = self.profile_names.clone();
        let highlighter = self.highlighter_cache.get_or_build(&names, || {
            build_highlighter_for_profiles_with_store(options, store, &names, interactive)
        })?;
        self.streaming.replace_highlighter(highlighter);
        if report {
            self.report_current();
        }
        Ok(())
    }

    fn streaming_for(
        options: &Options,
        store: &ProfileStore,
        profile_names: &[String],
        interactive: bool,
    ) -> Result<StreamingHighlighter, CliError> {
        let highlighter =
            build_highlighter_for_profiles_with_store(options, store, profile_names, interactive)?;
        Ok(new_streaming_highlighter(
            highlighter,
            interactive,
            options.benchmark,
            options.no_minimal_reset,
        ))
    }
}

fn observe_dynamic_profile(
    runtime: &mut Option<ProfileRuntime>,
    profile_input_rx: Option<&mpsc::Receiver<Vec<u8>>>,
    store: Option<&ProfileStore>,
    chunk: &AnsiChunk,
) -> Option<Vec<String>> {
    let runtime = runtime.as_mut()?;
    let store = store?;
    if let Some(receiver) = profile_input_rx {
        while let Ok(input) = receiver.try_recv() {
            runtime.observe_input(&input);
        }
    }
    runtime.observe_output(chunk.visible_bytes(), store)
}

fn write_rendered<W: Write>(writer: &mut W, trace: &IoTrace, rendered: Vec<u8>) -> io::Result<()> {
    trace.log("RENDER", &rendered);
    writer.write_all(&rendered)
}

fn new_streaming_highlighter(
    highlighter: Highlighter,
    interactive: bool,
    benchmark: bool,
    no_minimal_reset: bool,
) -> StreamingHighlighter {
    let mut streaming = if interactive && benchmark {
        StreamingHighlighter::new_interactive_with_benchmark(highlighter)
    } else if interactive {
        StreamingHighlighter::new_interactive(highlighter)
    } else if benchmark {
        StreamingHighlighter::new_with_benchmark(highlighter)
    } else {
        StreamingHighlighter::new(highlighter)
    };
    if no_minimal_reset {
        streaming.set_no_minimal_resets(true);
    }
    streaming
}

fn print_benchmark_report(report: Option<&BenchmarkReport>, input_bytes: usize, elapsed_secs: f64) {
    eprintln!("Benchmark results (time spent, match count):");
    if let Some(report) = report {
        let total = report.total_duration().as_secs_f64();
        let mut rules = report.rules().to_vec();
        rules.sort_by_key(|rule| Reverse(rule.duration));
        for rule in rules {
            let percent = if total > 0.0 {
                rule.duration.as_secs_f64() / total * 100.0
            } else {
                0.0
            };
            eprintln!(
                "{percent:>6.2}% {:>8.3}s  {:<7}  {}",
                rule.duration.as_secs_f64(),
                rule.match_count,
                rule.description
            );
        }
    }
    eprintln!("Processed {input_bytes} bytes in {elapsed_secs:.3}s");
}

fn prepare_chunk(input: &[u8], strip_existing_ansi: bool, strip_carry: &mut Vec<u8>) -> AnsiChunk {
    if strip_existing_ansi {
        // Reassemble any escape that was split across the previous read before
        // stripping, otherwise its tail bytes would survive as literal text.
        let mut combined = std::mem::take(strip_carry);
        combined.extend_from_slice(input);
        let split = incomplete_escape_start(&combined).unwrap_or(combined.len());
        strip_carry.extend_from_slice(&combined[split..]);
        if strip_carry.len() > MAX_INCOMPLETE_ESCAPE_BYTES {
            strip_carry.clear();
        }
        AnsiChunk::new(strip_ansi(&combined[..split]))
    } else {
        AnsiChunk::from_slice(input)
    }
}

/// Decides whether to surface the buffered interactive input echo now.
///
/// prismtty speculatively buffers a trailing partial token so a token split
/// across reads still highlights as one unit. For interactive echo that
/// buffering must not strand the last token: a keystroke or pasted line echoes
/// back and then the child goes idle, so the token would otherwise stay hidden
/// until the next byte. Two independent triggers surface it; the flush only ever
/// emits the child's own buffered output bytes, never `recent_input`:
///
/// 1. Echo-suffix: the buffered token is a byte-for-byte suffix of recently
///    forwarded input ([`consume_echo_suffix`], after ignoring a trailing
///    completion-trigger byte the device consumed), gated on the lenient
///    [`input_source_idle`] (a poll error counts as idle so raw-mode/ssh echo
///    still surfaces). Consumes the matched `recent_input`.
/// 2. Prompt-echo: the buffered token was split off a prompt-echo redraw
///    remainder (`buffered_prompt_echo`), a device redrawing `prompt# command`
///    after a Tab/`?` completion, where a device-completed word breaks the byte
///    match. Gated on a strict clean idle ([`input_source_idle_strict`]; a poll
///    error is NOT idle here, since a continuation may still arrive) and does
///    NOT touch `recent_input`, so typed-ahead input is preserved.
///
/// Screen safety does not rely on either heuristic: the session only ever emits
/// the child's own output bytes, never `recent_input`, so a non-echoed secret is
/// never drawn (it produces no buffered token).
fn should_flush_input_echo(
    interactive: bool,
    pty_fd: Option<RawFd>,
    buffered_echo: &[u8],
    buffered_prompt_echo: bool,
    recent_input: Option<&Mutex<Vec<u8>>>,
) -> Option<&'static str> {
    if !interactive || buffered_echo.is_empty() {
        return None;
    }
    if let Some(recent_input) = recent_input {
        if input_source_idle(pty_fd) {
            let mut recent_input = recent_input
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if consume_echo_suffix(&mut recent_input, buffered_echo) {
                return Some("echo-suffix");
            }
        }
    }
    if buffered_prompt_echo && input_source_idle_strict(pty_fd) {
        return Some("prompt-echo");
    }
    None
}

/// Bytes a device consumes from the command line as a completion/help trigger
/// without echoing them back (Tab) or without re-emitting them in the redraw
/// (`?`). A buffered echo token is built only from token-continuation bytes
/// (see `is_token_continuation` in `highlight.rs`), so it never ends in one of
/// these; ignoring a trailing run of them on `recent_input` therefore cannot
/// drop a byte a genuine echo match needs.
fn is_completion_trigger_byte(byte: u8) -> bool {
    matches!(byte, b'\t' | b'?')
}

/// If `recent_input` ends with `echo` after ignoring any trailing
/// completion-trigger bytes the device consumed without echoing, the buffered
/// token is genuine input echo: consume `recent_input` (so it cannot be matched
/// again by a later program token) and return true. Otherwise the token is
/// program output: leave `recent_input` untouched and return false.
fn consume_echo_suffix(recent_input: &mut Vec<u8>, echo: &[u8]) -> bool {
    let match_end = recent_input
        .iter()
        .rposition(|byte| !is_completion_trigger_byte(*byte))
        .map_or(0, |idx| idx + 1);
    if recent_input[..match_end].ends_with(echo) {
        recent_input.clear();
        true
    } else {
        false
    }
}

/// Reports whether the PTY master has no output readable at this instant, the
/// cue that an interactive echo burst has drained, so buffered echo can surface
/// without waiting for the next byte. A failed/interrupted poll is treated as
/// idle so echo still surfaces promptly; the recent-input suffix match in
/// [`should_flush_input_echo`] keeps that from disturbing program-output
/// buffering. Returns false when no descriptor is available (stdin mode).
fn input_source_idle(pty_fd: Option<RawFd>) -> bool {
    poll_idle(pty_fd, true)
}

/// Like [`input_source_idle`] but treats only a clean `poll == 0` as idle: a
/// poll error is NOT idle. Used by the prompt-echo flush trigger, where flushing
/// on a spurious idle could split a redraw whose continuation has not arrived.
fn input_source_idle_strict(pty_fd: Option<RawFd>) -> bool {
    poll_idle(pty_fd, false)
}

/// Nonblocking `poll(POLLIN)` on the PTY master. `error_is_idle` selects the
/// policy for a failed/interrupted poll (`ready < 0`): lenient callers treat it
/// as idle (echo still surfaces), strict callers do not (a continuation may be
/// arriving). Returns false when no descriptor is available (stdin mode).
fn poll_idle(pty_fd: Option<RawFd>, error_is_idle: bool) -> bool {
    let Some(fd) = pty_fd else {
        return false;
    };
    let mut poll_fd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `poll_fd` is one valid, initialized entry; `poll` only reads
    // `fd`/`events` and writes `revents`. The zero timeout makes it nonblocking.
    let ready = unsafe { libc::poll(&mut poll_fd, 1, 0) };
    if error_is_idle {
        ready <= 0
    } else {
        ready == 0
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    fn sample_highlighter() -> crate::highlight::Highlighter {
        let config =
            crate::config::PrismConfig::from_chromaterm_yaml("rules: []\n").expect("config loads");
        crate::highlight::Highlighter::from_config(config).expect("highlighter compiles")
    }

    // Switching back to a previously-seen profile set must reuse the cached
    // compiled highlighter instead of recompiling, so untrusted output that
    // flaps the detected profile cannot force unbounded recompilation.
    #[test]
    fn highlighter_cache_reuses_compiled_highlighters_for_seen_profile_sets() {
        let mut cache = super::HighlighterCache::default();
        let set_a = vec!["generic".to_string(), "cisco".to_string()];
        let set_b = vec!["generic".to_string(), "juniper".to_string()];
        let mut builds = 0usize;

        cache
            .get_or_build(&set_a, || {
                builds += 1;
                Ok::<_, ()>(sample_highlighter())
            })
            .unwrap();
        cache
            .get_or_build(&set_b, || {
                builds += 1;
                Ok::<_, ()>(sample_highlighter())
            })
            .unwrap();
        cache
            .get_or_build(&set_a, || {
                builds += 1;
                Ok::<_, ()>(sample_highlighter())
            })
            .unwrap();

        assert_eq!(
            builds, 2,
            "switching back to a seen profile set must not recompile"
        );
    }

    // In --strip-ansi mode an escape split across reads must be reassembled
    // before stripping, so its parameter/final bytes do not leak into the
    // visible output as literal text.
    #[test]
    fn strip_mode_carries_split_escape_across_reads() {
        let mut carry = Vec::new();
        let first = super::prepare_chunk(b"hello\x1b[3", true, &mut carry);
        let second = super::prepare_chunk(b"1m world", true, &mut carry);
        let mut visible = first.bytes().to_vec();
        visible.extend_from_slice(second.bytes());
        assert_eq!(
            visible,
            b"hello world",
            "split escape tail leaked into stripped output: {:?}",
            String::from_utf8_lossy(&visible)
        );
    }

    #[test]
    fn strip_mode_drops_oversized_incomplete_escape_carry() {
        let mut carry = Vec::new();
        let mut input = b"\x1b[".to_vec();
        input.extend(std::iter::repeat_n(
            b'1',
            crate::highlight::MAX_INCOMPLETE_ESCAPE_BYTES + 1,
        ));

        let chunk = super::prepare_chunk(&input, true, &mut carry);

        assert!(
            chunk.bytes().is_empty(),
            "oversized incomplete escape should be stripped as control data"
        );
        assert!(
            carry.is_empty(),
            "oversized incomplete escape carry must not grow without bound"
        );
    }

    #[test]
    fn dynamic_profile_observation_reuses_prepared_visible_chunk() {
        let source = include_str!("stream.rs");
        let runtime_source = source.split("#[cfg(test)]").next().unwrap_or(source);

        assert!(!runtime_source.contains("let visible_chunk = strip_ansi(chunk)"));
        assert!(runtime_source.contains("visible_bytes()"));
    }

    #[test]
    fn consume_echo_suffix_matches_only_genuine_echo() {
        // A buffered token that is a suffix of recent input is genuine echo:
        // matched and cleared so retained input cannot linger past its use.
        let mut recent = b"update add test.example.com 3600 A 192.0.2.1".to_vec();
        assert!(super::consume_echo_suffix(&mut recent, b"192.0.2.1"));
        assert!(
            recent.is_empty(),
            "matched input is cleared after the echo tail is surfaced"
        );
        // The same program-output bytes no longer match once consumed.
        assert!(!super::consume_echo_suffix(&mut recent, b"192.0.2.1"));

        // A program-output token that was never typed does not match (this is the
        // echo-off regression guard): recent input is the keystroke, not "Vlan11".
        let mut typed = b"x".to_vec();
        assert!(!super::consume_echo_suffix(&mut typed, b"Vlan11"));
        assert_eq!(typed, b"x", "non-matching input is left untouched");

        // Empty recent input matches nothing.
        let mut empty = Vec::new();
        assert!(!super::consume_echo_suffix(&mut empty, b"Vlan11"));
    }

    #[test]
    fn consume_echo_suffix_ignores_trailing_completion_trigger() {
        // A device consumes Tab/`?` from the command line without echoing it, so
        // the buffered redraw tail still matches the typed command once the
        // trailing trigger byte is ignored.
        let mut tab = b"sh lo\t".to_vec();
        assert!(super::consume_echo_suffix(&mut tab, b"lo"));
        assert!(tab.is_empty(), "matched input is cleared");

        let mut question = b"sh lo?".to_vec();
        assert!(super::consume_echo_suffix(&mut question, b"lo"));

        // A program-output token still does not match non-trigger type-ahead.
        let mut typed = b"x".to_vec();
        assert!(!super::consume_echo_suffix(&mut typed, b"Vlan11"));
        assert_eq!(typed, b"x");

        // recent_input of only trigger bytes collapses match_end to 0: no false
        // match, no panic.
        let mut all_triggers = b"\t\t?".to_vec();
        assert!(!super::consume_echo_suffix(&mut all_triggers, b"lo"));
        assert_eq!(all_triggers, b"\t\t?");
    }

    #[test]
    fn should_flush_input_echo_requires_interactive_buffered_and_recent_input() {
        // Not interactive: the interactive guard short-circuits, so a
        // non-interactive stream never flushes.
        assert!(super::should_flush_input_echo(false, None, b"tok", false, None).is_none());
        // Interactive but nothing buffered: never flushes.
        let recent = Mutex::new(b"tok".to_vec());
        assert!(super::should_flush_input_echo(true, None, b"", false, Some(&recent)).is_none());
        // Interactive with buffered echo but no recent_input source: never flushes.
        assert!(super::should_flush_input_echo(true, None, b"tok", false, None).is_none());
    }

    #[test]
    fn should_flush_input_echo_flushes_only_matching_idle_echo() {
        use nix::pty::openpty;
        use std::os::fd::AsRawFd;

        let pty = openpty(None, None).expect("openpty");
        let master_fd = pty.master.as_raw_fd();
        let slave_fd = pty.slave.as_raw_fd();

        // Buffered token is a suffix of recent input + idle: flush and clear it.
        // ECHO state is irrelevant now; the suffix match is the sole
        // discriminator, which is what lets raw-mode/ssh echo (ECHO off) surface.
        let recent = Mutex::new(b"router# show ".to_vec());
        assert_eq!(
            super::should_flush_input_echo(true, Some(master_fd), b"show ", false, Some(&recent),),
            Some("echo-suffix"),
        );
        assert!(
            recent.lock().unwrap().is_empty(),
            "matched input is cleared after the echo tail is surfaced"
        );

        // Token is not recent input (program output): do not flush, leave it.
        let recent = Mutex::new(b"x".to_vec());
        assert!(
            super::should_flush_input_echo(true, Some(master_fd), b"Vlan11", false, Some(&recent),)
                .is_none()
        );
        assert_eq!(recent.lock().unwrap().as_slice(), b"x");

        // Pending data == not idle: wait, do not consume.
        assert_eq!(
            unsafe { nix::libc::write(slave_fd, b"x".as_ptr().cast(), 1) },
            1
        );
        let recent = Mutex::new(b"show ".to_vec());
        assert!(
            super::should_flush_input_echo(true, Some(master_fd), b"show ", false, Some(&recent),)
                .is_none()
        );
        assert_eq!(recent.lock().unwrap().as_slice(), b"show ");
    }

    #[test]
    fn should_flush_input_echo_flushes_buffered_prompt_echo_without_byte_match() {
        use nix::pty::openpty;
        use std::os::fd::AsRawFd;

        let pty = openpty(None, None).expect("openpty");
        let master_fd = pty.master.as_raw_fd();
        let slave_fd = pty.slave.as_raw_fd();

        // The device redrew a `prompt# command` line and went idle. The buffered
        // tail is the child's own prompt echo even though the consumed completion
        // trigger (and a device-completed word) broke the byte match. It flushes
        // on strict idle and must NOT drop typed-ahead input.
        let recent = Mutex::new(b"sh lo\t".to_vec());
        assert_eq!(
            super::should_flush_input_echo(true, Some(master_fd), b"log", true, Some(&recent),),
            Some("prompt-echo"),
        );
        assert_eq!(
            recent.lock().unwrap().as_slice(),
            b"sh lo\t",
            "prompt-echo flush must not drop typed-ahead input"
        );

        // Without the prompt-echo flag and without a byte match, nothing flushes.
        let recent = Mutex::new(b"sh lo\t".to_vec());
        assert!(
            super::should_flush_input_echo(true, Some(master_fd), b"zzz", false, Some(&recent),)
                .is_none()
        );

        // Strict idle: pending output means the prompt-echo tail is not flushed
        // (a continuation may still arrive), unlike the lenient echo-suffix path.
        assert_eq!(
            unsafe { nix::libc::write(slave_fd, b"x".as_ptr().cast(), 1) },
            1
        );
        let recent = Mutex::new(b"sh lo\t".to_vec());
        assert!(
            super::should_flush_input_echo(true, Some(master_fd), b"log", true, Some(&recent),)
                .is_none()
        );
    }

    // End-to-end read-loop decision: the highlighter buffers the Cisco Tab redraw
    // tail, the real `should_flush_input_echo` gate decides to flush it on idle
    // via the prompt-echo trigger (the typed "sh lo\t" is not a byte suffix of the
    // device-completed "log"), and the flush surfaces exactly the tail. A local
    // PTY cannot reproduce the live bug (it splits the redraw at the LF, which
    // emits the tail), so the single-chunk delivery is driven through `push_str`.
    #[test]
    fn read_loop_surfaces_cisco_tab_redraw_tail_on_idle() {
        use nix::pty::openpty;
        use std::os::fd::AsRawFd;

        let pty = openpty(None, None).expect("openpty");
        let master_fd = pty.master.as_raw_fd();

        let highlighter =
            crate::highlight::Highlighter::from_config(crate::config::PrismConfig::default())
                .expect("empty config compiles");
        let mut streaming = crate::highlight::StreamingHighlighter::new_interactive(highlighter);
        let _ = streaming.push_str("LAB-N9K-CORE-01# ");
        let _ = streaming.push_str("sh lo");
        let redraw = "\x1b[23D\x1b[J\rLAB-N9K-CORE-01# sh log\r\r\nlogging   login     \r\r\n\x1b[J\rLAB-N9K-CORE-01# sh log";
        let _ = streaming.push_str(redraw);

        let recent = Mutex::new(b"sh lo\t".to_vec());
        assert_eq!(
            super::should_flush_input_echo(
                true,
                Some(master_fd),
                streaming.buffered_echo(),
                streaming.buffered_echo_completes_prompt_line(),
                Some(&recent),
            ),
            Some("prompt-echo"),
            "idle prompt-echo redraw tail must be flushed by the read-loop gate"
        );
        // The prompt-echo path must not consume typed-ahead input.
        assert_eq!(recent.lock().unwrap().as_slice(), b"sh lo\t");
        // The flush surfaces exactly the buffered tail.
        assert_eq!(
            crate::highlight::strip_ansi(&streaming.flush_buffered_echo()),
            b"log"
        );
    }

    // End-to-end wiring through `highlight_stream`: the read loop must actually
    // pass `session.buffered_prompt_echo()` to the gate and invoke the flush, so
    // a device-completed Tab redraw surfaces on idle. The reader delivers the
    // redraw as a single chunk (a real PTY splits it at the LF; see the comment
    // on `read_loop_surfaces_cisco_tab_redraw_tail_on_idle`); the idle PTY master
    // makes `input_source_idle_strict` true.
    #[test]
    fn highlight_stream_wires_prompt_echo_flush_end_to_end() {
        use nix::pty::openpty;
        use std::collections::VecDeque;
        use std::io::Read;
        use std::os::fd::AsRawFd;

        struct ChunkedReader(VecDeque<Vec<u8>>);
        impl Read for ChunkedReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                match self.0.pop_front() {
                    Some(c) => {
                        let n = c.len().min(buf.len());
                        buf[..n].copy_from_slice(&c[..n]);
                        Ok(n)
                    }
                    None => Ok(0),
                }
            }
        }

        let pty = openpty(None, None).expect("openpty");
        let master_fd = pty.master.as_raw_fd();

        let reader = ChunkedReader(VecDeque::from(vec![
            b"LAB-N9K-CORE-01# ".to_vec(),
            b"sh lo".to_vec(),
            b"\x1b[23D\x1b[J\rLAB-N9K-CORE-01# sh log\r\r\nlogging   login     \r\r\n\x1b[J\rLAB-N9K-CORE-01# sh log".to_vec(),
        ]));
        let mut writer: Vec<u8> = Vec::new();
        let options = crate::cli::args::Options {
            profiles: vec!["cisco".to_string()],
            no_auto_detect: true,
            no_dynamic_profile: true,
            ..Default::default()
        };
        let recent = std::sync::Arc::new(Mutex::new(b"sh lo\t".to_vec()));
        let input = super::InputSource {
            interactive: true,
            pty_fd: Some(master_fd),
            recent_input: Some(recent.clone()),
        };
        // Real trace so we can assert the prompt-echo flush fired DURING the read
        // loop (a FLUSH marker), distinguishing it from finish() emitting the
        // buffered tail at EOF, which happens regardless and would make a
        // writer-content assertion a tautology.
        let trace_file = tempfile::NamedTempFile::new().expect("trace temp file");
        let trace_path = trace_file.path().to_path_buf();
        let trace = super::IoTrace::open(Some(trace_path.as_path())).expect("trace opens");

        super::highlight_stream(reader, &mut writer, &options, input, None, trace, None)
            .expect("highlight_stream succeeds");

        let trace_text = std::fs::read_to_string(&trace_path).expect("read trace");

        // A FLUSH record whose hex payload decodes to "prompt-echo" proves the
        // read loop passed `session.buffered_prompt_echo()` into the gate and
        // invoked the flush mid-stream (the wiring under test). finish() never
        // logs FLUSH, so this cannot pass via the EOF tail emission.
        let saw_prompt_echo_flush = trace_text.lines().any(|line| {
            let mut fields = line.split_whitespace();
            let _ts = fields.next();
            if fields.next() != Some("FLUSH") {
                return false;
            }
            let bytes: Vec<u8> = fields
                .filter_map(|h| u8::from_str_radix(h, 16).ok())
                .collect();
            bytes == b"prompt-echo"
        });
        assert!(
            saw_prompt_echo_flush,
            "no FLUSH prompt-echo marker; trace:\n{trace_text}"
        );
        // The prompt-echo path must not consume typed-ahead input.
        assert_eq!(recent.lock().unwrap().as_slice(), b"sh lo\t");
    }
}
