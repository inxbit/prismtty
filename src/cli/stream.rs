use std::cmp::Reverse;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::os::fd::RawFd;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::libc;

use crate::highlight::{
    AnsiChunk, BenchmarkReport, Highlighter, MAX_INCOMPLETE_ESCAPE_BYTES,
    OversizedStringControlFilter, RuleIdentityRegistry, StreamingHighlighter,
    incomplete_sanitize_start, is_incomplete_utf8_prefix, strip_ansi, strip_string_escapes,
};
use crate::profile_runtime::ProfileRuntime;
use crate::profiles::ProfileStore;
use crate::terminal_text::escape_untrusted;

use super::CliError;
use super::args::Options;
use super::profile_selection::{
    ProfileReporter, auto_detect_enabled, build_highlighter_for_profiles_with_store,
    dynamic_profile_enabled, load_overlay_config, profile_store, select_profile_names_with_store,
    should_continue_auto_detect,
};
use super::runtime::ReloadWatcher;
use super::trace::IoTrace;

const AUTO_DETECT_SAMPLE_LIMIT: usize = 64 * 1024;

#[derive(Default)]
struct ChunkFilterState {
    carry: Vec<u8>,
    oversized_string_controls: OversizedStringControlFilter,
}

/// Cumulative per-rule matching time after which the session warns once that a
/// rule is pathologically slow. Healthy rules stay orders of magnitude below
/// this; an imported config with a catastrophic pattern blows past it within
/// the first few megabytes and would otherwise read as a silent hang.
const SLOW_RULE_WARN_THRESHOLD: Duration = Duration::from_secs(5);

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
    let mut chunk_filter = ChunkFilterState::default();
    let read = reader.read(&mut buffer)?;
    if read == 0 {
        return Ok(());
    }

    trace.log("OUT", &buffer[..read]);
    let first_chunk = prepare_chunk(
        &buffer[..read],
        options.strip_ansi,
        options.sanitize,
        &mut chunk_filter,
    );
    let mut detection_sample = first_chunk.bytes().to_vec();
    input_bytes += read;
    let store = profile_store()?;
    let profile_names = select_profile_names_with_store(options, &store, &detection_sample)?;
    let mut session = HighlightSession::new(options, store, input.interactive, profile_names)?;
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
        dynamic_profiles.then_some(session.profile_store()),
        &first_chunk,
    ) {
        apply_automatic_profile_switch(
            &mut session,
            writer,
            &trace,
            profile_runtime.as_mut(),
            next_profile_names,
        );
    }
    session.push(writer, &trace, &first_chunk)?;
    if let Some(warning) = session.slow_rule_warning() {
        eprintln!("{warning}");
    }
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
        let chunk = prepare_chunk(
            &buffer[..read],
            options.strip_ansi,
            options.sanitize,
            &mut chunk_filter,
        );
        input_bytes += read;
        if let Some(next_profile_names) = observe_dynamic_profile(
            &mut profile_runtime,
            profile_input_rx.as_ref(),
            dynamic_profiles.then_some(session.profile_store()),
            &chunk,
        ) {
            apply_automatic_profile_switch(
                &mut session,
                writer,
                &trace,
                profile_runtime.as_mut(),
                next_profile_names,
            );
        }
        if auto_detect_pending && detection_sample.len() < AUTO_DETECT_SAMPLE_LIMIT {
            detection_sample.extend_from_slice(chunk.bytes());
            let next_profile_names = select_profile_names_with_store(
                options,
                session.profile_store(),
                &detection_sample,
            )?;
            if next_profile_names.as_slice() != session.profile_names() {
                if apply_automatic_profile_switch(
                    &mut session,
                    writer,
                    &trace,
                    None,
                    next_profile_names,
                ) {
                    auto_detect_pending =
                        should_continue_auto_detect(options, session.profile_names());
                } else {
                    auto_detect_pending = false;
                    session.report_current();
                }
            } else if detection_sample.len() >= AUTO_DETECT_SAMPLE_LIMIT {
                auto_detect_pending = false;
                session.report_current();
            }
        }
        if reload_watcher
            .as_mut()
            .is_some_and(ReloadWatcher::reload_requested)
        {
            let retained_profile_sets = profile_runtime
                .as_ref()
                .map(ProfileRuntime::profile_sets_for_reload)
                .unwrap_or_default();
            match session.reload(writer, &trace, &retained_profile_sets) {
                Ok(()) => {
                    if let Some(runtime) = profile_runtime.as_mut() {
                        runtime.accept_reload_generation();
                    }
                }
                Err(error) => {
                    eprintln!(
                        "prismtty: reload ignored; keeping last known good configuration: {}",
                        escape_untrusted(&error.to_string())
                    );
                }
            }
        }
        session.push(writer, &trace, &chunk)?;
        if let Some(warning) = session.slow_rule_warning() {
            eprintln!("{warning}");
        }
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

    if !chunk_filter.carry.is_empty() {
        let trailing = if is_incomplete_utf8_prefix(&chunk_filter.carry) {
            std::mem::take(&mut chunk_filter.carry)
        } else if options.strip_ansi {
            strip_ansi(&chunk_filter.carry)
        } else if options.sanitize {
            strip_string_escapes(&chunk_filter.carry)
        } else {
            std::mem::take(&mut chunk_filter.carry)
        };
        session.push(writer, &trace, &AnsiChunk::new(trailing))?;
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
    least_to_most_recent: VecDeque<Vec<String>>,
}

impl HighlighterCache {
    fn get_or_build<E>(
        &mut self,
        profile_names: &[String],
        build: impl FnOnce() -> Result<Highlighter, E>,
    ) -> Result<Highlighter, E> {
        if let Some(cached) = self.entries.get(profile_names).cloned() {
            self.touch(profile_names);
            return Ok(cached);
        }
        let highlighter = build()?;
        if self.entries.len() >= HIGHLIGHTER_CACHE_LIMIT {
            if let Some(evicted) = self.least_to_most_recent.pop_front() {
                self.entries.remove(&evicted);
            }
        }
        let key = profile_names.to_vec();
        self.entries.insert(key.clone(), highlighter.clone());
        self.least_to_most_recent.push_back(key);
        Ok(highlighter)
    }

    fn touch(&mut self, profile_names: &[String]) {
        if let Some(index) = self
            .least_to_most_recent
            .iter()
            .position(|entry| entry == profile_names)
        {
            self.least_to_most_recent.remove(index);
        }
        self.least_to_most_recent.push_back(profile_names.to_vec());
    }

    fn replace_generation(&mut self, profile_names: &[String], highlighter: &Highlighter) {
        self.entries.clear();
        self.least_to_most_recent.clear();
        let key = profile_names.to_vec();
        self.entries.insert(key.clone(), highlighter.clone());
        self.least_to_most_recent.push_back(key);
    }

    fn insert_prebuilt(&mut self, profile_names: &[String], highlighter: &Highlighter) {
        if self.entries.contains_key(profile_names) {
            self.touch(profile_names);
            return;
        }
        if self.entries.len() >= HIGHLIGHTER_CACHE_LIMIT {
            if let Some(evicted) = self.least_to_most_recent.pop_front() {
                self.entries.remove(&evicted);
            }
        }
        let key = profile_names.to_vec();
        self.entries.insert(key.clone(), highlighter.clone());
        self.least_to_most_recent.push_back(key);
    }
}

struct HighlightSession<'a> {
    options: &'a Options,
    store: ProfileStore,
    overlay_config: crate::config::PrismConfig,
    interactive: bool,
    profile_names: Vec<String>,
    streaming: StreamingHighlighter,
    reporter: ProfileReporter,
    highlighter_cache: HighlighterCache,
    rule_identities: RuleIdentityRegistry,
    rejected_profile_sets: VecDeque<Vec<String>>,
    slow_rule_warned: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProfileSwitchOutcome {
    Unchanged,
    Switched,
    Rejected,
}

impl<'a> HighlightSession<'a> {
    fn new(
        options: &'a Options,
        store: ProfileStore,
        interactive: bool,
        profile_names: Vec<String>,
    ) -> Result<Self, CliError> {
        let overlay_config = load_overlay_config(options)?;
        let mut rule_identities = RuleIdentityRegistry::default();
        let highlighter = build_highlighter_for_profiles_with_store(
            options,
            &store,
            &profile_names,
            interactive,
            &overlay_config,
            &mut rule_identities,
        )?;
        let streaming = new_streaming_highlighter(
            highlighter.clone(),
            interactive,
            options.benchmark,
            options.no_minimal_reset,
        );
        let mut highlighter_cache = HighlighterCache::default();
        highlighter_cache.replace_generation(&profile_names, &highlighter);
        let reporter = ProfileReporter::new(options.show_profile, auto_detect_enabled(options));
        Ok(Self {
            options,
            store,
            overlay_config,
            interactive,
            profile_names,
            streaming,
            reporter,
            highlighter_cache,
            rule_identities,
            rejected_profile_sets: VecDeque::new(),
            slow_rule_warned: false,
        })
    }

    fn profile_names(&self) -> &[String] {
        &self.profile_names
    }

    fn profile_store(&self) -> &ProfileStore {
        &self.store
    }

    fn report_current(&mut self) {
        self.reporter.report(&self.profile_names);
    }

    fn switch_profiles<W: Write>(
        &mut self,
        writer: &mut W,
        trace: &IoTrace,
        profile_names: Vec<String>,
    ) -> Result<ProfileSwitchOutcome, CliError> {
        if profile_names == self.profile_names {
            return Ok(ProfileSwitchOutcome::Unchanged);
        }
        if self.rejected_profile_sets.contains(&profile_names) {
            return Ok(ProfileSwitchOutcome::Rejected);
        }
        match self.rebuild(writer, trace, profile_names.clone(), true) {
            Ok(()) => Ok(ProfileSwitchOutcome::Switched),
            Err(error) => {
                // Profile and overlay configuration are immutable snapshots for
                // this reload generation, so a failed set is deterministic
                // until the next successful reload clears this bounded cache.
                self.remember_rejected_profile_set(profile_names);
                Err(error)
            }
        }
    }

    fn reload<W: Write>(
        &mut self,
        writer: &mut W,
        trace: &IoTrace,
        retained_profile_sets: &[Vec<String>],
    ) -> Result<(), CliError> {
        let next_store = profile_store()?;
        self.reload_with_store(writer, trace, next_store, retained_profile_sets)
    }

    fn reload_with_store<W: Write>(
        &mut self,
        _writer: &mut W,
        _trace: &IoTrace,
        next_store: ProfileStore,
        retained_profile_sets: &[Vec<String>],
    ) -> Result<(), CliError> {
        let next_overlay_config = load_overlay_config(self.options)?;
        let mut next_rule_identities = RuleIdentityRegistry::default();
        let highlighter = build_highlighter_for_profiles_with_store(
            self.options,
            &next_store,
            &self.profile_names,
            self.interactive,
            &next_overlay_config,
            &mut next_rule_identities,
        )?;
        let mut retained_highlighters = Vec::new();
        for profile_names in retained_profile_sets {
            if profile_names == &self.profile_names
                || retained_highlighters
                    .iter()
                    .any(|(retained, _)| retained == profile_names)
            {
                continue;
            }
            let retained = build_highlighter_for_profiles_with_store(
                self.options,
                &next_store,
                profile_names,
                self.interactive,
                &next_overlay_config,
                &mut next_rule_identities,
            )?;
            retained_highlighters.push((profile_names.clone(), retained));
        }
        self.store = next_store;
        self.overlay_config = next_overlay_config;
        self.rule_identities = next_rule_identities;
        self.highlighter_cache
            .replace_generation(&self.profile_names, &highlighter);
        for (profile_names, retained) in retained_highlighters {
            self.highlighter_cache
                .insert_prebuilt(&profile_names, &retained);
        }
        self.rejected_profile_sets.clear();
        self.streaming.replace_highlighter_generation(highlighter);
        self.slow_rule_warned = false;
        Ok(())
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

    /// One-time warning naming the slowest rule once its cumulative matching
    /// time crosses [`SLOW_RULE_WARN_THRESHOLD`]. A pathological (usually
    /// imported) regex degrades throughput so badly it reads as a hang; naming
    /// the rule points the user at `--benchmark` instead of stalling silently.
    fn slow_rule_warning(&mut self) -> Option<String> {
        self.slow_rule_warning_at(SLOW_RULE_WARN_THRESHOLD)
    }

    fn slow_rule_warning_at(&mut self, threshold: Duration) -> Option<String> {
        if self.slow_rule_warned {
            return None;
        }
        if let Some(rule) = self.streaming.first_rule_error() {
            self.slow_rule_warned = true;
            let description = escape_untrusted(rule.description);
            let error = escape_untrusted(rule.last.unwrap_or("PCRE2 match error"));
            return Some(format!(
                "prismtty: rule '{description}' stopped after a bounded regex match error: {error} (inspect with --benchmark)"
            ));
        }
        let rule = self.streaming.slowest_rule()?;
        if rule.duration < threshold {
            return None;
        }
        self.slow_rule_warned = true;
        let description = escape_untrusted(&rule.description);
        Some(format!(
            "prismtty: rule '{}' has spent {:.1}s matching; a slow regex is degrading throughput (profile with --benchmark)",
            description,
            rule.duration.as_secs_f64(),
        ))
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
        let options = self.options;
        let store = &self.store;
        let overlay_config = &self.overlay_config;
        let interactive = self.interactive;
        let rule_identities = &mut self.rule_identities;
        let highlighter_cache = &mut self.highlighter_cache;
        let highlighter = highlighter_cache.get_or_build(&profile_names, || {
            build_highlighter_for_profiles_with_store(
                options,
                store,
                &profile_names,
                interactive,
                overlay_config,
                rule_identities,
            )
        })?;
        self.profile_names = profile_names;
        self.streaming.replace_highlighter(highlighter);
        if report {
            self.report_current();
        }
        Ok(())
    }

    fn remember_rejected_profile_set(&mut self, profile_names: Vec<String>) {
        if self.rejected_profile_sets.contains(&profile_names) {
            return;
        }
        if self.rejected_profile_sets.len() >= HIGHLIGHTER_CACHE_LIMIT {
            self.rejected_profile_sets.pop_front();
        }
        self.rejected_profile_sets.push_back(profile_names);
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

fn apply_automatic_profile_switch<W: Write>(
    session: &mut HighlightSession<'_>,
    writer: &mut W,
    trace: &IoTrace,
    runtime: Option<&mut ProfileRuntime>,
    profile_names: Vec<String>,
) -> bool {
    let accepted = match session.switch_profiles(writer, trace, profile_names) {
        Ok(ProfileSwitchOutcome::Unchanged | ProfileSwitchOutcome::Switched) => true,
        Ok(ProfileSwitchOutcome::Rejected) => false,
        Err(error) => {
            eprintln!(
                "prismtty: automatic profile switch ignored; keeping last known good rules: {}",
                escape_untrusted(&error.to_string())
            );
            false
        }
    };

    if let Some(runtime) = runtime {
        if accepted {
            runtime.accept_transition();
        } else {
            runtime.reject_transition();
        }
    }
    accepted
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
        let mut rules = report.rules_with_errors().collect::<Vec<_>>();
        rules.sort_by_key(|(rule, _error_count, _last_error)| Reverse(rule.duration));
        for (rule, error_count, last_error) in rules {
            let percent = if total > 0.0 {
                rule.duration.as_secs_f64() / total * 100.0
            } else {
                0.0
            };
            let description = escape_untrusted(&rule.description);
            eprintln!(
                "{percent:>6.2}% {:>8.3}s  {:<7}  {}",
                rule.duration.as_secs_f64(),
                rule.match_count,
                description
            );
            if error_count > 0 {
                let error = escape_untrusted(last_error.unwrap_or("PCRE2 match error"));
                eprintln!("         match errors: {} (last: {})", error_count, error);
            }
        }
    }
    eprintln!("Processed {input_bytes} bytes in {elapsed_secs:.3}s");
}

fn prepare_chunk(
    input: &[u8],
    strip_existing_ansi: bool,
    sanitize: bool,
    state: &mut ChunkFilterState,
) -> AnsiChunk {
    // Reassemble any escape that was split across the previous read before
    // filtering or profile observation. Preserve mode returns the completed
    // control byte-for-byte, but it must not expose a later payload fragment as
    // visible banner text to dynamic profile detection.
    let incoming = if state.oversized_string_controls.is_discarding() {
        let Some(suffix) = state.oversized_string_controls.consume_discarded(input) else {
            return AnsiChunk::new(Vec::new());
        };
        suffix
    } else {
        input.to_vec()
    };
    let mut combined = std::mem::take(&mut state.carry);
    combined.extend_from_slice(&incoming);
    let combined = state
        .oversized_string_controls
        .filter_reassembled(&combined);
    let split = incomplete_sanitize_start(&combined).unwrap_or(combined.len());
    state.carry.extend_from_slice(&combined[split..]);
    if state.carry.len() > MAX_INCOMPLETE_ESCAPE_BYTES {
        state.carry.clear();
    }
    if strip_existing_ansi {
        AnsiChunk::new(strip_ansi(&combined[..split]))
    } else if sanitize {
        AnsiChunk::new(strip_string_escapes(&combined[..split]))
    } else {
        AnsiChunk::from_slice(&combined[..split])
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

    #[test]
    fn highlighter_cache_evicts_only_the_least_recently_used_entry() {
        let mut cache = super::HighlighterCache::default();
        let mut builds = 0usize;
        let sets = (0..super::HIGHLIGHTER_CACHE_LIMIT)
            .map(|index| vec![format!("profile-{index}")])
            .collect::<Vec<_>>();

        for set in &sets {
            cache
                .get_or_build(set, || {
                    builds += 1;
                    Ok::<_, ()>(sample_highlighter())
                })
                .unwrap();
        }
        cache
            .get_or_build(&sets[0], || {
                builds += 1;
                Ok::<_, ()>(sample_highlighter())
            })
            .unwrap();
        cache
            .get_or_build(&[String::from("overflow")], || {
                builds += 1;
                Ok::<_, ()>(sample_highlighter())
            })
            .unwrap();
        cache
            .get_or_build(&sets[0], || {
                builds += 1;
                Ok::<_, ()>(sample_highlighter())
            })
            .unwrap();

        assert_eq!(
            builds,
            super::HIGHLIGHTER_CACHE_LIMIT + 1,
            "the recently used entry must survive one capacity eviction"
        );
    }

    #[test]
    fn failed_profile_switch_keeps_the_last_good_session() {
        let mut store = crate::profiles::ProfileStore::builtin();
        let rule = crate::config::PrismConfig::from_chromaterm_yaml(
            "rules:\n  - description: bounded\n    regex: bounded\n    color: f#ffffff\n",
        )
        .expect("test rule parses")
        .rules
        .pop()
        .expect("test rule exists");
        store.insert_profile(
            "oversized".to_string(),
            Vec::new(),
            vec!["OversizedOS".to_string()],
            vec![rule; 513],
        );

        let options = crate::cli::args::Options::default();
        let mut session =
            super::HighlightSession::new(&options, store, false, vec!["generic".to_string()])
                .expect("last-good session builds");
        let trace = super::IoTrace::open(None).expect("no-op trace");
        let mut writer = Vec::new();

        assert!(
            session
                .switch_profiles(
                    &mut writer,
                    &trace,
                    vec!["generic".to_string(), "oversized".to_string()],
                )
                .is_err(),
            "an aggregate-over-limit profile must be rejected"
        );
        assert_eq!(
            session.profile_names(),
            ["generic".to_string()],
            "a failed compile must not change the selected profile set"
        );
        assert_eq!(session.rejected_profile_sets.len(), 1);
        assert_eq!(
            session
                .switch_profiles(
                    &mut writer,
                    &trace,
                    vec!["generic".to_string(), "oversized".to_string()],
                )
                .expect("a deterministic rejected set is skipped without recompiling"),
            super::ProfileSwitchOutcome::Rejected
        );
        assert_eq!(session.rejected_profile_sets.len(), 1);

        session
            .push(
                &mut writer,
                &trace,
                &crate::highlight::AnsiChunk::from_slice(b"still usable\n"),
            )
            .expect("last-good highlighter remains usable");
    }

    #[test]
    fn failed_dynamic_profile_switch_rolls_back_runtime_transition() {
        let runtime_store = crate::profiles::ProfileStore::builtin();
        let mut session_store = runtime_store.clone();
        let rule = crate::config::PrismConfig::from_chromaterm_yaml(
            "rules:\n  - description: bounded\n    regex: bounded\n    color: f#ffffff\n",
        )
        .expect("test rule parses")
        .rules
        .pop()
        .expect("test rule exists");
        session_store.insert_profile(
            "juniper".to_string(),
            Vec::new(),
            vec!["JUNOS".to_string()],
            vec![rule; 513],
        );

        let initial = vec!["generic".to_string(), "linux-unix".to_string()];
        let mut runtime = crate::profile_runtime::ProfileRuntime::new(initial.clone());
        runtime.observe_input(b"ssh router-a\r");
        let proposed = runtime
            .observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &runtime_store)
            .expect("runtime proposes Juniper");

        let options = crate::cli::args::Options::default();
        let mut session =
            super::HighlightSession::new(&options, session_store, true, initial.clone())
                .expect("last-good session builds");
        let trace = super::IoTrace::open(None).expect("no-op trace");
        let mut writer = Vec::new();

        assert!(
            !super::apply_automatic_profile_switch(
                &mut session,
                &mut writer,
                &trace,
                Some(&mut runtime),
                proposed,
            ),
            "an invalid detected profile must be ignored"
        );
        assert_eq!(session.profile_names(), initial);
        assert_eq!(runtime.active_profiles(), initial);
        assert_eq!(runtime.stack_len(), 1);
    }

    #[test]
    fn profile_switches_use_one_overlay_snapshot_until_reload() {
        let config = tempfile::NamedTempFile::new().expect("temp config");
        let yaml = "rules:\n  - description: local\n    regex: local\n    color: f#ffffff\n";
        std::fs::write(config.path(), yaml).expect("config writes");
        let options = crate::cli::args::Options {
            config: Some(config.path().to_path_buf()),
            ..crate::cli::args::Options::default()
        };
        let mut session = super::HighlightSession::new(
            &options,
            crate::profiles::ProfileStore::builtin(),
            false,
            vec!["generic".to_string()],
        )
        .expect("initial config builds");
        let trace = super::IoTrace::open(None).expect("no-op trace");
        let mut writer = Vec::new();
        let cisco = vec!["generic".to_string(), "cisco".to_string()];

        std::fs::remove_file(config.path()).expect("config is temporarily removed");
        assert_eq!(
            session
                .switch_profiles(&mut writer, &trace, cisco.clone())
                .expect("profile switch uses the loaded overlay snapshot"),
            super::ProfileSwitchOutcome::Switched
        );

        std::fs::write(
            config.path(),
            "rules:\n  - description: invalid\n    regex: '('\n    color: f#ffffff\n",
        )
        .expect("temporarily invalid regex writes");
        let juniper = vec!["generic".to_string(), "juniper".to_string()];
        assert_eq!(
            session
                .switch_profiles(&mut writer, &trace, juniper.clone())
                .expect("on-disk edits wait for reload"),
            super::ProfileSwitchOutcome::Switched
        );
        assert!(
            session
                .reload_with_store(
                    &mut writer,
                    &trace,
                    crate::profiles::ProfileStore::builtin(),
                    &[],
                )
                .is_err(),
            "an invalid new overlay generation is rejected transactionally"
        );
        std::fs::write(config.path(), yaml).expect("valid regex is restored");
        session
            .reload_with_store(
                &mut writer,
                &trace,
                crate::profiles::ProfileStore::builtin(),
                &[],
            )
            .expect("restored overlay starts a new generation");
        assert_eq!(session.profile_names(), juniper);
    }

    #[test]
    fn initial_highlighter_is_cached_for_safe_unwind() {
        let config = tempfile::NamedTempFile::new().expect("temp config");
        std::fs::write(config.path(), "rules: []\n").expect("config writes");
        let options = crate::cli::args::Options {
            config: Some(config.path().to_path_buf()),
            ..crate::cli::args::Options::default()
        };
        let initial = vec!["generic".to_string()];
        let mut session = super::HighlightSession::new(
            &options,
            crate::profiles::ProfileStore::builtin(),
            false,
            initial.clone(),
        )
        .expect("initial session builds");
        let trace = super::IoTrace::open(None).expect("no-op trace");
        let mut writer = Vec::new();
        assert_eq!(
            session
                .switch_profiles(
                    &mut writer,
                    &trace,
                    vec!["generic".to_string(), "cisco".to_string()],
                )
                .expect("remote profile builds"),
            super::ProfileSwitchOutcome::Switched
        );

        std::fs::remove_file(config.path()).expect("config becomes unavailable");
        assert_eq!(
            session
                .switch_profiles(&mut writer, &trace, initial.clone())
                .expect("known-good initial highlighter is reused"),
            super::ProfileSwitchOutcome::Switched
        );
        assert_eq!(session.profile_names(), initial);
    }

    #[test]
    fn rejected_profile_set_is_bounded_until_overlay_reload() {
        let store = crate::profiles::ProfileStore::builtin();
        let generic_count = crate::config::PrismConfig::from_profiles(&store, &["generic"])
            .expect("generic resolves")
            .rules
            .len();
        let juniper_count =
            crate::config::PrismConfig::from_profiles(&store, &["generic", "juniper"])
                .expect("Juniper resolves")
                .rules
                .len();
        assert!(juniper_count > generic_count);
        let overlay_rule_count = 512 - generic_count;
        let mut yaml = String::from("rules:\n");
        for index in 0..overlay_rule_count {
            yaml.push_str(&format!(
                "  - description: overlay-{index}\n    regex: token-{index}\n    color: f#ffffff\n"
            ));
        }

        let config = tempfile::NamedTempFile::new().expect("temp config");
        std::fs::write(config.path(), &yaml).expect("near-limit config writes");
        let options = crate::cli::args::Options {
            config: Some(config.path().to_path_buf()),
            ..crate::cli::args::Options::default()
        };
        let initial = vec!["generic".to_string()];
        let target = vec!["generic".to_string(), "juniper".to_string()];
        let mut session = super::HighlightSession::new(&options, store.clone(), false, initial)
            .expect("near-limit generic set builds");
        let trace = super::IoTrace::open(None).expect("no-op trace");
        let mut writer = Vec::new();

        assert!(
            session
                .switch_profiles(&mut writer, &trace, target.clone())
                .is_err(),
            "the larger profile set crosses the aggregate limit"
        );
        assert_eq!(session.rejected_profile_sets.len(), 1);
        std::fs::write(config.path(), "rules: []\n").expect("config is simplified");
        assert_eq!(
            session
                .switch_profiles(&mut writer, &trace, target.clone())
                .expect("same generation uses the negative cache"),
            super::ProfileSwitchOutcome::Rejected
        );

        session
            .reload_with_store(&mut writer, &trace, store, &[])
            .expect("reload accepts the simplified overlay");
        assert_eq!(
            session
                .switch_profiles(&mut writer, &trace, target)
                .expect("new generation retries the profile set"),
            super::ProfileSwitchOutcome::Switched
        );
    }

    #[test]
    fn dynamic_reload_requires_buildable_retained_profiles() {
        let mut current_store = crate::profiles::ProfileStore::builtin();
        current_store.insert_profile("custom".to_string(), Vec::new(), Vec::new(), Vec::new());
        let options = crate::cli::args::Options::default();
        let mut session =
            super::HighlightSession::new(&options, current_store, true, vec!["custom".to_string()])
                .expect("current custom profile builds");

        let rule = crate::config::PrismConfig::from_chromaterm_yaml(
            "rules:\n  - description: bounded\n    regex: bounded\n    color: f#ffffff\n",
        )
        .expect("test rule parses")
        .rules
        .pop()
        .expect("test rule exists");
        let mut next_store = crate::profiles::ProfileStore::builtin();
        next_store.insert_profile("custom".to_string(), Vec::new(), Vec::new(), Vec::new());
        next_store.insert_profile(
            "generic".to_string(),
            Vec::new(),
            Vec::new(),
            vec![rule; 513],
        );
        let trace = super::IoTrace::open(None).expect("no-op trace");
        let mut writer = Vec::new();

        assert!(
            session
                .reload_with_store(
                    &mut writer,
                    &trace,
                    next_store,
                    &[vec!["generic".to_string()]],
                )
                .is_err(),
            "a dynamic reload must reject an unusable retained frame"
        );
        assert_eq!(session.profile_names(), ["custom".to_string()]);
        assert!(
            session
                .profile_store()
                .profile("generic")
                .is_some_and(|profile| profile.rules.len() < 513),
            "the previous store remains installed after rejection"
        );
    }

    #[test]
    fn reload_rebuilds_from_disk_and_keeps_last_good_rules_on_failure() {
        let config = tempfile::NamedTempFile::new().expect("temp config");
        let write_config = |color: &str| {
            std::fs::write(
                config.path(),
                format!(
                    "rules:\n  - description: reload token\n    regex: reload-token\n    color: {color}\n"
                ),
            )
            .expect("config writes");
        };
        write_config("f#ff0000");

        let options = crate::cli::args::Options {
            config: Some(config.path().to_path_buf()),
            force_rgb: true,
            ..crate::cli::args::Options::default()
        };
        let mut session = super::HighlightSession::new(
            &options,
            crate::profiles::ProfileStore::builtin(),
            false,
            vec!["generic".to_string()],
        )
        .expect("session builds");
        let trace = super::IoTrace::open(None).expect("no-op trace");
        let chunk = crate::highlight::AnsiChunk::from_slice(b"reload-token\n");
        let mut rendered = Vec::new();
        session
            .push(&mut rendered, &trace, &chunk)
            .expect("initial render succeeds");
        assert!(
            rendered
                .windows(b"\x1b[38;2;255;0;0mreload-token".len())
                .any(|window| window == b"\x1b[38;2;255;0;0mreload-token")
        );

        write_config("f#0000ff");
        session.slow_rule_warned = true;
        session
            .reload_with_store(
                &mut rendered,
                &trace,
                crate::profiles::ProfileStore::builtin(),
                &[],
            )
            .expect("valid reload succeeds");
        assert!(
            !session.slow_rule_warned,
            "new generation may emit a fresh warning"
        );
        rendered.clear();
        session
            .push(&mut rendered, &trace, &chunk)
            .expect("reloaded render succeeds");
        assert!(
            rendered
                .windows(b"\x1b[38;2;0;0;255mreload-token".len())
                .any(|window| window == b"\x1b[38;2;0;0;255mreload-token")
        );

        std::fs::write(config.path(), "rules: [").expect("invalid config writes");
        assert!(
            session
                .reload_with_store(
                    &mut rendered,
                    &trace,
                    crate::profiles::ProfileStore::builtin(),
                    &[],
                )
                .is_err()
        );
        rendered.clear();
        session
            .push(&mut rendered, &trace, &chunk)
            .expect("last good rules remain usable");
        assert!(
            rendered
                .windows(b"\x1b[38;2;0;0;255mreload-token".len())
                .any(|window| window == b"\x1b[38;2;0;0;255mreload-token")
        );
    }

    // In --strip-ansi mode an escape split across reads must be reassembled
    // before stripping, so its parameter/final bytes do not leak into the
    // visible output as literal text.
    #[test]
    fn strip_mode_carries_split_escape_across_reads() {
        let mut carry = super::ChunkFilterState::default();
        let first = super::prepare_chunk(b"hello\x1b[3", true, false, &mut carry);
        let second = super::prepare_chunk(b"1m world", true, false, &mut carry);
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
        let mut carry = super::ChunkFilterState::default();
        let mut input = b"\x1b[".to_vec();
        input.extend(std::iter::repeat_n(
            b'1',
            crate::highlight::MAX_INCOMPLETE_ESCAPE_BYTES + 1,
        ));

        let chunk = super::prepare_chunk(&input, true, false, &mut carry);

        assert!(
            chunk.bytes().is_empty(),
            "oversized incomplete escape should be stripped as control data"
        );
        assert!(
            carry.carry.is_empty(),
            "oversized incomplete escape carry must not grow without bound"
        );
    }

    // --sanitize: a string escape split across reads must be reassembled and
    // dropped whole, so neither its payload nor its tail leaks to the terminal.
    #[test]
    fn sanitize_mode_drops_split_string_escape_across_reads() {
        let mut carry = super::ChunkFilterState::default();
        let first = super::prepare_chunk(b"safe\x1b]52;c;", false, true, &mut carry);
        let second = super::prepare_chunk(b"aGVsbG8=\x07 after", false, true, &mut carry);
        let mut visible = first.bytes().to_vec();
        visible.extend_from_slice(second.bytes());
        assert_eq!(
            visible,
            b"safe after",
            "split OSC 52 leaked into sanitized output: {:?}",
            String::from_utf8_lossy(&visible)
        );
    }

    #[test]
    fn preserve_mode_hides_split_string_payloads_from_dynamic_profile_detection() {
        for (prefix, tail) in [
            (
                b"\x1b]0;".as_slice(),
                b"--- JUNOS 22.4R3 Kernel 64-bit\n\x07visible\n".as_slice(),
            ),
            (
                b"\x1bP".as_slice(),
                b"--- JUNOS 22.4R3 Kernel 64-bit\n\x1b\\visible\n".as_slice(),
            ),
            (
                b"\x9d0;".as_slice(),
                b"--- JUNOS 22.4R3 Kernel 64-bit\n\x07visible\n".as_slice(),
            ),
            (
                b"\x90".as_slice(),
                b"--- JUNOS 22.4R3 Kernel 64-bit\n\x9cvisible\n".as_slice(),
            ),
            (
                b"\x1b[1\x1b]0;".as_slice(),
                b"--- JUNOS 22.4R3 Kernel 64-bit\n\x07visible\n".as_slice(),
            ),
        ] {
            let mut state = super::ChunkFilterState::default();
            let first = super::prepare_chunk(prefix, false, false, &mut state);
            let second = super::prepare_chunk(tail, false, false, &mut state);

            assert!(first.bytes().is_empty());
            assert_eq!(second.bytes(), [prefix, tail].concat());
            assert_eq!(second.visible_bytes(), b"visible\n");

            let store = crate::profiles::ProfileStore::builtin();
            let mut runtime =
                crate::profile_runtime::ProfileRuntime::new(vec!["generic".to_string()]);
            assert_eq!(runtime.observe_output(first.visible_bytes(), &store), None);
            assert_eq!(runtime.observe_output(second.visible_bytes(), &store), None);
            assert_eq!(runtime.active_profiles(), vec!["generic".to_string()]);
        }
    }

    #[test]
    fn sanitize_mode_drops_split_c1_string_escape_across_reads() {
        let mut carry = super::ChunkFilterState::default();
        let first = super::prepare_chunk(b"safe\x9d52;c;", false, true, &mut carry);
        let second = super::prepare_chunk(b"aGVsbG8=\x07 after", false, true, &mut carry);
        let mut visible = first.bytes().to_vec();
        visible.extend_from_slice(second.bytes());
        assert_eq!(
            visible,
            b"safe after",
            "split C1 OSC 52 leaked into sanitized output: {:?}",
            String::from_utf8_lossy(&visible)
        );
    }

    #[test]
    fn filtered_modes_drop_split_utf8_encoded_c1_strings() {
        for (strip_existing_ansi, sanitize) in [(true, false), (false, true)] {
            for (input, expected) in [
                (
                    b"safe\xc2\x9d52;c;aGVsbG8=\x07 after".as_slice(),
                    b"safe after".as_slice(),
                ),
                (
                    b"safe\xc2\x90dcs\xc2\x9c after".as_slice(),
                    b"safe after".as_slice(),
                ),
            ] {
                for split in 0..=input.len() {
                    let mut state = super::ChunkFilterState::default();
                    let mut output = super::prepare_chunk(
                        &input[..split],
                        strip_existing_ansi,
                        sanitize,
                        &mut state,
                    )
                    .bytes()
                    .to_vec();
                    output.extend_from_slice(
                        super::prepare_chunk(
                            &input[split..],
                            strip_existing_ansi,
                            sanitize,
                            &mut state,
                        )
                        .bytes(),
                    );

                    assert_eq!(output, expected, "unsafe split at {split}");
                    assert!(state.carry.is_empty(), "carry remained at split {split}");
                }
            }
        }
    }

    #[test]
    fn filtered_modes_keep_discarding_oversized_utf8_encoded_c1_strings() {
        for (strip_existing_ansi, sanitize) in [(true, false), (false, true)] {
            let mut state = super::ChunkFilterState::default();
            let mut oversized = b"safe\xc2\x90".to_vec();
            oversized.extend(std::iter::repeat_n(
                b'x',
                crate::highlight::MAX_INCOMPLETE_ESCAPE_BYTES + 1,
            ));

            let mut output =
                super::prepare_chunk(&oversized, strip_existing_ansi, sanitize, &mut state)
                    .bytes()
                    .to_vec();
            output.extend_from_slice(
                super::prepare_chunk(b"\xc2", strip_existing_ansi, sanitize, &mut state).bytes(),
            );
            output.extend_from_slice(
                super::prepare_chunk(b"\x9c after", strip_existing_ansi, sanitize, &mut state)
                    .bytes(),
            );

            assert_eq!(output, b"safe after");
            assert!(state.carry.is_empty());
        }
    }

    #[test]
    fn filtered_modes_resume_after_split_can_or_sub_cancels_string() {
        for (strip_existing_ansi, sanitize) in [(true, false), (false, true)] {
            for control in [
                b"\x1b]52;abc".as_slice(),
                b"\x1bPabc".as_slice(),
                b"\x9d52;abc".as_slice(),
                b"\x90abc".as_slice(),
            ] {
                for cancel in [0x18, 0x1a] {
                    let mut state = super::ChunkFilterState::default();
                    let mut first = b"before".to_vec();
                    first.extend_from_slice(control);
                    let mut output =
                        super::prepare_chunk(&first, strip_existing_ansi, sanitize, &mut state)
                            .bytes()
                            .to_vec();
                    let second = [cancel, b'a', b'f', b't', b'e', b'r', b'\n'];
                    output.extend_from_slice(
                        super::prepare_chunk(&second, strip_existing_ansi, sanitize, &mut state)
                            .bytes(),
                    );

                    assert_eq!(output, b"beforeafter\n");
                    assert!(state.carry.is_empty());
                }
            }
        }
    }

    #[test]
    fn sanitize_mode_keeps_discarding_oversized_strings_until_their_terminator() {
        for (introducer, terminator) in [
            (b"\x1b]".as_slice(), b"\x07".as_slice()),
            (b"\x9d".as_slice(), b"\x07".as_slice()),
            (b"\x1bP".as_slice(), b"\x1b\\".as_slice()),
            (b"\x1bX".as_slice(), b"\x1b\\".as_slice()),
            (b"\x1b^".as_slice(), b"\x1b\\".as_slice()),
            (b"\x1b_".as_slice(), b"\x1b\\".as_slice()),
            (b"\x90".as_slice(), b"\x9c".as_slice()),
            (b"\x98".as_slice(), b"\x9c".as_slice()),
            (b"\x9e".as_slice(), b"\x9c".as_slice()),
            (b"\x9f".as_slice(), b"\x9c".as_slice()),
        ] {
            let mut carry = super::ChunkFilterState::default();
            let mut first = b"safe".to_vec();
            first.extend_from_slice(introducer);
            first.extend(std::iter::repeat_n(
                b'x',
                crate::highlight::MAX_INCOMPLETE_ESCAPE_BYTES + 1,
            ));

            let mut output = super::prepare_chunk(&first, false, true, &mut carry)
                .bytes()
                .to_vec();
            output.extend_from_slice(
                super::prepare_chunk(b"\x1b[2J\x9b2J", false, true, &mut carry).bytes(),
            );
            if terminator == b"\x1b\\" {
                output.extend_from_slice(
                    super::prepare_chunk(b"\x1b", false, true, &mut carry).bytes(),
                );
                output.extend_from_slice(
                    super::prepare_chunk(b"\\after", false, true, &mut carry).bytes(),
                );
            } else {
                let mut final_chunk = terminator.to_vec();
                final_chunk.extend_from_slice(b"after");
                output.extend_from_slice(
                    super::prepare_chunk(&final_chunk, false, true, &mut carry).bytes(),
                );
            }

            assert_eq!(
                output, b"safeafter",
                "payload escaped from {introducer:02x?}: {output:02x?}"
            );
            assert!(carry.carry.is_empty());
        }
    }

    #[test]
    fn sanitize_mode_resumes_after_split_can_or_sub_cancels_oversized_string() {
        for control in [
            b"\x1b]52;".as_slice(),
            b"\x1bP".as_slice(),
            b"\x9d52;".as_slice(),
            b"\x90".as_slice(),
        ] {
            for cancel in [0x18, 0x1a] {
                let mut state = super::ChunkFilterState::default();
                let mut first = b"before".to_vec();
                first.extend_from_slice(control);
                first.extend(std::iter::repeat_n(
                    b'x',
                    crate::highlight::MAX_INCOMPLETE_ESCAPE_BYTES + 1,
                ));

                let mut output = super::prepare_chunk(&first, false, true, &mut state)
                    .bytes()
                    .to_vec();
                output.extend_from_slice(
                    super::prepare_chunk(&[cancel], false, true, &mut state).bytes(),
                );
                output.extend_from_slice(
                    super::prepare_chunk(b"after\n", false, true, &mut state).bytes(),
                );

                assert_eq!(output, b"beforeafter\n");
                assert!(state.carry.is_empty());
            }
        }
    }

    #[test]
    fn sanitize_oversized_discard_ignores_non_c1_utf8_continuations() {
        let mut carry = super::ChunkFilterState::default();
        let mut first = b"safe\x1bP".to_vec();
        first.extend(std::iter::repeat_n(
            b'x',
            crate::highlight::MAX_INCOMPLETE_ESCAPE_BYTES + 1,
        ));

        let mut output = super::prepare_chunk(&first, false, true, &mut carry)
            .bytes()
            .to_vec();
        for chunk in [
            b"\xc2".as_slice(),
            b"\xa9".as_slice(),
            b"\xe2".as_slice(),
            b"\x9d".as_slice(),
            b"\x9c\x1b[2J".as_slice(),
            b"\x1b".as_slice(),
            b"\\after".as_slice(),
        ] {
            output.extend_from_slice(super::prepare_chunk(chunk, false, true, &mut carry).bytes());
        }

        assert_eq!(output, b"safeafter");
        assert!(carry.carry.is_empty());
    }

    #[test]
    fn sanitize_tracker_rebases_after_filtering_a_visible_prefix() {
        let mut carry = super::ChunkFilterState::default();
        let mut output = super::prepare_chunk(b"safe\x1b]ab\x1b]", false, true, &mut carry)
            .bytes()
            .to_vec();
        output
            .extend_from_slice(super::prepare_chunk(b"\x07after", false, true, &mut carry).bytes());

        assert_eq!(output, b"safeafter");
        assert!(carry.carry.is_empty());
    }

    #[test]
    fn sanitize_mode_carries_cancelled_prefix_until_nested_string_terminates() {
        for (first, second) in [
            (
                b"safe\x1b[1\x1b]52;c;".as_slice(),
                b"SGVsbG8=\x07Jafter".as_slice(),
            ),
            (
                b"safe\x1b(\x1b]52;c;".as_slice(),
                b"SGVsbG8=\x07Jafter".as_slice(),
            ),
            (
                b"safe\x9b1\x9d52;c;".as_slice(),
                b"SGVsbG8=\x07Jafter".as_slice(),
            ),
            (
                b"safe\x1b[1\x1b".as_slice(),
                b"]52;c;SGVsbG8=\x07Jafter".as_slice(),
            ),
        ] {
            let mut carry = super::ChunkFilterState::default();
            let mut output = super::prepare_chunk(first, false, true, &mut carry)
                .bytes()
                .to_vec();
            output.extend_from_slice(super::prepare_chunk(second, false, true, &mut carry).bytes());

            assert_eq!(output, b"safeJafter", "unsafe output for {first:02x?}");
            assert!(carry.carry.is_empty());
        }
    }

    #[test]
    fn sanitize_mode_discards_oversized_string_reached_through_cancelled_csi() {
        let mut carry = super::ChunkFilterState::default();
        let mut first = b"safe\x1b[1\x1b]52;c;".to_vec();
        first.extend(std::iter::repeat_n(
            b'x',
            crate::highlight::MAX_INCOMPLETE_ESCAPE_BYTES + 1,
        ));

        let mut output = super::prepare_chunk(&first, false, true, &mut carry)
            .bytes()
            .to_vec();
        output.extend_from_slice(
            super::prepare_chunk(b"\x1b[2J\x9b2J", false, true, &mut carry).bytes(),
        );
        output.extend_from_slice(
            super::prepare_chunk(b"\x07Jafter", false, true, &mut carry).bytes(),
        );

        assert_eq!(output, b"safeJafter");
        assert!(carry.carry.is_empty());
    }

    #[test]
    fn sanitize_mode_preserves_split_utf8_across_reads() {
        let mut carry = super::ChunkFilterState::default();
        let first = super::prepare_chunk(b"prompt \xe2", false, true, &mut carry);
        let second = super::prepare_chunk(b"\x9d\xaf ready", false, true, &mut carry);
        let mut visible = first.bytes().to_vec();
        visible.extend_from_slice(second.bytes());
        assert_eq!(visible, "prompt \u{276f} ready".as_bytes());
    }

    #[test]
    fn strip_mode_preserves_utf8_when_each_c1_valued_continuation_is_split() {
        let mut carry = super::ChunkFilterState::default();
        let chunks = [
            b"prompt \xe2".as_slice(),
            b"\x9d".as_slice(),
            b"\xaf ready".as_slice(),
        ];
        let mut visible = Vec::new();

        for chunk in chunks {
            visible.extend_from_slice(super::prepare_chunk(chunk, true, false, &mut carry).bytes());
        }

        assert!(carry.carry.is_empty());
        assert_eq!(visible, "prompt \u{276f} ready".as_bytes());
    }

    // The slow-rule warning names the slowest rule once per session; the
    // threshold is injected so the test does not depend on wall-clock time.
    #[test]
    fn slow_rule_warning_fires_once_per_session() {
        let store = crate::profiles::ProfileStore::builtin();
        let options = crate::cli::args::Options::default();
        let mut session = super::HighlightSession::new(
            &options,
            store.clone(),
            false,
            vec!["generic".to_string()],
        )
        .expect("session builds");
        let trace = super::IoTrace::open(None).expect("no-op trace");
        let mut writer: Vec<u8> = Vec::new();
        let chunk = crate::highlight::AnsiChunk::from_slice(b"status up\n");
        session
            .push(&mut writer, &trace, &chunk)
            .expect("push succeeds");

        let warning = session
            .slow_rule_warning_at(std::time::Duration::ZERO)
            .expect("zero threshold names the slowest rule");
        assert!(
            warning.contains("--benchmark"),
            "warning should point at --benchmark: {warning}"
        );
        assert!(
            session
                .slow_rule_warning_at(std::time::Duration::ZERO)
                .is_none(),
            "warning must fire at most once per session"
        );
        // A healthy rule set never crosses the real threshold.
        let mut fresh =
            super::HighlightSession::new(&options, store, false, vec!["generic".to_string()])
                .expect("session builds");
        fresh
            .push(&mut writer, &trace, &chunk)
            .expect("push succeeds");
        assert!(fresh.slow_rule_warning().is_none());
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
            // SAFETY: `slave_fd` is the open PTY slave from openpty and the buffer is a valid
            // 1-byte slice.
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
            // SAFETY: `slave_fd` is the open PTY slave from openpty and the buffer is a valid
            // 1-byte slice.
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
