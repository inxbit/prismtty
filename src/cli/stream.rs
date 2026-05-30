use std::cmp::Reverse;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::time::Instant;

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

/// Largest read still treated as interactive keystroke echo. Bulk program output
/// arrives in much larger reads, so only tiny reads trigger an echo flush; that
/// keeps cross-read token highlighting intact for streamed output.
const INTERACTIVE_ECHO_FLUSH_MAX_READ: usize = 8;

pub(super) fn highlight_stream<R: Read, W: Write>(
    mut reader: R,
    writer: &mut W,
    options: &Options,
    interactive: bool,
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
    let mut session = HighlightSession::new(options, &store, interactive, profile_names)?;
    session.report_current();
    let dynamic_profiles =
        dynamic_profile_enabled(options, interactive) && profile_input_rx.is_some();
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
    if should_flush_input_echo(interactive, read) {
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
        if should_flush_input_echo(interactive, read) {
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

fn should_flush_input_echo(interactive: bool, read: usize) -> bool {
    interactive && read <= INTERACTIVE_ECHO_FLUSH_MAX_READ
}

#[cfg(test)]
mod tests {
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
    fn input_echo_flush_targets_only_small_interactive_reads() {
        // Keystroke-sized interactive reads flush buffered echo promptly.
        assert!(super::should_flush_input_echo(true, 1));
        assert!(super::should_flush_input_echo(
            true,
            super::INTERACTIVE_ECHO_FLUSH_MAX_READ
        ));
        // Bulk interactive reads keep speculative token buffering for highlighting.
        assert!(!super::should_flush_input_echo(
            true,
            super::INTERACTIVE_ECHO_FLUSH_MAX_READ + 1
        ));
        // Noninteractive streams never force an early echo flush.
        assert!(!super::should_flush_input_echo(false, 1));
    }
}
