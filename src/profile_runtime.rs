use crate::profiles::{ProfileStore, is_generic_profile_set};

const OUTPUT_WINDOW_LIMIT: usize = 64 * 1024;
const INPUT_LINE_LIMIT: usize = 4096;
const CLOSE_MARKER_WINDOW_LIMIT: usize = 256;
const DETECTION_BYTE_INTERVAL: usize = 4096;
const PROFILE_STACK_LIMIT: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClosePolicy {
    SshLike,
    Telnet,
    Screen,
    Serial,
    Generic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloseMarkerKind {
    SshLike,
    Telnet,
    Screen,
    Serial,
    Logout,
}

impl ClosePolicy {
    fn accepts(self, marker: CloseMarkerKind) -> bool {
        self == ClosePolicy::Generic
            || marker == CloseMarkerKind::Logout
            || matches!(
                (self, marker),
                (ClosePolicy::SshLike, CloseMarkerKind::SshLike)
                    | (ClosePolicy::Telnet, CloseMarkerKind::Telnet)
                    | (ClosePolicy::Screen, CloseMarkerKind::Screen)
                    | (ClosePolicy::Serial, CloseMarkerKind::Serial)
            )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RemoteAttempt {
    policy: ClosePolicy,
    target: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ObservedCloseMarker {
    kind: CloseMarkerKind,
    target: Option<String>,
}

#[derive(Clone, Debug)]
struct ProfileFrame {
    profiles: Vec<String>,
    close_policy: ClosePolicy,
    target: Option<String>,
    unresolved_detection: bool,
}

#[derive(Clone, Debug)]
struct ProfileTransitionState {
    active_profiles: Vec<String>,
    baseline_profiles: Vec<String>,
    stack: Vec<ProfileFrame>,
    pending_remote: Option<RemoteAttempt>,
    baseline_locked: bool,
    pending_prompt_profiles: Option<Vec<String>>,
    pending_prompt_hits: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum InputEscapeState {
    #[default]
    None,
    Esc,
    Csi {
        parameter: u16,
        numeric: bool,
    },
    Ss3,
}

#[derive(Clone, Debug)]
pub(crate) struct ProfileRuntime {
    active_profiles: Vec<String>,
    baseline_profiles: Vec<String>,
    stack: Vec<ProfileFrame>,
    output_window: String,
    output_window_lower: String,
    output_bytes_since_detection: usize,
    input_line: String,
    input_line_reliable: bool,
    unreliable_remote_attempt: Option<RemoteAttempt>,
    input_escape_state: InputEscapeState,
    pending_remote: Option<RemoteAttempt>,
    baseline_locked: bool,
    pending_prompt_profiles: Option<Vec<String>>,
    pending_prompt_hits: usize,
    transition_rollback: Option<ProfileTransitionState>,
}

impl ProfileRuntime {
    pub(crate) fn new(initial_profiles: Vec<String>) -> Self {
        let baseline_locked = !is_generic_profile_set(&initial_profiles);
        Self {
            active_profiles: initial_profiles.clone(),
            baseline_profiles: initial_profiles,
            stack: Vec::new(),
            output_window: String::new(),
            output_window_lower: String::new(),
            output_bytes_since_detection: 0,
            input_line: String::new(),
            input_line_reliable: true,
            unreliable_remote_attempt: None,
            input_escape_state: InputEscapeState::None,
            pending_remote: None,
            baseline_locked,
            pending_prompt_profiles: None,
            pending_prompt_hits: 0,
            transition_rollback: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn active_profiles(&self) -> Vec<String> {
        self.active_profiles.clone()
    }

    #[cfg(test)]
    pub(crate) fn stack_len(&self) -> usize {
        self.stack.len()
    }

    pub(crate) fn observe_input(&mut self, input: &[u8]) {
        for byte in input {
            match self.input_escape_state {
                InputEscapeState::Esc => {
                    self.input_escape_state = match *byte {
                        b'[' => InputEscapeState::Csi {
                            parameter: 0,
                            numeric: true,
                        },
                        b'O' => InputEscapeState::Ss3,
                        _ => {
                            self.invalidate_input_line();
                            InputEscapeState::None
                        }
                    };
                    continue;
                }
                InputEscapeState::Csi { parameter, numeric } => {
                    if (0x40..=0x7e).contains(byte) {
                        self.input_escape_state = InputEscapeState::None;
                        if !(numeric && *byte == b'~' && matches!(parameter, 200 | 201)) {
                            self.invalidate_input_line();
                        }
                    } else if numeric && byte.is_ascii_digit() {
                        self.input_escape_state = InputEscapeState::Csi {
                            parameter: parameter
                                .saturating_mul(10)
                                .saturating_add(u16::from(*byte - b'0')),
                            numeric: true,
                        };
                    } else {
                        self.input_escape_state = InputEscapeState::Csi {
                            parameter,
                            numeric: false,
                        };
                    }
                    continue;
                }
                InputEscapeState::Ss3 => {
                    self.input_escape_state = InputEscapeState::None;
                    self.invalidate_input_line();
                    continue;
                }
                InputEscapeState::None => {}
            }

            match byte {
                0x1b => self.input_escape_state = InputEscapeState::Esc,
                0x03 => {
                    self.input_line.clear();
                    self.input_line_reliable = true;
                    self.unreliable_remote_attempt = None;
                    self.pending_remote = None;
                }
                0x15 => {
                    self.input_line.clear();
                    self.input_line_reliable = true;
                    self.unreliable_remote_attempt = None;
                }
                0x17 => self.invalidate_input_line(),
                b'\r' | b'\n' => {
                    if self.input_line_reliable {
                        self.submit_input_line();
                    } else if let Some(attempt) = self.unreliable_remote_attempt.take() {
                        self.arm_remote_attempt(Some(attempt));
                    }
                    self.input_line.clear();
                    self.input_line_reliable = true;
                    self.unreliable_remote_attempt = None;
                }
                0x08 | 0x7f => {
                    if self.input_line_reliable {
                        self.input_line.pop();
                    }
                }
                byte if (byte.is_ascii_graphic() || *byte == b' ' || *byte == b'\t')
                    && self.input_line_reliable
                    && self.input_line.len() < INPUT_LINE_LIMIT =>
                {
                    self.input_line.push(*byte as char);
                }
                byte if byte.is_ascii_graphic() || *byte == b' ' || *byte == b'\t' => {
                    self.invalidate_input_line();
                }
                _ => self.invalidate_input_line(),
            }
        }
    }

    fn invalidate_input_line(&mut self) {
        if self.input_line_reliable {
            self.unreliable_remote_attempt = remote_attempt(&self.input_line).map(|mut attempt| {
                attempt.target = None;
                attempt
            });
        }
        self.input_line.clear();
        self.input_line_reliable = false;
    }

    pub(crate) fn observe_output(
        &mut self,
        visible_output: &[u8],
        store: &ProfileStore,
    ) -> Option<Vec<String>> {
        // A caller that did not explicitly reject the previous proposal has
        // accepted it by continuing to feed output. Snapshot only the small
        // profile-state graph (not the 64 KiB detection window) so a failed
        // highlighter build can roll back the proposed transition cheaply.
        self.transition_rollback = None;
        let rollback = self.transition_state();
        let transition = self.observe_output_inner(visible_output, store);
        if transition.is_some() {
            self.transition_rollback = Some(rollback);
        }
        transition
    }

    fn observe_output_inner(
        &mut self,
        visible_output: &[u8],
        store: &ProfileStore,
    ) -> Option<Vec<String>> {
        let text = String::from_utf8_lossy(visible_output);
        if text.is_empty() {
            return None;
        }

        self.output_window.push_str(&text);
        self.output_window_lower
            .push_str(&text.to_ascii_lowercase());
        trim_to_recent_chars(&mut self.output_window, OUTPUT_WINDOW_LIMIT);
        trim_to_recent_chars(&mut self.output_window_lower, OUTPUT_WINDOW_LIMIT);
        self.output_bytes_since_detection =
            self.output_bytes_since_detection.saturating_add(text.len());

        let close_markers = observed_close_markers(recent_complete_lines(
            &self.output_window_lower,
            CLOSE_MARKER_WINDOW_LIMIT,
        ));
        if !close_markers.is_empty() {
            let pending_match = self.pending_remote.as_ref().and_then(|pending| {
                close_markers
                    .iter()
                    .find(|marker| close_marker_matches(marker, pending.policy, &pending.target))
            });
            let top_match = self.stack.last().and_then(|frame| {
                close_markers
                    .iter()
                    .find(|marker| close_marker_matches(marker, frame.close_policy, &frame.target))
            });
            let top_specific_match = self.stack.last().and_then(|frame| {
                close_markers.iter().find(|marker| {
                    marker.target.is_some()
                        && close_marker_matches(marker, frame.close_policy, &frame.target)
                        && !self.pending_remote.as_ref().is_some_and(|pending| {
                            pending.target.is_some()
                                && close_marker_matches(marker, pending.policy, &pending.target)
                        })
                })
            });

            if top_specific_match.is_some() || (pending_match.is_none() && top_match.is_some()) {
                self.pending_remote = None;
                self.clear_output_window();
                self.clear_pending_prompt();
                return self.pop_profile();
            }
            if pending_match.is_some() {
                self.pending_remote = None;
                self.clear_output_window();
                self.clear_pending_prompt();
                return None;
            }
            if top_match.is_some() {
                self.pending_remote = None;
                self.clear_output_window();
                self.clear_pending_prompt();
                return self.pop_profile();
            }
        }

        if self.pending_remote.as_ref().is_some_and(|pending| {
            contains_remote_open_failure(&self.output_window_lower, pending.policy)
        }) {
            self.pending_remote = None;
            self.clear_output_window();
            self.clear_pending_prompt();
            return None;
        }

        if !self.should_detect_profiles(&text) {
            return None;
        }
        self.output_bytes_since_detection = 0;

        let detected =
            store.detect_profiles_with_lowercase(&self.output_window, &self.output_window_lower);
        if is_generic_profile_set(&detected) {
            self.clear_pending_prompt();
            return None;
        }
        if detected == self.active_profiles {
            if let Some(remote_attempt) = self.pending_remote.take() {
                let profiles = self.active_profiles.clone();
                self.clear_output_window();
                self.clear_pending_prompt();
                let _ = self.switch_to(profiles, Some(remote_attempt), false);
            }
            self.clear_pending_prompt();
            return None;
        }

        if self.should_learn_baseline(&detected, store) {
            self.baseline_profiles = detected.clone();
            self.active_profiles = detected.clone();
            self.baseline_locked = true;
            self.clear_output_window();
            self.clear_pending_prompt();
            return Some(detected);
        }

        let active_profile = store.active_specific_profile(&self.active_profiles);
        let at_local_baseline = self.at_local_baseline(store);
        if let Some(profile) =
            store.strong_transition_profile(&detected, &self.output_window, active_profile)
        {
            let remote_attempt = self.pending_remote.take();
            self.clear_output_window();
            self.clear_pending_prompt();
            return self.switch_to(profile_set(&profile), remote_attempt, at_local_baseline);
        }

        let unresolved_remote = self
            .stack
            .last()
            .is_some_and(|frame| frame.unresolved_detection);
        let has_remote_hint = self.pending_remote.is_some() || unresolved_remote;
        if has_remote_hint || active_profile.is_none() || at_local_baseline {
            if let Some(profile) =
                store.prompt_transition_profile(&detected, &self.output_window, active_profile)
            {
                if store.prompt_switches_on_first_detection(
                    &profile,
                    has_remote_hint,
                    at_local_baseline,
                ) {
                    let remote_attempt = self.pending_remote.take();
                    self.clear_output_window();
                    self.clear_pending_prompt();
                    return self.switch_to(
                        profile_set(&profile),
                        remote_attempt,
                        at_local_baseline,
                    );
                }
                return self.note_prompt_detection(profile_set(&profile), at_local_baseline);
            }
        }

        if active_profile.is_some() {
            self.clear_output_window();
        }
        self.clear_pending_prompt();
        None
    }

    pub(crate) fn accept_transition(&mut self) {
        self.transition_rollback = None;
    }

    pub(crate) fn reject_transition(&mut self) {
        let Some(rollback) = self.transition_rollback.take() else {
            return;
        };
        let rejected_remote = rollback.pending_remote;
        self.active_profiles = rollback.active_profiles;
        self.baseline_profiles = rollback.baseline_profiles;
        self.stack = rollback.stack;
        self.pending_remote = None;
        self.baseline_locked = rollback.baseline_locked;
        self.pending_prompt_profiles = rollback.pending_prompt_profiles;
        self.pending_prompt_hits = rollback.pending_prompt_hits;
        if let Some(rejected_remote) = rejected_remote {
            // A transition banner confirms this nested connection exists even
            // though its highlighter could not be built. Keep a no-op frame
            // with the previous active profiles so this connection's close is
            // accounted for independently of its parent and later children.
            self.push_frame_with_detection_state(rejected_remote, true);
        }
        // Do not repeatedly reconsider the same hostile banner or failed
        // command after its profile set was rejected by the compiler.
        self.clear_output_window();
        self.clear_pending_prompt();
    }

    pub(crate) fn profile_sets_for_reload(&self) -> Vec<Vec<String>> {
        let mut profile_sets = vec![self.baseline_profiles.clone()];
        for frame in &self.stack {
            if !profile_sets.contains(&frame.profiles) {
                profile_sets.push(frame.profiles.clone());
            }
        }
        profile_sets
    }

    pub(crate) fn accept_reload_generation(&mut self) {
        // The session validated and cached every hidden profile set before it
        // committed the new store/config generation. Preserve exact nesting,
        // pending remote evidence, and baseline locking; only discard detection
        // bytes and a rollback proposal produced under the old generation.
        self.transition_rollback = None;
        self.clear_output_window();
        self.clear_pending_prompt();
    }

    fn transition_state(&self) -> ProfileTransitionState {
        ProfileTransitionState {
            active_profiles: self.active_profiles.clone(),
            baseline_profiles: self.baseline_profiles.clone(),
            stack: self.stack.clone(),
            pending_remote: self.pending_remote.clone(),
            baseline_locked: self.baseline_locked,
            pending_prompt_profiles: self.pending_prompt_profiles.clone(),
            pending_prompt_hits: self.pending_prompt_hits,
        }
    }

    fn submit_input_line(&mut self) {
        let trimmed = self.input_line.trim_start();
        self.arm_remote_attempt(remote_attempt(trimmed));
    }

    fn arm_remote_attempt(&mut self, attempt: Option<RemoteAttempt>) {
        self.pending_remote = attempt;
        if self.pending_remote.is_some() {
            self.baseline_locked = true;
            self.clear_output_window();
            self.clear_pending_prompt();
        }
    }

    fn should_detect_profiles(&self, text: &str) -> bool {
        self.pending_remote.is_some()
            || self.output_bytes_since_detection >= DETECTION_BYTE_INTERVAL
            || text.bytes().any(|byte| matches!(byte, b'\r' | b'\n'))
            || looks_like_prompt_detection_boundary(text)
    }

    fn at_local_baseline(&self, store: &ProfileStore) -> bool {
        self.stack.is_empty()
            && self.active_profiles == self.baseline_profiles
            && store.profiles_are_local_baseline(&self.active_profiles)
    }

    fn should_learn_baseline(&self, detected: &[String], store: &ProfileStore) -> bool {
        !self.baseline_locked
            && self.stack.is_empty()
            && is_generic_profile_set(&self.active_profiles)
            && !is_generic_profile_set(detected)
            && store.profiles_are_local_baseline(detected)
    }

    fn note_prompt_detection(
        &mut self,
        detected: Vec<String>,
        at_local_baseline: bool,
    ) -> Option<Vec<String>> {
        if self
            .pending_prompt_profiles
            .as_ref()
            .is_some_and(|profiles| profiles == &detected)
        {
            self.pending_prompt_hits += 1;
        } else {
            self.pending_prompt_profiles = Some(detected.clone());
            self.pending_prompt_hits = 1;
        }

        if self.pending_prompt_hits >= 2 {
            let remote_attempt = self.pending_remote.take();
            self.clear_output_window();
            self.clear_pending_prompt();
            self.switch_to(detected, remote_attempt, at_local_baseline)
        } else {
            None
        }
    }

    fn switch_to(
        &mut self,
        profiles: Vec<String>,
        remote_attempt: Option<RemoteAttempt>,
        open_baseline_frame: bool,
    ) -> Option<Vec<String>> {
        if is_generic_profile_set(&profiles) {
            return None;
        }
        if remote_attempt.is_none() {
            if let Some(frame) = self.stack.last_mut() {
                frame.unresolved_detection = false;
            }
        }
        if let Some(remote_attempt) = remote_attempt {
            self.push_frame(remote_attempt);
            let changed = profiles != self.active_profiles;
            self.active_profiles = profiles;
            return changed.then(|| self.active_profiles.clone());
        }
        if profiles == self.active_profiles {
            return None;
        }
        if let Some(stack_index) = self
            .stack
            .iter()
            .position(|frame| frame.profiles == profiles)
        {
            self.stack.truncate(stack_index);
            self.active_profiles = profiles;
            return Some(self.active_profiles.clone());
        }
        if profiles == self.baseline_profiles {
            self.stack.clear();
            self.active_profiles = self.baseline_profiles.clone();
            return Some(self.active_profiles.clone());
        }

        if open_baseline_frame {
            self.push_frame(RemoteAttempt {
                policy: ClosePolicy::Generic,
                target: None,
            });
        }

        // A new strong banner without a newly observed remote command is a
        // refinement of the current connection, not another nested session.
        self.active_profiles = profiles;
        Some(self.active_profiles.clone())
    }

    fn pop_profile(&mut self) -> Option<Vec<String>> {
        let next = self
            .stack
            .pop()
            .map(|frame| frame.profiles)
            .unwrap_or_else(|| self.baseline_profiles.clone());
        if next == self.active_profiles {
            None
        } else {
            self.active_profiles = next;
            Some(self.active_profiles.clone())
        }
    }

    fn push_frame(&mut self, remote_attempt: RemoteAttempt) {
        self.push_frame_with_detection_state(remote_attempt, false);
    }

    fn push_frame_with_detection_state(
        &mut self,
        remote_attempt: RemoteAttempt,
        unresolved_detection: bool,
    ) {
        if self.stack.len() >= PROFILE_STACK_LIMIT {
            // The first frame is the route back to the local baseline. Preserve
            // it and collapse the oldest interior nesting level instead.
            self.stack.remove(1);
        }
        self.stack.push(ProfileFrame {
            profiles: self.active_profiles.clone(),
            close_policy: remote_attempt.policy,
            target: remote_attempt.target,
            unresolved_detection,
        });
    }

    fn clear_pending_prompt(&mut self) {
        self.pending_prompt_profiles = None;
        self.pending_prompt_hits = 0;
    }

    fn clear_output_window(&mut self) {
        self.output_window.clear();
        self.output_window_lower.clear();
        self.output_bytes_since_detection = 0;
    }
}

fn profile_set(profile: &str) -> Vec<String> {
    vec!["generic".to_string(), profile.to_string()]
}

fn remote_attempt(line: &str) -> Option<RemoteAttempt> {
    let words = split_shell_words(line);
    let command = words
        .as_ref()
        .and_then(|words| words.first())
        .map(|word| word.as_str())
        .or_else(|| line.split_whitespace().next())?
        .to_ascii_lowercase();
    let policy = match command.as_str() {
        "ssh" | "mosh" => ClosePolicy::SshLike,
        "telnet" => ClosePolicy::Telnet,
        "screen" => ClosePolicy::Screen,
        "cu" | "minicom" | "picocom" => ClosePolicy::Serial,
        _ => return None,
    };
    let Some(words) = words else {
        return Some(RemoteAttempt {
            policy,
            target: None,
        });
    };
    let mut skip_option_value = false;
    let mut options_ended = false;
    let mut target = None;
    for word in words.iter().skip(1) {
        if skip_option_value {
            skip_option_value = false;
            continue;
        }
        if !options_ended && word == "--" {
            options_ended = true;
            continue;
        }
        if !options_ended && word.starts_with('-') {
            skip_option_value = remote_option_takes_value(&command, word);
            continue;
        }
        if is_literal_remote_target(word) {
            target = Some(normalize_remote_target(word));
        }
        break;
    }
    Some(RemoteAttempt { policy, target })
}

fn split_shell_words(line: &str) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = Quote::None;
    let mut started = false;
    let mut characters = line.chars();
    while let Some(character) = characters.next() {
        match quote {
            Quote::None if character.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            Quote::None if character == '\'' => {
                quote = Quote::Single;
                started = true;
            }
            Quote::None if character == '"' => {
                quote = Quote::Double;
                started = true;
            }
            Quote::None if character == '\\' => {
                word.push(characters.next()?);
                started = true;
            }
            Quote::Single if character == '\'' => quote = Quote::None,
            Quote::Double if character == '"' => quote = Quote::None,
            Quote::Double if character == '\\' => {
                word.push(characters.next()?);
                started = true;
            }
            _ => {
                word.push(character);
                started = true;
            }
        }
    }
    if quote != Quote::None {
        return None;
    }
    if started {
        words.push(word);
    }
    Some(words)
}

fn is_literal_remote_target(word: &str) -> bool {
    !word.starts_with('~')
        && !word
            .chars()
            .any(|character| matches!(character, '$' | '`' | '*' | '?' | '[' | ']'))
}

fn remote_option_takes_value(command: &str, option: &str) -> bool {
    if remote_option_word_takes_value(command, option) {
        return true;
    }
    // Combined getopt-style short flags (`-4p 2222`, `-vp 2222`): the first
    // flag that takes a value consumes the rest of the word as an attached
    // value (`-p2222`), so only a value-taking flag in the last position
    // leaves its value to the next word.
    let Some(flags) = option.strip_prefix('-') else {
        return false;
    };
    if flags.starts_with('-') || flags.chars().count() < 2 {
        return false;
    }
    let mut flags = flags.chars().peekable();
    while let Some(flag) = flags.next() {
        if remote_option_word_takes_value(command, &format!("-{flag}")) {
            return flags.peek().is_none();
        }
    }
    false
}

fn remote_option_word_takes_value(command: &str, option: &str) -> bool {
    match command {
        "ssh" => matches!(
            option,
            "-B" | "-b"
                | "-c"
                | "-D"
                | "-E"
                | "-e"
                | "-F"
                | "-I"
                | "-i"
                | "-J"
                | "-L"
                | "-l"
                | "-m"
                | "-O"
                | "-o"
                | "-P"
                | "-p"
                | "-Q"
                | "-R"
                | "-S"
                | "-W"
                | "-w"
        ),
        "mosh" => matches!(option, "-p" | "--port" | "--ssh"),
        "telnet" => matches!(option, "-l"),
        "screen" => matches!(option, "-c" | "-d" | "-m" | "-S" | "-T"),
        "cu" => matches!(option, "-l" | "-s"),
        "minicom" => matches!(option, "-D" | "-b" | "-c" | "-t"),
        "picocom" => matches!(option, "-b" | "--baud" | "--imap" | "--omap" | "--emap"),
        _ => false,
    }
}

fn normalize_remote_target(target: &str) -> String {
    target
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(target)
        .trim_matches(['[', ']'])
        .to_ascii_lowercase()
}

fn trim_to_recent_chars(text: &mut String, limit: usize) {
    if text.len() <= limit {
        return;
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text.drain(..start);
}

fn observed_close_markers(text: &str) -> Vec<ObservedCloseMarker> {
    // Anchored to whole-line teardown shapes so benign output that merely
    // mentions these words (e.g. a log line "... connection to X closed and
    // reopened") does not pop a still-live remote profile. The caller passes
    // lowercased text.
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line == "logout" {
                Some(ObservedCloseMarker {
                    kind: CloseMarkerKind::Logout,
                    target: None,
                })
            } else if matches!(
                line,
                "connection closed"
                    | "connection closed."
                    | "connection closed by remote host"
                    | "connection closed by remote host."
            ) {
                Some(ObservedCloseMarker {
                    kind: CloseMarkerKind::SshLike,
                    target: None,
                })
            } else if let Some(target) = ssh_close_target(line) {
                Some(ObservedCloseMarker {
                    kind: CloseMarkerKind::SshLike,
                    target: Some(normalize_remote_target(target)),
                })
            } else if matches!(
                line,
                "connection closed by foreign host" | "connection closed by foreign host."
            ) {
                Some(ObservedCloseMarker {
                    kind: CloseMarkerKind::Telnet,
                    target: None,
                })
            } else if line == "[screen is terminating]"
                || (line.starts_with("[detached from ") && line.ends_with(']'))
            {
                Some(ObservedCloseMarker {
                    kind: CloseMarkerKind::Screen,
                    target: None,
                })
            } else if matches!(
                line,
                "disconnected"
                    | "disconnected."
                    | "leaving minicom.."
                    | "terminating..."
                    | "thanks for using picocom"
            ) {
                Some(ObservedCloseMarker {
                    kind: CloseMarkerKind::Serial,
                    target: None,
                })
            } else {
                None
            }
        })
        .collect()
}

fn ssh_close_target(line: &str) -> Option<&str> {
    let remainder = line.strip_prefix("connection to ")?;
    for suffix in [
        " closed by remote host.",
        " closed by remote host",
        " closed.",
        " closed",
    ] {
        if let Some(target) = remainder.strip_suffix(suffix) {
            return (!target.is_empty()).then_some(target);
        }
    }
    None
}

fn close_marker_matches(
    marker: &ObservedCloseMarker,
    policy: ClosePolicy,
    expected_target: &Option<String>,
) -> bool {
    if !policy.accepts(marker.kind) {
        return false;
    }
    match (&marker.target, expected_target) {
        (Some(observed), Some(expected)) => observed == expected,
        _ => true,
    }
}

#[cfg(test)]
fn contains_close_marker(text: &str) -> bool {
    !observed_close_markers(text).is_empty()
}

fn contains_remote_open_failure(text: &str, policy: ClosePolicy) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        match policy {
            ClosePolicy::SshLike => {
                line.starts_with("ssh: could not resolve hostname ")
                    || ((line.starts_with("permission denied (")
                        || line.contains(": permission denied ("))
                        && line.ends_with(")."))
                    || line == "host key verification failed."
                    || line == "warning: remote host identification has changed!"
                    || line
                        .strip_prefix("kex_exchange_identification: ")
                        .is_some_and(|failure| {
                            matches!(
                                failure,
                                "read: connection reset by peer" | "connection reset by peer"
                            )
                        })
                    || (line.starts_with("ssh: connect to host ")
                        && [
                            "connection refused",
                            "connection timed out",
                            "network is unreachable",
                            "no route to host",
                            "operation timed out",
                        ]
                        .iter()
                        .any(|failure| line.ends_with(failure)))
            }
            ClosePolicy::Telnet => {
                line.starts_with("telnet: unable to connect to remote host:")
                    || line.starts_with("telnet: could not resolve ")
            }
            ClosePolicy::Screen => {
                line.starts_with("cannot open your terminal") || line.starts_with("cannot exec ")
            }
            ClosePolicy::Serial => {
                line.starts_with("fatal: cannot open ")
                    || line.starts_with("minicom: cannot open ")
                    || line.starts_with("cu: open (")
            }
            ClosePolicy::Generic => false,
        }
    })
}

fn looks_like_prompt_detection_boundary(text: &str) -> bool {
    let trimmed = text.trim_end();
    matches!(trimmed.as_bytes().last(), Some(b'#' | b'>' | b'$' | b'%'))
}

fn recent_chars(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// The most recent terminated lines of `text`, at most `limit` bytes.
///
/// A trailing unterminated line is excluded: a read boundary can split
/// `Connection to X closed by administrator.` right after `closed`, and that
/// prefix must not count as a teardown marker before the line is complete.
/// A leading line cut by the byte limit is excluded for the same reason.
fn recent_complete_lines(text: &str, limit: usize) -> &str {
    let Some(last_newline) = text.rfind('\n') else {
        return "";
    };
    let terminated = &text[..=last_newline];
    if terminated.len() <= limit {
        return terminated;
    }
    let recent = recent_chars(terminated, limit);
    let start = terminated.len() - recent.len();
    if start == 0 || terminated.as_bytes()[start - 1] == b'\n' {
        return recent;
    }
    recent
        .split_once('\n')
        .map_or("", |(_, complete_lines)| complete_lines)
}

#[cfg(test)]
mod tests {
    use super::ProfileRuntime;
    use crate::profiles::ProfileStore;

    const VENDOR_NAMES: &[&str] = &[
        "arista",
        "arubacx",
        "cisco",
        "fortinet",
        "juniper",
        "linux-unix",
        "palo-alto",
        "versa",
    ];

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn runtime_source_has_no_static_vendor_priority_array() {
        let source = include_str!("profile_runtime.rs");
        let runtime_source = source.split("#[cfg(test)]").next().unwrap_or(source);

        assert!(
            !runtime_source.contains("PROFILE_PRIORITY"),
            "profile_runtime.rs must get vendor priority from ProfileStore metadata, not a static array"
        );
    }

    #[test]
    fn runtime_source_has_no_vendor_string_match_arms() {
        let source = include_str!("profile_runtime.rs");
        let runtime_source = source.split("#[cfg(test)]").next().unwrap_or(source);

        for vendor in VENDOR_NAMES {
            let match_arm = format!("\"{vendor}\" =>");
            assert!(
                !runtime_source.contains(&match_arm),
                "profile_runtime.rs must not dispatch vendor behavior with a hardcoded {match_arm} arm"
            );
            let match_or_pattern = format!("| \"{vendor}\"");
            assert!(
                !runtime_source.contains(&match_or_pattern),
                "profile_runtime.rs must not dispatch vendor behavior with a hardcoded {match_or_pattern} pattern"
            );
            let leading_match_or_pattern = format!("\"{vendor}\" |");
            assert!(
                !runtime_source.contains(&leading_match_or_pattern),
                "profile_runtime.rs must not dispatch vendor behavior with a hardcoded {leading_match_or_pattern} pattern"
            );
            let equality = format!("== \"{vendor}\"");
            assert!(
                !runtime_source.contains(&equality),
                "profile_runtime.rs must not dispatch vendor behavior with a hardcoded {equality} comparison"
            );
        }
    }

    #[test]
    fn learns_linux_baseline_from_local_shell_output() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic"]));

        let changed = runtime.observe_output(
            b"OS: Ubuntu 24.04.4 LTS\nKernel: Linux 6.8.0\nTerminal: /dev/pts/1\n",
            &store,
        );

        assert_eq!(changed, Some(names(&["generic", "linux-unix"])));
        assert_eq!(runtime.active_profiles(), names(&["generic", "linux-unix"]));
    }

    #[test]
    fn promotes_to_remote_vendor_after_ssh_input_hint() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh router-a\r");
        let changed = runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store);

        assert_eq!(changed, Some(names(&["generic", "juniper"])));
        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));
    }

    #[test]
    fn strong_banner_cycle_unwinds_existing_profiles_instead_of_growing_stack() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh router-a\r");
        assert_eq!(
            runtime.observe_output(b"Cisco IOS Software\n", &store),
            Some(names(&["generic", "cisco"]))
        );
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
            Some(names(&["generic", "juniper"]))
        );
        assert_eq!(
            runtime.observe_output(b"Version: FortiGate v7.2.8\n", &store),
            Some(names(&["generic", "fortinet"]))
        );

        for _ in 0..4 {
            assert_eq!(
                runtime.observe_output(b"Cisco IOS Software\n", &store),
                Some(names(&["generic", "cisco"]))
            );
            assert_eq!(
                runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
                Some(names(&["generic", "juniper"]))
            );
            assert_eq!(
                runtime.observe_output(b"Version: FortiGate v7.2.8\n", &store),
                Some(names(&["generic", "fortinet"]))
            );
        }

        assert!(
            runtime.stack_len() <= 3,
            "cycling through known strong profiles should not grow the profile stack without bound"
        );
    }

    #[test]
    fn dynamic_detection_defers_small_banner_chunks_until_newline() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        assert_eq!(
            runtime.observe_output(b"Cisco Nexus Operating System (NX-OS) Software", &store),
            None
        );
        assert_eq!(runtime.active_profiles(), names(&["generic", "linux-unix"]));

        assert_eq!(
            runtime.observe_output(b"\n", &store),
            Some(names(&["generic", "cisco"]))
        );
        assert_eq!(runtime.active_profiles(), names(&["generic", "cisco"]));
    }

    #[test]
    fn oversized_input_line_is_invalidated_until_newline() {
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(&vec![b'a'; 8192]);

        assert!(runtime.input_line.is_empty());
        assert!(!runtime.input_line_reliable);
        runtime.observe_input(b"\rssh router-a\r");
        assert!(runtime.input_line_reliable);
        assert_eq!(
            runtime
                .pending_remote
                .as_ref()
                .and_then(|attempt| attempt.target.as_deref()),
            Some("router-a")
        );
    }

    // getopt-style commands accept combined short flags (`-4p 2222`) and
    // attached values (`-p2222`); the close target must still be the host.
    #[test]
    fn combined_short_options_do_not_shift_the_remote_target() {
        for line in [
            "ssh -4p 2222 router-a\r",
            "ssh -vp 2222 router-a\r",
            "ssh -p2222 router-a\r",
            "ssh -4p2222 router-a\r",
            "ssh -46 router-a\r",
        ] {
            let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
            runtime.observe_input(line.as_bytes());
            assert_eq!(
                runtime
                    .pending_remote
                    .as_ref()
                    .and_then(|attempt| attempt.target.as_deref()),
                Some("router-a"),
                "{line:?}"
            );
        }
    }

    #[test]
    fn split_close_marker_pops_remote_profile() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh router-a\r");
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
            Some(names(&["generic", "juniper"]))
        );

        assert_eq!(runtime.observe_output(b"Connection clo", &store), None);
        let changed = runtime.observe_output(b"sed\r\n", &store);

        assert_eq!(changed, Some(names(&["generic", "linux-unix"])));
        assert_eq!(runtime.active_profiles(), names(&["generic", "linux-unix"]));
    }

    // A read boundary can split a benign line right after a marker-shaped
    // prefix; only terminated lines may count as teardown markers.
    #[test]
    fn partial_trailing_line_is_not_a_close_marker() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh router-a\r");
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
            Some(names(&["generic", "juniper"]))
        );

        assert_eq!(
            runtime.observe_output(b"Connection to router-a closed", &store),
            None
        );
        assert_eq!(
            runtime.observe_output(b" by administrator.\r\n", &store),
            None
        );
        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));

        // The genuine marker still pops once its line is complete.
        assert_eq!(
            runtime.observe_output(b"Connection to router-a closed.\r\n", &store),
            Some(names(&["generic", "linux-unix"]))
        );
    }

    #[test]
    fn unsolicited_close_marker_does_not_change_an_unarmed_profile() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        runtime.switch_to(names(&["generic", "juniper"]), None, false);

        let changed = runtime.observe_output(b"Connection to unrelated-host closed.\n", &store);

        assert_eq!(changed, None);
        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));
        assert_eq!(runtime.stack_len(), 0);
    }

    // Only a teardown-shaped line should count as a close marker; benign output
    // that merely contains the words must not pop the profile.
    #[test]
    fn close_marker_requires_a_teardown_shaped_line() {
        assert!(super::contains_close_marker(
            "connection to 192.0.2.5 closed.\n"
        ));
        assert!(super::contains_close_marker(
            "connection closed by remote host\r\n"
        ));
        assert!(super::contains_close_marker("logout\n"));

        assert!(!super::contains_close_marker(
            "apr 01 connection to 192.0.2.5 closed and reopened\n"
        ));
        assert!(!super::contains_close_marker(
            "the connection closed flag is documented here\n"
        ));
        assert!(!super::contains_close_marker(
            "connection closed flag is false\n"
        ));
    }

    #[test]
    fn remote_hint_promotes_fortinet_on_first_prompt() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh firewall-a\r");
        let changed = runtime.observe_output(b"FW-EDGE # ", &store);

        assert_eq!(changed, Some(names(&["generic", "fortinet"])));
        assert_eq!(runtime.active_profiles(), names(&["generic", "fortinet"]));
        assert_eq!(runtime.stack_len(), 1);
        assert_eq!(
            runtime.observe_output(b"Connection to firewall-a closed.\n", &store),
            Some(names(&["generic", "linux-unix"]))
        );
    }

    #[test]
    fn local_shell_wrapper_command_promotes_fortinet_on_first_prompt() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"fw\r");
        let changed = runtime.observe_output(b"FW-EDGE # ", &store);

        assert_eq!(changed, Some(names(&["generic", "fortinet"])));
        assert_eq!(runtime.active_profiles(), names(&["generic", "fortinet"]));
        assert_eq!(runtime.stack_len(), 1);
        assert_eq!(
            runtime.observe_output(b"Connection to fw closed.\n", &store),
            Some(names(&["generic", "linux-unix"]))
        );
    }

    #[test]
    fn promotes_all_builtin_remote_profiles_from_strong_banners() {
        let store = ProfileStore::builtin();

        for (sample, expected) in [
            (
                "--- JUNOS 22.4R3 Kernel 64-bit\n",
                names(&["generic", "juniper"]),
            ),
            (
                "Cisco Nexus Operating System (NX-OS) Software\n",
                names(&["generic", "cisco"]),
            ),
            (
                "Version: FortiGate-VM64 v7.4\n",
                names(&["generic", "fortinet"]),
            ),
            ("ArubaOS-CX Version 10.13\n", names(&["generic", "arubacx"])),
            (
                "Arista Networks EOS version 4.31\n",
                names(&["generic", "arista"]),
            ),
            ("PAN-OS 11.1\n", names(&["generic", "palo-alto"])),
            ("Versa Director 22.1\n", names(&["generic", "versa"])),
        ] {
            let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
            assert_eq!(
                runtime.observe_output(sample.as_bytes(), &store),
                Some(expected)
            );
            assert_eq!(runtime.stack_len(), 1);
            assert_eq!(
                runtime.observe_output(b"Connection closed.\n", &store),
                Some(names(&["generic", "linux-unix"]))
            );
        }
    }

    #[test]
    fn weak_generic_output_does_not_downgrade_remote_profile() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "juniper"]));

        let changed = runtime.observe_output(b"show configuration | display set\n", &store);

        assert_eq!(changed, None);
        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));
    }

    #[test]
    fn active_remote_ignores_weak_command_output_from_other_profiles() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh router-a\r");
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
            Some(names(&["generic", "juniper"]))
        );

        let changed = runtime.observe_output(
            b"show interfaces descriptions\nshow ip route\nrouter ospf 1\ndiagnose debug flow\n",
            &store,
        );

        assert_eq!(changed, None);
        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));
    }

    #[test]
    fn active_remote_does_not_accumulate_multiple_specific_profiles() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh router-a\r");
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\nlabuser@mx480>\n", &store,),
            Some(names(&["generic", "juniper"]))
        );

        for chunk in [
            b"labuser@mx480> show interfaces descriptions\nshow ip route\n".as_slice(),
            b"labuser@mx480> show interfaces descriptions\nshow ip route\n".as_slice(),
        ] {
            assert_eq!(runtime.observe_output(chunk, &store), None);
        }

        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));
    }

    #[test]
    fn locked_remote_requires_remote_hint_before_prompt_based_switch() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "juniper"]));

        assert_eq!(runtime.observe_output(b"CoreSW#\n", &store), None);
        assert_eq!(runtime.observe_output(b"CoreSW#\n", &store), None);
        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));

        runtime.observe_input(b"ssh core-sw\r");
        assert_eq!(runtime.observe_output(b"CoreSW#\n", &store), None);
        assert_eq!(
            runtime.observe_output(b"CoreSW#\n", &store),
            Some(names(&["generic", "cisco"]))
        );
    }

    #[test]
    fn command_output_words_do_not_arm_remote_candidate() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "cisco"]));

        assert_eq!(runtime.observe_output(b"cu\n", &store), None);
        assert_eq!(runtime.observe_output(b"FW-EDGE #\n", &store), None);
        assert_eq!(runtime.observe_output(b"FW-EDGE #\n", &store), None);
        assert_eq!(runtime.active_profiles(), names(&["generic", "cisco"]));
    }

    #[test]
    fn active_cisco_ignores_fortinet_words_in_interface_descriptions() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "cisco"]));

        let changed = runtime.observe_output(
            b"Eth1/46       eth  40G     [VPC37] FortiGate firewall uplink Eth1/49\n",
            &store,
        );

        assert_eq!(changed, None);
        assert_eq!(runtime.active_profiles(), names(&["generic", "cisco"]));
    }

    #[test]
    fn nested_remote_close_pops_to_previous_profile() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh router-a\r");
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
            Some(names(&["generic", "juniper"]))
        );
        runtime.observe_input(b"ssh core-sw\r");
        assert_eq!(
            runtime.observe_output(b"Cisco Nexus Operating System (NX-OS) Software\n", &store),
            Some(names(&["generic", "cisco"]))
        );

        assert_eq!(
            runtime.observe_output(b"Connection to core-sw closed.\n", &store),
            Some(names(&["generic", "juniper"]))
        );
        assert_eq!(
            runtime.observe_output(b"Connection to router-a closed.\n", &store),
            Some(names(&["generic", "linux-unix"]))
        );
    }

    #[test]
    fn one_remote_close_unwinds_all_banner_refinements_for_that_connection() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh router-a\r");
        assert_eq!(
            runtime.observe_output(b"Cisco IOS Software\n", &store),
            Some(names(&["generic", "cisco"]))
        );
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
            Some(names(&["generic", "juniper"]))
        );
        assert_eq!(runtime.stack_len(), 1);

        assert_eq!(
            runtime.observe_output(b"Connection to router-a closed.\n", &store),
            Some(names(&["generic", "linux-unix"]))
        );
        assert_eq!(runtime.stack_len(), 0);
    }

    #[test]
    fn failed_nested_remote_attempt_does_not_pop_the_parent_connection() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh router-a\r");
        runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store);
        runtime.observe_input(b"ssh missing-host\r");

        assert_eq!(
            runtime.observe_output(b"Connection to missing-host closed.\n", &store),
            None
        );
        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));
        assert_eq!(runtime.stack_len(), 1);
    }

    #[test]
    fn edited_nested_attempt_does_not_mask_a_targeted_parent_close() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh router-a\r");
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
            Some(names(&["generic", "juniper"]))
        );
        runtime.accept_transition();
        runtime.observe_input(b"ssh hot\x1b[Ds\r");

        assert_eq!(
            runtime.observe_output(b"Connection to router-a closed.\n", &store),
            Some(names(&["generic", "linux-unix"]))
        );
        assert_eq!(runtime.pending_remote, None);
        assert_eq!(runtime.stack_len(), 0);
    }

    #[test]
    fn failed_remote_open_followed_by_local_prompt_does_not_create_a_frame() {
        let store = ProfileStore::builtin();

        for failure in [
            "ssh: Could not resolve hostname missing: nodename nor servname provided\nuser@local$ ",
            "ssh: connect to host 192.0.2.10 port 22: Connection refused\nuser@local$ ",
            "user@missing: Permission denied (publickey,password).\nuser@local$ ",
            "Host key verification failed.\nuser@local$ ",
            "kex_exchange_identification: read: Connection reset by peer\nuser@local$ ",
        ] {
            let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
            runtime.observe_input(b"ssh missing\r");

            assert_eq!(runtime.observe_output(failure.as_bytes(), &store), None);
            assert_eq!(runtime.active_profiles(), names(&["generic", "linux-unix"]));
            assert_eq!(runtime.stack_len(), 0);
            assert!(runtime.at_local_baseline(&store));

            assert_eq!(
                runtime.observe_output(b"FW-EDGE # ", &store),
                Some(names(&["generic", "fortinet"]))
            );
        }
    }

    #[test]
    fn close_marker_prose_does_not_pop_an_established_remote_profile() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        runtime.observe_input(b"ssh router-a\r");
        runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store);

        assert_eq!(
            runtime.observe_output(b"Connection closed flag is false\n", &store),
            None
        );
        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));
        assert_eq!(runtime.stack_len(), 1);
        assert_eq!(
            runtime.observe_output(b"warning: channel was not closed by remote host.\n", &store),
            None
        );
        assert_eq!(runtime.stack_len(), 1);
    }

    #[test]
    fn truncated_close_marker_window_does_not_turn_prose_into_a_teardown_line() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        runtime.observe_input(b"ssh router-a\r");
        runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store);

        let marker = "connection closed";
        let tail = "x".repeat(super::CLOSE_MARKER_WINDOW_LIMIT - marker.len() - 1);
        let output = format!("status message says {marker}\n{tail}");
        assert_eq!(runtime.observe_output(output.as_bytes(), &store), None);
        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));
        assert_eq!(runtime.stack_len(), 1);
    }

    #[test]
    fn supported_remote_commands_unwind_on_their_protocol_teardown_marker() {
        let store = ProfileStore::builtin();
        for (command, marker) in [
            ("ssh router", "Connection to router closed.\n"),
            ("mosh router", "Connection closed.\n"),
            ("telnet router", "Connection closed by foreign host.\n"),
            ("screen /dev/ttyUSB0", "[screen is terminating]\n"),
            ("cu -l /dev/ttyUSB0", "Disconnected.\n"),
            ("minicom router", "Leaving Minicom..\n"),
            ("picocom /dev/ttyUSB0", "Terminating...\n"),
        ] {
            let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
            runtime.observe_input(format!("{command}\r").as_bytes());
            assert_eq!(
                runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
                Some(names(&["generic", "juniper"]))
            );
            assert_eq!(
                runtime.observe_output(marker.as_bytes(), &store),
                Some(names(&["generic", "linux-unix"])),
                "{command} did not unwind on {marker:?}"
            );
        }
    }

    #[test]
    fn ssh_option_values_with_spaces_preserve_the_close_target() {
        let store = ProfileStore::builtin();

        for command in [
            b"ssh -o \"ProxyCommand foo\" router-a\r".as_slice(),
            b"ssh -o 'ProxyCommand foo' router-a\r".as_slice(),
            b"ssh -o ProxyCommand\\ foo router-a\r".as_slice(),
            b"ssh -P work router-a\r".as_slice(),
        ] {
            let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
            runtime.observe_input(command);
            assert_eq!(
                runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
                Some(names(&["generic", "juniper"]))
            );
            assert_eq!(
                runtime.observe_output(b"Connection to router-a closed.\n", &store),
                Some(names(&["generic", "linux-unix"])),
                "command {command:?} retained the wrong SSH close target"
            );
        }
    }

    #[test]
    fn input_tracking_handles_cancellation_line_kill_and_editing_controls() {
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh cancelled\x03echo ok\r");
        assert_eq!(runtime.pending_remote, None);

        runtime.observe_input(b"ssh cancelled\x15echo ok\r");
        assert_eq!(runtime.pending_remote, None);

        runtime.observe_input(b"ssh hot\x1b[Ds\r");
        assert_eq!(
            runtime.pending_remote,
            Some(super::RemoteAttempt {
                policy: super::ClosePolicy::SshLike,
                target: None,
            })
        );
        runtime.pending_remote = None;

        runtime.observe_input(b"ssh wrong\x17router\r");
        assert_eq!(
            runtime.pending_remote,
            Some(super::RemoteAttempt {
                policy: super::ClosePolicy::SshLike,
                target: None,
            })
        );
        runtime.pending_remote = None;

        runtime.observe_input(b"\x1b[200~ssh router\x1b[201~\r");
        assert_eq!(
            runtime
                .pending_remote
                .as_ref()
                .map(|attempt| attempt.policy),
            Some(super::ClosePolicy::SshLike)
        );
        runtime.pending_remote = None;

        runtime.observe_input(b"ssh\x1bOD router\r");
        assert_eq!(
            runtime.pending_remote,
            Some(super::RemoteAttempt {
                policy: super::ClosePolicy::SshLike,
                target: None,
            })
        );
    }

    #[test]
    fn edited_nested_ssh_command_preserves_unknown_target_frame() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh router-a\r");
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
            Some(names(&["generic", "juniper"]))
        );
        runtime.accept_transition();

        runtime.observe_input(b"ssh hot\x1b[Ds\r");
        assert_eq!(
            runtime.observe_output(b"Version: FortiGate-VM64 v7.4.0\n", &store),
            Some(names(&["generic", "fortinet"]))
        );
        runtime.accept_transition();
        assert_eq!(runtime.stack_len(), 2);

        assert_eq!(
            runtime.observe_output(b"Connection closed.\n", &store),
            Some(names(&["generic", "juniper"]))
        );
        assert_eq!(runtime.stack_len(), 1);
        assert_eq!(
            runtime.observe_output(b"Connection to router-a closed.\n", &store),
            Some(names(&["generic", "linux-unix"]))
        );
        assert_eq!(runtime.stack_len(), 0);
    }

    #[test]
    fn edited_ssh_from_generic_does_not_learn_remote_linux_as_baseline() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic"]));

        runtime.observe_input(b"ssh hot\x1b[Ds\r");
        assert!(runtime.baseline_locked);
        assert_eq!(
            runtime.observe_output(
                b"OS: Ubuntu 24.04.4 LTS\nKernel: Linux 6.8.0\nTerminal: /dev/pts/1\n",
                &store,
            ),
            Some(names(&["generic", "linux-unix"]))
        );
        assert_eq!(runtime.stack_len(), 1);

        assert_eq!(
            runtime.observe_output(b"Connection to host closed.\n", &store),
            Some(names(&["generic"]))
        );
        assert_eq!(runtime.stack_len(), 0);
    }

    #[test]
    fn protocol_specific_open_failures_do_not_create_phantom_frames() {
        let store = ProfileStore::builtin();
        for (command, failure) in [
            (
                "telnet missing",
                "telnet: Unable to connect to remote host: Connection refused\nuser@local$ ",
            ),
            (
                "screen missing",
                "Cannot open your terminal '/dev/ttyUSB9' - please check.\nuser@local$ ",
            ),
            (
                "picocom /dev/ttyUSB9",
                "FATAL: cannot open /dev/ttyUSB9: No such file or directory\nuser@local$ ",
            ),
        ] {
            let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
            runtime.observe_input(format!("{command}\r").as_bytes());
            assert_eq!(runtime.observe_output(failure.as_bytes(), &store), None);
            assert_eq!(runtime.stack_len(), 0, "{command} created a phantom frame");
            assert!(runtime.at_local_baseline(&store));
        }
    }

    #[test]
    fn parent_close_during_nested_attempt_unwinds_parent_by_target() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        runtime.observe_input(b"ssh router-a\r");
        runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store);
        runtime.observe_input(b"ssh router-b\r");

        assert_eq!(
            runtime.observe_output(b"Disconnected.\nConnection to router-a closed.\n", &store,),
            Some(names(&["generic", "linux-unix"]))
        );
        assert_eq!(runtime.pending_remote, None);
        assert_eq!(runtime.stack_len(), 0);
    }

    #[test]
    fn prompt_only_detection_requires_repetition_without_input_hint() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic"]));

        assert_eq!(runtime.observe_output(b"labuser@mx480>\n", &store), None);
        assert_eq!(
            runtime.observe_output(b"labuser@mx480>\n", &store),
            Some(names(&["generic", "juniper"]))
        );
    }

    // Overflowing the profile stack must evict the OLDEST entry so the
    // remaining history stays contiguous and pops restore the right profiles.
    // The eviction used to always remove index 1, stranding the oldest entry
    // forever and corrupting the restore order for deeply nested sessions.
    #[test]
    fn profile_stack_overflow_preserves_a_route_to_the_baseline() {
        let mut runtime = ProfileRuntime::new(names(&["generic"]));

        for index in 0..(super::PROFILE_STACK_LIMIT + 2) {
            let switched = runtime.switch_to(
                vec![format!("vendor-{index}")],
                Some(super::RemoteAttempt {
                    policy: super::ClosePolicy::Generic,
                    target: None,
                }),
                false,
            );
            assert!(switched.is_some(), "switch {index} must take effect");
        }

        assert_eq!(runtime.stack_len(), super::PROFILE_STACK_LIMIT);
        assert_eq!(
            runtime.stack.first().map(|frame| frame.profiles.clone()),
            Some(names(&["generic"])),
            "the baseline frame must survive overflow"
        );
        assert_eq!(
            runtime.stack.last().map(|frame| frame.profiles.clone()),
            Some(vec!["vendor-8".to_string()]),
            "newest entry must be the previously active vendor-8"
        );
        while !runtime.stack.is_empty() {
            let _ = runtime.pop_profile();
        }
        assert_eq!(runtime.active_profiles(), names(&["generic"]));
    }

    #[test]
    fn rejected_profile_transition_restores_runtime_state() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        runtime.observe_input(b"ssh router-a\r");
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
            Some(names(&["generic", "juniper"]))
        );
        assert_eq!(runtime.stack_len(), 1);

        runtime.reject_transition();

        assert_eq!(runtime.active_profiles(), names(&["generic", "linux-unix"]));
        assert_eq!(runtime.stack_len(), 1);
        assert_eq!(runtime.pending_remote, None);
    }

    #[test]
    fn accepted_profile_transition_discards_rollback_state() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        runtime.observe_input(b"ssh router-a\r");
        assert!(
            runtime
                .observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store)
                .is_some()
        );

        runtime.accept_transition();
        runtime.reject_transition();

        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));
        assert_eq!(runtime.stack_len(), 1);
    }

    #[test]
    fn rejected_nested_transition_keeps_parent_close_accounting() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        runtime.observe_input(b"ssh router-a\r");
        assert!(
            runtime
                .observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store)
                .is_some()
        );
        runtime.accept_transition();

        runtime.observe_input(b"ssh router-b\r");
        assert_eq!(
            runtime.observe_output(b"Cisco IOS Software\n", &store),
            Some(names(&["generic", "cisco"]))
        );
        runtime.reject_transition();
        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));
        assert_eq!(runtime.stack_len(), 2);
        assert_eq!(runtime.pending_remote, None);

        assert_eq!(
            runtime.observe_output(b"Connection closed.\n", &store),
            None
        );
        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));
        assert_eq!(runtime.stack_len(), 1);
        assert_eq!(runtime.pending_remote, None);
    }

    #[test]
    fn rejected_nested_transition_survives_additional_nesting() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        runtime.observe_input(b"ssh router-a\r");
        assert!(
            runtime
                .observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store)
                .is_some()
        );
        runtime.accept_transition();

        runtime.observe_input(b"ssh router-b\r");
        assert!(
            runtime
                .observe_output(b"Cisco IOS Software\n", &store)
                .is_some()
        );
        runtime.reject_transition();
        assert_eq!(runtime.stack_len(), 2);

        runtime.observe_input(b"ssh router-c\r");
        assert!(
            runtime
                .observe_output(b"Version: FortiGate-VM64 v7.4.0\n", &store)
                .is_some()
        );
        runtime.accept_transition();
        assert_eq!(runtime.stack_len(), 3);
        assert_eq!(runtime.active_profiles(), names(&["generic", "fortinet"]));

        assert_eq!(
            runtime.observe_output(b"Connection to router-c closed.\n", &store),
            Some(names(&["generic", "juniper"]))
        );
        assert_eq!(runtime.stack_len(), 2);
        assert_eq!(
            runtime.observe_output(b"Connection closed.\n", &store),
            None
        );
        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));
        assert_eq!(runtime.stack_len(), 1);
    }

    #[test]
    fn rejected_prompt_transition_retries_after_reload() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        runtime.observe_input(b"ssh router-a\r");
        assert!(
            runtime
                .observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store)
                .is_some()
        );
        runtime.accept_transition();

        runtime.observe_input(b"ssh firewall\r");
        assert_eq!(
            runtime.observe_output(b"FW-EDGE # ", &store),
            Some(names(&["generic", "fortinet"]))
        );
        runtime.reject_transition();
        assert!(
            runtime
                .stack
                .last()
                .is_some_and(|frame| frame.unresolved_detection)
        );

        runtime.observe_input(b"ssh router-c\r");
        assert_eq!(
            runtime.observe_output(b"Cisco IOS Software\n", &store),
            Some(names(&["generic", "cisco"]))
        );
        runtime.accept_transition();
        assert_eq!(
            runtime.observe_output(b"Connection to router-c closed.\n", &store),
            Some(names(&["generic", "juniper"]))
        );
        runtime.accept_transition();
        assert!(
            runtime
                .stack
                .last()
                .is_some_and(|frame| frame.unresolved_detection),
            "accepting the child close must not resolve its rejected parent"
        );

        runtime.accept_reload_generation();
        assert_eq!(
            runtime.observe_output(b"FW-EDGE # ", &store),
            Some(names(&["generic", "fortinet"]))
        );
        runtime.accept_transition();
        assert!(
            runtime
                .stack
                .last()
                .is_some_and(|frame| !frame.unresolved_detection)
        );
    }

    #[test]
    fn reload_validation_exposes_and_preserves_hidden_profile_frames() {
        let mut store = ProfileStore::builtin();
        store.insert_profile("custom".to_string(), Vec::new(), Vec::new(), Vec::new());
        let mut runtime = ProfileRuntime::new(names(&["generic", "custom"]));
        assert!(
            runtime
                .switch_to(
                    names(&["generic", "juniper"]),
                    Some(super::RemoteAttempt {
                        policy: super::ClosePolicy::SshLike,
                        target: Some("router-a".to_string()),
                    }),
                    false,
                )
                .is_some()
        );
        assert_eq!(runtime.stack_len(), 1);
        assert!(
            runtime
                .profile_sets_for_reload()
                .contains(&names(&["generic", "custom"]))
        );

        runtime.accept_reload_generation();

        assert_eq!(runtime.stack_len(), 1);
        assert_eq!(
            runtime.stack.first().map(|frame| frame.profiles.clone()),
            Some(names(&["generic", "custom"]))
        );
        assert_eq!(runtime.baseline_profiles, names(&["generic", "custom"]));
        assert_eq!(
            runtime.observe_output(b"Connection to router-a closed.\n", &store),
            Some(names(&["generic", "custom"]))
        );
    }

    #[test]
    fn reload_preserves_same_profile_nesting() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        runtime.observe_input(b"ssh router-a\r");
        assert!(
            runtime
                .observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store)
                .is_some()
        );
        runtime.accept_transition();
        runtime.observe_input(b"ssh router-b\r");
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
            None
        );
        assert_eq!(runtime.stack_len(), 2);

        runtime.accept_reload_generation();

        assert_eq!(runtime.stack_len(), 2);
        assert_eq!(
            runtime.observe_output(b"Connection to router-b closed.\n", &store),
            None
        );
        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));
        assert_eq!(runtime.stack_len(), 1);
        assert_eq!(
            runtime.observe_output(b"Connection to router-a closed.\n", &store),
            Some(names(&["generic", "linux-unix"]))
        );
    }

    #[test]
    fn reload_preserves_pending_remote_detection_hint() {
        let mut runtime = ProfileRuntime::new(names(&["generic", "juniper"]));
        runtime.observe_input(b"ssh router-b\r");

        runtime.accept_reload_generation();

        assert_eq!(
            runtime
                .pending_remote
                .as_ref()
                .and_then(|attempt| attempt.target.as_deref()),
            Some("router-b")
        );
        assert!(runtime.baseline_locked);
    }
}
