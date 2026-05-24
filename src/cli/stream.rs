use std::cmp::Reverse;
use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::time::Instant;

use crate::highlight::{AnsiChunk, BenchmarkReport, Highlighter, StreamingHighlighter, strip_ansi};
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
    let read = reader.read(&mut buffer)?;
    if read == 0 {
        return Ok(());
    }

    trace.log("OUT", &buffer[..read]);
    let first_chunk = prepare_chunk(&buffer[..read], options.strip_ansi);
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
    writer.flush()?;

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        trace.log("OUT", &buffer[..read]);
        let chunk = prepare_chunk(&buffer[..read], options.strip_ansi);
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

struct HighlightSession<'a> {
    options: &'a Options,
    store: &'a ProfileStore,
    interactive: bool,
    profile_names: Vec<String>,
    streaming: StreamingHighlighter,
    reporter: ProfileReporter,
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
        let highlighter = build_highlighter_for_profiles_with_store(
            self.options,
            self.store,
            &self.profile_names,
            self.interactive,
        )?;
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

fn prepare_chunk(input: &[u8], strip_existing_ansi: bool) -> AnsiChunk {
    if strip_existing_ansi {
        AnsiChunk::new(strip_ansi(input))
    } else {
        AnsiChunk::from_slice(input)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn dynamic_profile_observation_reuses_prepared_visible_chunk() {
        let source = include_str!("stream.rs");
        let runtime_source = source.split("#[cfg(test)]").next().unwrap_or(source);

        assert!(!runtime_source.contains("let visible_chunk = strip_ansi(chunk)"));
        assert!(runtime_source.contains("visible_bytes()"));
    }
}
