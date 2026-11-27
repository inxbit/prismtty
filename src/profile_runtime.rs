use crate::profiles::{ProfileStore, is_generic_profile_set};

const OUTPUT_WINDOW_LIMIT: usize = 64 * 1024;
const INPUT_LINE_LIMIT: usize = 4096;
const CLOSE_MARKER_WINDOW_LIMIT: usize = 256;
const DETECTION_BYTE_INTERVAL: usize = 4096;
const PROFILE_STACK_LIMIT: usize = 8;
/// Lines typed after a remote command before its attempt expires: a user
/// name, a password, a one-time code, and the Enter that redraws a prompt.
const REMOTE_ATTEMPT_LINE_GRACE: u8 = 4;
/// Bytes of visible output after a typed `exit` within which the parent's
/// prompt must end, counted from the first output read after the Enter, as
/// the terminal receives them (escape sequences are not counted).
const EXIT_CONFIRM_WINDOW: usize = 512;
/// Time after a typed `exit` in which the connection's end must be shown. A
/// device's own client prints its close line one round trip after the
/// session ends; three seconds leaves room for a slow link.
const EXIT_CONFIRM_TIME: std::time::Duration = std::time::Duration::from_secs(3);
/// Output allowed between telnet's command echo and its close line.
const TELNET_EXCHANGE_WINDOW: usize = 256;
/// Output allowed between the Ctrl-] and the end of telnet's command echo:
/// device output already in flight when the escape was typed comes first.
const TELNET_ESCAPE_ECHO_WINDOW: usize = 4096;
/// The longest unterminated line kept to finish a telnet client's
/// `Escape character is 'X'.` announcement in the next read.
const TELNET_ANNOUNCEMENT_LIMIT: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClosePolicy {
    SshLike,
    Telnet,
    /// Telnet started with `-e`, `-E` or `-r`: Ctrl-] does not open its
    /// command prompt.
    TelnetOtherEscape,
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
    /// `[Connection to X closed by foreign host]` from a device's own client;
    /// handled only through `confirm_exit_close`.
    ForeignHostBracket,
}

impl ClosePolicy {
    fn accepts(self, marker: CloseMarkerKind) -> bool {
        if marker == CloseMarkerKind::ForeignHostBracket {
            return false;
        }
        self == ClosePolicy::Generic
            || marker == CloseMarkerKind::Logout
            || matches!(
                (self, marker),
                (ClosePolicy::SshLike, CloseMarkerKind::SshLike)
                    | (ClosePolicy::Telnet, CloseMarkerKind::Telnet)
                    | (ClosePolicy::TelnetOtherEscape, CloseMarkerKind::Telnet)
                    | (ClosePolicy::Screen, CloseMarkerKind::Screen)
                    | (ClosePolicy::Serial, CloseMarkerKind::Serial)
            )
    }
}

/// A program started by name from the local shell whose own prompt has the
/// shape of a device prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalClient {
    Sftp,
    Mysql,
}

impl LocalClient {
    fn from_command(command: &str) -> Option<Self> {
        match command {
            "sftp" => Some(Self::Sftp),
            "mysql" => Some(Self::Mysql),
            _ => None,
        }
    }

    /// The first word of every line the client asks for input on.
    fn prompt(self) -> &'static str {
        match self {
            Self::Sftp => "sftp>",
            Self::Mysql => "mysql>",
        }
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
    /// A remote command was typed inside this connection, so a close message
    /// that names no known target may belong to that nested attempt.
    nested_attempt_seen: bool,
    /// The prompt that waited where the command opening this connection was
    /// typed: what the parent shows again once the connection has ended.
    parent_prompt: Option<String>,
    /// A default-escape telnet whose `telnet> !` subshell or `z` suspend
    /// handed the keyboard to a local shell: Ctrl-] may not reach it now.
    telnet_keyboard_lost: bool,
    /// A prompt has waited while this connection was the innermost one.
    prompt_seen: bool,
    /// A line was typed into this connection before any prompt of it was
    /// seen: what it typed into is unknown, so its exit is not trusted.
    typed_before_prompt: bool,
    /// The escape character the telnet client announced, for telnet frames.
    telnet_escape: TelnetEscape,
}

/// What a telnet client said its escape character is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TelnetEscape {
    /// No `Escape character is` line was seen.
    #[default]
    Unknown,
    /// `Escape character is '^]'.`
    Default,
    /// Another character, or none, announced or set on the command line.
    Other,
}

/// A reliably typed `exit`, `quit` or `logout` on a connection whose parent
/// prompt is known, waiting for proof that the connection ended.
#[derive(Clone, Debug)]
struct ExitIntent {
    /// Stack depth when the exit was typed: the connection it may end.
    depth: usize,
    /// When the exit was typed.
    armed_at: std::time::Instant,
    /// Visible output bytes since the Enter, at most one past
    /// `EXIT_CONFIRM_WINDOW`: enough to see whether a line ends within it.
    seen: Vec<u8>,
    /// How much of `seen` has been read as complete lines, in output order.
    scanned: usize,
    /// The bracketed close naming this connection's target has been seen.
    bracket_seen: bool,
    /// The parent's prompt is the waiting line: the unterminated end of the
    /// output, after the bracket. A key typed at it, before any more output,
    /// confirms it.
    parent_waiting: bool,
}

/// From a Ctrl-] that reaches a tracked default-escape telnet: the command
/// typed at telnet's prompt and the output since that byte.
#[derive(Clone, Debug)]
struct TelnetExchange {
    /// The command typed so far, at most 8 bytes.
    command: String,
    /// Set when Enter submitted `quit` or a prefix of it: the length of
    /// `seen` then, since telnet's echo of the command ends after it.
    enter_at: Option<usize>,
    /// Visible output bytes since the Ctrl-], at most what the two windows
    /// and a close line need.
    seen: Vec<u8>,
    /// The stack as it was at the Ctrl-]: any frame pushed or popped since
    /// cancels the exchange.
    stack_generation: u64,
}

/// How firmly output identified the active profile's device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ActiveEvidence {
    /// Prompt-only detection, which cannot tell apart vendors that share a
    /// prompt shape, or a profile restored by a close.
    #[default]
    Weak,
    /// A strong banner selected the profile; its prompt has not appeared.
    Banner,
    /// The device's prompt followed its banner, so later strong signals of
    /// other vendors are command output such as neighbor descriptions.
    BannerAndPrompt,
}

#[derive(Clone, Debug)]
struct ProfileTransitionState {
    active_profiles: Vec<String>,
    active_evidence: ActiveEvidence,
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
    active_evidence: ActiveEvidence,
    baseline_profiles: Vec<String>,
    stack: Vec<ProfileFrame>,
    output_window: String,
    output_window_lower: String,
    output_bytes_since_detection: usize,
    input_line: String,
    input_line_reliable: bool,
    unreliable_remote_attempt: Option<RemoteAttempt>,
    /// The client last started by name at the local shell. Its prompt is not
    /// a device's until the local prompt, a connection or an attempt follows.
    local_client: Option<LocalClient>,
    input_escape_state: InputEscapeState,
    pending_remote: Option<RemoteAttempt>,
    pending_remote_lines: u8,
    baseline_locked: bool,
    pending_prompt_profiles: Option<Vec<String>>,
    pending_prompt_hits: usize,
    transition_rollback: Option<ProfileTransitionState>,
    /// Visible output since the last line terminator (bounded).
    last_output_line: String,
    /// The prompt that waited where the pending attempt was typed.
    pending_parent_prompt: Option<(RemoteAttempt, String)>,
    /// The waiting prompt when the first key of the current input line
    /// arrived (`None` until then): edits and history keys cannot hide it.
    line_start_prompt: Option<Option<String>>,
    exit_intent: Option<ExitIntent>,
    telnet_exchange: Option<TelnetExchange>,
    /// A pop made by a key typed at the parent's waiting prompt, reported
    /// with the next read, and the state to roll back to if it is rejected.
    pending_transition: Option<Vec<String>>,
    pending_rollback: Option<ProfileTransitionState>,
    /// The unterminated last line of a read that may begin a telnet
    /// client's escape announcement (bounded).
    telnet_announcement_partial: String,
    /// Counts every frame push and pop.
    stack_generation: u64,
    /// What the client of a pending telnet attempt announced.
    pending_telnet_escape: Option<TelnetEscape>,
}

impl ProfileRuntime {
    pub(crate) fn new(initial_profiles: Vec<String>) -> Self {
        let baseline_locked = !is_generic_profile_set(&initial_profiles);
        Self {
            active_profiles: initial_profiles.clone(),
            active_evidence: ActiveEvidence::Weak,
            baseline_profiles: initial_profiles,
            stack: Vec::new(),
            output_window: String::new(),
            output_window_lower: String::new(),
            output_bytes_since_detection: 0,
            input_line: String::new(),
            input_line_reliable: true,
            unreliable_remote_attempt: None,
            local_client: None,
            input_escape_state: InputEscapeState::None,
            pending_remote: None,
            pending_remote_lines: 0,
            baseline_locked,
            pending_prompt_profiles: None,
            pending_prompt_hits: 0,
            transition_rollback: None,
            last_output_line: String::new(),
            pending_parent_prompt: None,
            line_start_prompt: None,
            exit_intent: None,
            telnet_exchange: None,
            pending_transition: None,
            pending_rollback: None,
            telnet_announcement_partial: String::new(),
            stack_generation: 0,
            pending_telnet_escape: None,
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
            if self.exit_intent.is_some() {
                self.exit_key_typed();
            }
            self.observe_telnet_command_byte(*byte);
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

            if self.line_start_prompt.is_none() && !matches!(byte, b'\r' | b'\n') {
                let prompt = self.waiting_prompt_now();
                // A connection waiting at its parent's exact prompt text cannot
                // be told apart from that parent, so C-2b stops trusting it.
                if let (Some(prompt), Some(frame)) = (prompt.as_ref(), self.stack.last_mut()) {
                    if frame.parent_prompt.as_ref() == Some(prompt) {
                        frame.parent_prompt = None;
                    }
                }
                self.line_start_prompt = Some(prompt);
            }
            match byte {
                0x1b => self.input_escape_state = InputEscapeState::Esc,
                0x03 => {
                    self.input_line.clear();
                    self.input_line_reliable = true;
                    self.unreliable_remote_attempt = None;
                    self.pending_remote = None;
                    self.exit_intent = None;
                    self.line_start_prompt = None;
                }
                0x15 => {
                    self.input_line.clear();
                    self.input_line_reliable = true;
                    self.unreliable_remote_attempt = None;
                }
                0x17 => self.invalidate_input_line(),
                b'\r' | b'\n' => {
                    self.exit_intent = None;
                    if self.input_line_reliable {
                        self.submit_input_line();
                    } else if let Some(attempt) = self.unreliable_remote_attempt.take() {
                        let parent_prompt = self.line_start_prompt.clone().flatten();
                        self.arm_remote_attempt(attempt.clone());
                        self.pending_parent_prompt = parent_prompt.map(|prompt| (attempt, prompt));
                    } else {
                        self.age_pending_remote();
                    }
                    self.line_start_prompt = None;
                    self.input_line.clear();
                    self.input_line_reliable = true;
                    self.unreliable_remote_attempt = None;
                }
                0x08 | 0x7f => {
                    if self.input_line_reliable {
                        self.input_line.pop();
                    }
                }
                byte if (byte.is_ascii_graphic() || *byte == b' ')
                    && self.input_line_reliable
                    && self.input_line.len() < INPUT_LINE_LIMIT =>
                {
                    self.input_line.push(*byte as char);
                }
                byte if byte.is_ascii_graphic() || *byte == b' ' => {
                    self.invalidate_input_line();
                }
                // TAB asks the shell to complete the word, so like the other
                // editing keys it leaves the typed text unreliable.
                _ => self.invalidate_input_line(),
            }
        }
    }

    /// Follows telnet's command mode: Ctrl-] then one command line.
    fn observe_telnet_command_byte(&mut self, byte: u8) {
        if byte == 0x1d {
            let stack_generation = self.stack_generation;
            self.telnet_exchange = self
                .default_escape_telnet_reachable()
                .then(|| TelnetExchange {
                    command: String::new(),
                    enter_at: None,
                    seen: Vec::new(),
                    stack_generation,
                });
            return;
        }
        let Some(exchange) = self.telnet_exchange.as_mut() else {
            return;
        };
        if exchange.enter_at.is_some() {
            // The command was submitted; a later Enter ends the exchange.
            if matches!(byte, b'\r' | b'\n') {
                self.telnet_exchange = None;
            }
            return;
        }
        match byte {
            b'\r' | b'\n' => {
                let command = exchange.command.trim().to_ascii_lowercase();
                if command.starts_with('!') || command == "z" {
                    // A local shell owns the keyboard now (`!` subshell, `z`
                    // suspend); this rule cannot tell when telnet has it again.
                    if let Some(outermost) = self.outermost_default_escape_telnet() {
                        self.stack[outermost].telnet_keyboard_lost = true;
                    }
                    self.telnet_exchange = None;
                } else if !command.is_empty() && "quit".starts_with(&command) {
                    // Not `close`: it leaves telnet's own `telnet>` prompt
                    // waiting, so the keyboard does not return to the parent.
                    exchange.command = command;
                    exchange.enter_at = Some(exchange.seen.len());
                } else {
                    self.telnet_exchange = None;
                }
            }
            0x08 | 0x7f => {
                exchange.command.pop();
            }
            byte if (byte.is_ascii_graphic() || byte == b' ') && exchange.command.len() < 8 => {
                exchange.command.push(byte as char);
            }
            _ => self.telnet_exchange = None,
        }
    }

    fn outermost_default_escape_telnet(&self) -> Option<usize> {
        self.stack.iter().position(|frame| {
            frame.close_policy == ClosePolicy::Telnet
                && frame.telnet_escape == TelnetEscape::Default
        })
    }

    /// Ctrl-] is known to open a tracked telnet's command prompt: every
    /// telnet in the stack said which escape it uses, one of them uses
    /// Ctrl-], and no local shell took the keyboard from it.
    fn default_escape_telnet_reachable(&self) -> bool {
        let telnets = || {
            self.stack.iter().filter(|frame| {
                matches!(
                    frame.close_policy,
                    ClosePolicy::Telnet | ClosePolicy::TelnetOtherEscape
                )
            })
        };
        if telnets()
            .any(|frame| frame.telnet_keyboard_lost || frame.telnet_escape == TelnetEscape::Unknown)
        {
            return false;
        }
        self.outermost_default_escape_telnet().is_some()
            || (self
                .pending_remote
                .as_ref()
                .is_some_and(|pending| pending.policy == ClosePolicy::Telnet)
                && self.pending_telnet_escape == Some(TelnetEscape::Default))
    }

    /// Reads a telnet client's `Escape character is 'X'.` line into the
    /// pending attempt or the innermost telnet frame that has none yet. The
    /// line may arrive across reads: a bounded unterminated tail is kept.
    fn note_telnet_escape(&mut self, text: &str) {
        let pending_telnet = self
            .pending_remote
            .as_ref()
            .is_some_and(|pending| pending.policy == ClosePolicy::Telnet);
        let unknown_frame = self.stack.last().is_some_and(|frame| {
            frame.close_policy == ClosePolicy::Telnet
                && frame.telnet_escape == TelnetEscape::Unknown
        });
        if !pending_telnet && !unknown_frame {
            self.telnet_announcement_partial.clear();
            return;
        }
        let mut combined = std::mem::take(&mut self.telnet_announcement_partial);
        combined.push_str(text);
        let complete_end = combined.rfind(is_line_terminator).map_or(0, |end| end + 1);
        for line in combined[..complete_end].split(is_line_terminator) {
            let line = line.trim().to_ascii_lowercase();
            let Some(value) = line
                .strip_prefix("escape character is '")
                .and_then(|rest| rest.strip_suffix("'."))
            else {
                continue;
            };
            let escape = if value == "^]" {
                TelnetEscape::Default
            } else {
                TelnetEscape::Other
            };
            if pending_telnet {
                self.pending_telnet_escape = Some(escape);
            } else if let Some(frame) = self.stack.last_mut().filter(|frame| {
                frame.close_policy == ClosePolicy::Telnet
                    && frame.telnet_escape == TelnetEscape::Unknown
            }) {
                frame.telnet_escape = escape;
            }
        }
        let tail = &combined[complete_end..];
        if tail.len() <= TELNET_ANNOUNCEMENT_LIMIT {
            self.telnet_announcement_partial = tail.to_string();
        }
    }

    /// A prompt waiting after this read, while a connection is the innermost
    /// one, is that connection's. A child that shows its parent's exact
    /// prompt cannot be told apart from it, so C-2b stops trusting it.
    fn note_child_prompt(&mut self) {
        if self
            .exit_intent
            .as_ref()
            .is_some_and(|intent| intent.parent_waiting)
        {
            return;
        }
        let Some(prompt) = self.waiting_prompt_now() else {
            return;
        };
        if let Some(frame) = self.stack.last_mut() {
            frame.prompt_seen = true;
            if frame.parent_prompt.as_ref() == Some(&prompt) {
                frame.parent_prompt = None;
            }
        }
    }

    /// The last output line, when it is a waiting prompt.
    fn waiting_prompt_now(&self) -> Option<String> {
        let prompt = self.last_output_line.trim_end();
        (!prompt.is_empty() && looks_like_prompt_detection_boundary(prompt)).then(|| {
            prompt
                .trim_start_matches(|ch: char| ch.is_ascii_control())
                .to_string()
        })
    }

    /// The prompt left on the line once the echo of `typed` is taken off it.
    fn waiting_prompt_before(&self, typed: &str) -> Option<String> {
        let line = self.last_output_line.as_str();
        let echoed = (0..=typed.len())
            .rev()
            .find(|len| typed.is_char_boundary(*len) && line.ends_with(&typed[..*len]))
            .unwrap_or(0);
        let prompt = line[..line.len() - echoed].trim_end();
        (!prompt.is_empty() && looks_like_prompt_detection_boundary(prompt)).then(|| {
            prompt
                .trim_start_matches(|ch: char| ch.is_ascii_control())
                .to_string()
        })
    }

    fn invalidate_input_line(&mut self) {
        if self.input_line_reliable {
            self.unreliable_remote_attempt =
                self.typed_remote_attempt(&self.input_line)
                    .map(|mut attempt| {
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
        // A pop made by a key since the last read is reported with this read
        // and rolls back to the state from before it.
        let rollback = self
            .pending_rollback
            .take()
            .unwrap_or_else(|| self.transition_state());
        let transition = self.observe_output_inner(visible_output, store);
        let transition = transition.or_else(|| self.pending_transition.take());
        self.pending_transition = None;
        self.note_child_prompt();
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

        match text.rfind(is_line_terminator) {
            Some(end) => {
                self.last_output_line.clear();
                self.last_output_line.push_str(&text[end + 1..]);
            }
            None => self.last_output_line.push_str(&text),
        }
        trim_to_recent_chars(&mut self.last_output_line, 512);
        self.note_telnet_escape(&text);
        if let Some(transition) = self.confirm_exit_close(visible_output) {
            self.output_window.push_str(&text);
            self.output_window_lower
                .push_str(&text.to_ascii_lowercase());
            return transition;
        }
        if self.telnet_quit_confirmed(visible_output) {
            self.clear_output_window();
            self.clear_pending_prompt();
            if let Some(outermost) = self.outermost_default_escape_telnet() {
                self.pending_remote = None;
                let before = self.active_profiles.clone();
                while self.stack.len() > outermost {
                    self.pop_profile();
                }
                return (self.active_profiles != before).then(|| self.active_profiles.clone());
            }
            if self
                .pending_remote
                .as_ref()
                .is_some_and(|pending| pending.policy == ClosePolicy::Telnet)
            {
                self.pending_remote = None;
            }
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
                close_markers.iter().position(|marker| {
                    close_marker_matches(marker, frame.close_policy, &frame.target)
                })
            });
            let top_specific_match = self.stack.last().and_then(|frame| {
                close_markers.iter().position(|marker| {
                    marker.target.is_some()
                        && close_marker_matches(marker, frame.close_policy, &frame.target)
                        && !self.pending_remote.as_ref().is_some_and(|pending| {
                            pending.target.is_some()
                                && close_marker_matches(marker, pending.policy, &pending.target)
                        })
                })
            });

            if let Some(marker) =
                top_specific_match.or(top_match.filter(|_| pending_match.is_none()))
            {
                self.pending_remote = None;
                self.clear_output_window();
                self.clear_pending_prompt();
                return self.pop_profiles_closed_from(&close_markers[marker..]);
            }
            if pending_match.is_some() {
                self.pending_remote = None;
                self.clear_output_window();
                self.clear_pending_prompt();
                return None;
            }
            if let Some(marker) = top_match {
                self.pending_remote = None;
                self.clear_output_window();
                self.clear_pending_prompt();
                return self.pop_profiles_closed_from(&close_markers[marker..]);
            }
            // OpenSSH names the HostName an ssh_config alias resolves to, not
            // the typed alias. With no remote command typed inside this
            // connection, a targeted ssh close naming no other connection
            // can only be its own.
            let untracked_close = self
                .stack
                .last()
                .filter(|frame| self.pending_remote.is_none() && !frame.nested_attempt_seen)
                .and_then(|frame| {
                    close_markers.iter().position(|marker| {
                        frame.close_policy.accepts(marker.kind)
                            && is_untracked_ssh_close(marker, &self.stack)
                    })
                });
            if let Some(marker) = untracked_close {
                self.clear_output_window();
                self.clear_pending_prompt();
                return self.pop_profiles_closed_from(&close_markers[marker..]);
            }
            // Likewise an alias attempt that no output resolved ends on the
            // close naming its HostName; left pending, the next prompt would
            // record a connection that never closes.
            if self.pending_remote.as_ref().is_some_and(|pending| {
                close_markers.iter().any(|marker| {
                    pending.policy.accepts(marker.kind)
                        && is_untracked_ssh_close(marker, &self.stack)
                })
            }) {
                self.pending_remote = None;
                self.clear_output_window();
                self.clear_pending_prompt();
                return None;
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

        // A client started by name at the local shell is over once the local
        // prompt waits again. This pass may still hold its last prompt line.
        let local_client = self.local_client;
        if local_client.is_some()
            && self.at_local_baseline(store)
            && store
                .active_specific_profile(&self.active_profiles)
                .is_some_and(|profile| self.waiting_at_prompt(store, profile))
        {
            self.local_client = None;
        }

        // Gate on the evidence from before this read, so a read carrying a
        // banner and a prompt can never block that banner's own switch.
        let banner_and_prompt = self.active_evidence == ActiveEvidence::BannerAndPrompt;
        if self.active_evidence == ActiveEvidence::Banner
            && store
                .active_specific_profile(&self.active_profiles)
                .is_some_and(|profile| store.matches_runtime_prompt(profile, &self.output_window))
        {
            self.active_evidence = ActiveEvidence::BannerAndPrompt;
        }

        let detected =
            store.detect_profiles_with_lowercase(&self.output_window, &self.output_window_lower);
        if is_generic_profile_set(&detected) {
            self.retain_unterminated_tail();
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
            self.retain_unterminated_tail();
            self.clear_pending_prompt();
            return None;
        }

        if self.should_learn_baseline(&detected, store) {
            self.baseline_profiles = detected.clone();
            self.active_profiles = detected.clone();
            self.active_evidence = ActiveEvidence::Weak;
            self.baseline_locked = true;
            self.clear_output_window();
            self.clear_pending_prompt();
            return Some(detected);
        }

        let active_profile = store.active_specific_profile(&self.active_profiles);
        let at_local_baseline = self.at_local_baseline(store);
        let unresolved_remote = self
            .stack
            .last()
            .is_some_and(|frame| frame.unresolved_detection);
        let has_remote_hint = self.pending_remote.is_some() || unresolved_remote;
        // Once a banner-identified device shows its prompt, another vendor's
        // strong signal is command output, such as an LLDP or CDP neighbor's
        // system description, unless a remote command was typed.
        let identified_remote = banner_and_prompt
            && !has_remote_hint
            && active_profile.is_some_and(|profile| !store.is_local_baseline_profile(profile));
        let strong_profile = if identified_remote {
            None
        } else {
            store.strong_transition_profile(&detected, &self.output_window_lower, active_profile)
        };
        if let Some(profile) = strong_profile {
            let remote_attempt = self.pending_remote.take();
            let evidence = if self.waiting_at_prompt(store, &profile) {
                ActiveEvidence::BannerAndPrompt
            } else {
                ActiveEvidence::Banner
            };
            self.clear_output_window();
            self.clear_pending_prompt();
            let transition =
                self.switch_to(profile_set(&profile), remote_attempt, at_local_baseline);
            self.active_evidence = evidence;
            return transition;
        }

        if has_remote_hint || active_profile.is_none() || at_local_baseline {
            // The lines a local client asks for input on are its own prompt,
            // whatever their shape.
            let prompt_profile = match local_client {
                Some(client) => store.prompt_transition_profile(
                    &detected,
                    &without_prompt_lines(&self.output_window, client.prompt()),
                    active_profile,
                ),
                None => {
                    store.prompt_transition_profile(&detected, &self.output_window, active_profile)
                }
            };
            if let Some(profile) = prompt_profile {
                // Without a remote hint, only a prompt waiting for input (the
                // unterminated last line) is trusted on first sight; a
                // prompt-shaped line in local output must repeat.
                if store.prompt_switches_on_first_detection(
                    &profile,
                    has_remote_hint,
                    at_local_baseline,
                ) && (has_remote_hint || self.waiting_at_prompt(store, &profile))
                {
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
        } else {
            self.retain_unterminated_tail();
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
        self.active_evidence = rollback.active_evidence;
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
            active_evidence: self.active_evidence,
            baseline_profiles: self.baseline_profiles.clone(),
            stack: self.stack.clone(),
            pending_remote: self.pending_remote.clone(),
            baseline_locked: self.baseline_locked,
            pending_prompt_profiles: self.pending_prompt_profiles.clone(),
            pending_prompt_hits: self.pending_prompt_hits,
        }
    }

    fn submit_input_line(&mut self) {
        let typed = std::mem::take(&mut self.input_line);
        match self.typed_remote_attempt(typed.trim_start()) {
            Some(attempt) => {
                let parent_prompt = self
                    .line_start_prompt
                    .clone()
                    .flatten()
                    .or_else(|| self.waiting_prompt_before(&typed));
                self.arm_remote_attempt(attempt.clone());
                self.pending_parent_prompt = parent_prompt.map(|prompt| (attempt, prompt));
            }
            None => self.age_pending_remote(),
        }
        if let Some(frame) = self.stack.last_mut().filter(|frame| !frame.prompt_seen) {
            frame.typed_before_prompt = true;
        }
        if matches!(
            typed.trim().to_ascii_lowercase().as_str(),
            "exit" | "quit" | "logout"
        ) && self.stack.last().is_some_and(|frame| {
            frame.target.is_some()
                && frame.parent_prompt.is_some()
                && frame.prompt_seen
                && !frame.typed_before_prompt
                && matches!(
                    frame.close_policy,
                    ClosePolicy::SshLike | ClosePolicy::Telnet | ClosePolicy::TelnetOtherEscape
                )
        }) {
            self.exit_intent = Some(ExitIntent {
                depth: self.stack.len(),
                armed_at: std::time::Instant::now(),
                seen: Vec::new(),
                scanned: 0,
                bracket_seen: false,
                parent_waiting: false,
            });
        }
        self.input_line = typed;
        self.remember_local_client();
    }

    /// The connection an exit intent may end: its target and the prompt its
    /// parent showed, while it is still the top frame.
    fn exit_intent_frame(&self) -> Option<(String, String)> {
        let intent = self.exit_intent.as_ref()?;
        let frame = self
            .stack
            .last()
            .filter(|_| self.stack.len() == intent.depth)?;
        Some((frame.target.clone()?, frame.parent_prompt.clone()?))
    }

    /// C-2b: after a typed exit, a `[Connection to X closed by foreign host]`
    /// line naming the top connection ends it only when, reading the output
    /// since the Enter line by line in the order it came, the first prompt
    /// after that line is the one its parent showed when the connection was
    /// opened, ending within the first `EXIT_CONFIRM_WINDOW` bytes and shown
    /// within `EXIT_CONFIRM_TIME`. The first prompt decides either way. A
    /// complete line decides at once. The unterminated last line is the
    /// waiting prompt only once a key is typed at it before more output
    /// arrives (`exit_key_typed`); a line that goes on is judged when it
    /// ends. Bytes are counted as they arrive, so read grouping does not
    /// change the result.
    fn confirm_exit_close(&mut self, visible: &[u8]) -> Option<Option<Vec<String>>> {
        let expired = self.exit_intent.as_ref()?.armed_at.elapsed() > EXIT_CONFIRM_TIME;
        let Some((target, parent_prompt)) = self.exit_intent_frame().filter(|_| !expired) else {
            self.exit_intent = None;
            return None;
        };
        let intent = self.exit_intent.as_mut()?;
        intent.parent_waiting = false;
        let room = (EXIT_CONFIRM_WINDOW + 1).saturating_sub(intent.seen.len());
        intent
            .seen
            .extend_from_slice(&visible[..visible.len().min(room)]);
        let mut decision = None;
        while let Some(length) = intent.seen[intent.scanned..]
            .iter()
            .position(|byte| matches!(byte, b'\r' | b'\n'))
        {
            let start = intent.scanned;
            let end = start + length;
            intent.scanned = end + 1;
            let line = String::from_utf8_lossy(&intent.seen[start..end]);
            if is_foreign_host_bracket(&line, &target) {
                intent.bracket_seen = true;
            } else if !line.trim().is_empty() && looks_like_prompt_detection_boundary(&line) {
                decision = Some(
                    intent.bracket_seen
                        && end <= EXIT_CONFIRM_WINDOW
                        && waiting_prompt_text(&line) == parent_prompt,
                );
                break;
            }
        }
        match decision {
            Some(true) => {
                self.exit_intent = None;
                Some(self.pop_exited_connection())
            }
            Some(false) => {
                self.exit_intent = None;
                None
            }
            None if intent.seen.len() > EXIT_CONFIRM_WINDOW => {
                // Nothing that ends past the window can confirm.
                self.exit_intent = None;
                None
            }
            None => {
                let rest = String::from_utf8_lossy(&intent.seen[intent.scanned..]);
                intent.parent_waiting =
                    intent.bracket_seen && waiting_prompt_text(&rest) == parent_prompt;
                None
            }
        }
    }

    /// A key typed while an exit intent is set: if the parent's prompt is
    /// the waiting line, the connection ended. Any other key, typed at the
    /// child's prompt or ahead of the output, ends the intent.
    fn exit_key_typed(&mut self) {
        let waiting = self
            .exit_intent
            .as_ref()
            .is_some_and(|intent| intent.parent_waiting)
            && self.exit_intent_frame().is_some();
        self.exit_intent = None;
        if !waiting {
            return;
        }
        let snapshot = self.transition_state();
        if let Some(profiles) = self.pop_exited_connection() {
            self.pending_transition = Some(profiles);
            self.pending_rollback.get_or_insert(snapshot);
        }
    }

    fn pop_exited_connection(&mut self) -> Option<Vec<String>> {
        self.pending_remote = None;
        self.clear_output_window();
        self.clear_pending_prompt();
        let before = self.active_profiles.clone();
        self.pop_profile();
        (self.active_profiles != before).then(|| self.active_profiles.clone())
    }

    /// C-2c: after the Ctrl-], telnet's own `telnet> COMMAND` line must end
    /// within `TELNET_ESCAPE_ECHO_WINDOW` bytes, after the Enter that
    /// submitted the command, and the first non-blank line after it must be
    /// a bare `Connection closed.` that starts within another
    /// `TELNET_EXCHANGE_WINDOW` bytes. Lines are read in the order they came:
    /// a close message before the echo changes the stack first (the
    /// close-marker path), which cancels the exchange. Returns whether the
    /// exchange completed.
    fn telnet_quit_confirmed(&mut self, visible: &[u8]) -> bool {
        let Some(exchange) = self.telnet_exchange.as_mut() else {
            return false;
        };
        if exchange.stack_generation != self.stack_generation {
            self.telnet_exchange = None;
            return false;
        }
        let limit = TELNET_ESCAPE_ECHO_WINDOW + TELNET_EXCHANGE_WINDOW + 64;
        let room = limit.saturating_sub(exchange.seen.len());
        exchange
            .seen
            .extend_from_slice(&visible[..visible.len().min(room)]);
        let enter_at = exchange.enter_at;
        let echo = format!("telnet> {}", exchange.command);
        let mut offset = 0usize;
        let mut echo_end = None;
        let mut decided = None;
        for piece in exchange
            .seen
            .split_inclusive(|byte| matches!(byte, b'\r' | b'\n'))
        {
            let start = offset;
            offset += piece.len();
            if !matches!(piece.last(), Some(b'\r' | b'\n')) {
                break;
            }
            let line = String::from_utf8_lossy(piece).trim().to_ascii_lowercase();
            match echo_end {
                None => {
                    if !observed_close_markers(&line).is_empty() {
                        // A close message before telnet's echo: the stack
                        // changes first, and the exchange with it.
                        decided = Some(false);
                        break;
                    }
                    // The echo ends with the line end that follows the
                    // user's Enter; a `telnet> q` line already complete
                    // before it is not this command's echo.
                    if line == echo && enter_at.is_some_and(|enter_at| offset > enter_at) {
                        if offset > TELNET_ESCAPE_ECHO_WINDOW {
                            decided = Some(false);
                            break;
                        }
                        echo_end = Some(offset);
                    }
                }
                Some(end) => {
                    if line.is_empty() {
                        continue;
                    }
                    decided = Some(
                        matches!(line.as_str(), "connection closed" | "connection closed.")
                            && start - end <= TELNET_EXCHANGE_WINDOW,
                    );
                    break;
                }
            }
        }
        match decided {
            Some(confirmed) => {
                self.telnet_exchange = None;
                confirmed
            }
            None => {
                // The echo line must end within the first window; the close
                // line must start within one more window after it.
                let bound = echo_end.map_or(TELNET_ESCAPE_ECHO_WINDOW, |end| {
                    end + TELNET_EXCHANGE_WINDOW
                });
                if offset > bound || exchange.seen.len() >= limit {
                    self.telnet_exchange = None;
                }
                false
            }
        }
    }

    /// `sftp` or `mysql` started by name where the local baseline's profiles
    /// are active and no connection is open.
    fn remember_local_client(&mut self) {
        if !self.stack.is_empty() || self.active_profiles != self.baseline_profiles {
            return;
        }
        if let Some(client) = self
            .input_line
            .split_whitespace()
            .next()
            .and_then(LocalClient::from_command)
        {
            self.local_client = Some(client);
        }
    }

    /// Where the local baseline's profiles are active (the local shell or a
    /// host like it), `ssh` is OpenSSH. A network device's own `ssh` takes
    /// keywords after the host (`vrf management`, `routing-instance mgmt`),
    /// so OpenSSH's rules for commands that open no session do not apply.
    fn typed_remote_attempt(&self, line: &str) -> Option<RemoteAttempt> {
        remote_attempt(line, self.active_profiles == self.baseline_profiles)
    }

    /// A login can take a few typed lines (user name, password, the Enter
    /// that redraws a prompt-only device's prompt) before output resolves
    /// the attempt, so ordinary lines expire it only after a grace period.
    fn age_pending_remote(&mut self) {
        if self.pending_remote.is_none() {
            return;
        }
        self.pending_remote_lines = self.pending_remote_lines.saturating_add(1);
        if self.pending_remote_lines > REMOTE_ATTEMPT_LINE_GRACE {
            self.pending_remote = None;
        }
    }

    fn arm_remote_attempt(&mut self, attempt: RemoteAttempt) {
        self.pending_parent_prompt = None;
        self.pending_telnet_escape = None;
        self.pending_remote = Some(attempt);
        self.pending_remote_lines = 0;
        if let Some(frame) = self.stack.last_mut() {
            frame.nested_attempt_seen = true;
        }
        self.baseline_locked = true;
        self.clear_output_window();
        self.clear_pending_prompt();
        self.local_client = None;
    }

    fn should_detect_profiles(&self, text: &str) -> bool {
        self.pending_remote.is_some()
            || self.output_bytes_since_detection >= DETECTION_BYTE_INTERVAL
            || text.bytes().any(|byte| matches!(byte, b'\r' | b'\n'))
            || looks_like_prompt_detection_boundary(text)
    }

    fn waiting_at_prompt(&self, store: &ProfileStore, profile: &str) -> bool {
        let tail = unterminated_tail(&self.output_window);
        looks_like_prompt_detection_boundary(tail) && store.matches_runtime_prompt(profile, tail)
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
            // The repeat must come from new output, not from the next pass
            // finding the same complete line again. A prompt waiting for
            // input is the unterminated tail and stays.
            self.retain_unterminated_tail();
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
            self.active_evidence = ActiveEvidence::Weak;
            return changed.then(|| self.active_profiles.clone());
        }
        if profiles == self.active_profiles {
            return None;
        }
        self.active_evidence = ActiveEvidence::Weak;
        if let Some(stack_index) = self
            .stack
            .iter()
            .position(|frame| frame.profiles == profiles)
        {
            self.stack.truncate(stack_index);
            self.stack_generation += 1;
            self.telnet_exchange = None;
            self.exit_intent = None;
            self.active_profiles = profiles;
            return Some(self.active_profiles.clone());
        }
        if profiles == self.baseline_profiles {
            self.stack.clear();
            self.stack_generation += 1;
            self.telnet_exchange = None;
            self.exit_intent = None;
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
        self.active_evidence = ActiveEvidence::Weak;
        self.exit_intent = None;
        self.stack_generation += 1;
        let next = self
            .stack
            .pop()
            .map(|frame| frame.profiles)
            .unwrap_or_else(|| self.baseline_profiles.clone());
        self.local_client = None;
        if next == self.active_profiles {
            None
        } else {
            self.active_profiles = next;
            Some(self.active_profiles.clone())
        }
    }

    /// Pops the connection the first of `markers` closed, then each
    /// connection under it that a later marker names. Leaving two nested
    /// connections at once puts both teardown lines in one read. Only a
    /// marker that names its connection goes on: a bare `logout` or
    /// "Connection closed." after the first belongs to the same teardown.
    fn pop_profiles_closed_from(&mut self, markers: &[ObservedCloseMarker]) -> Option<Vec<String>> {
        let before = self.active_profiles.clone();
        self.pop_profile();
        let mut later = &markers[1..];
        // One exit prints `logout` and then its close line. That line ended
        // the connection just popped, whichever host it names.
        if markers[0].kind == CloseMarkerKind::Logout
            && later
                .first()
                .is_some_and(|marker| marker.kind != CloseMarkerKind::Logout)
        {
            later = &later[1..];
        }
        while let Some(marker) = self.stack.last().and_then(|frame| {
            later.iter().position(|marker| {
                marker.target.is_some()
                    && marker.target == frame.target
                    && frame.close_policy.accepts(marker.kind)
            })
        }) {
            self.pop_profile();
            later = &later[marker + 1..];
        }
        (self.active_profiles != before).then(|| self.active_profiles.clone())
    }

    fn push_frame(&mut self, remote_attempt: RemoteAttempt) {
        self.push_frame_with_detection_state(remote_attempt, false);
    }

    fn push_frame_with_detection_state(
        &mut self,
        remote_attempt: RemoteAttempt,
        unresolved_detection: bool,
    ) {
        self.local_client = None;
        if self.stack.len() >= PROFILE_STACK_LIMIT {
            // The first frame is the route back to the local baseline. Preserve
            // it and collapse the oldest interior nesting level instead.
            self.stack.remove(1);
        }
        let parent_prompt = self
            .pending_parent_prompt
            .as_ref()
            .filter(|(attempt, _)| *attempt == remote_attempt)
            .map(|(_, prompt)| prompt.clone());
        self.exit_intent = None;
        self.stack.push(ProfileFrame {
            profiles: self.active_profiles.clone(),
            close_policy: remote_attempt.policy,
            target: remote_attempt.target,
            unresolved_detection,
            nested_attempt_seen: false,
            parent_prompt,
            telnet_keyboard_lost: false,
            prompt_seen: false,
            typed_before_prompt: false,
            telnet_escape: match remote_attempt.policy {
                ClosePolicy::TelnetOtherEscape => TelnetEscape::Other,
                ClosePolicy::Telnet => self.pending_telnet_escape.take().unwrap_or_default(),
                _ => TelnetEscape::Unknown,
            },
        });
        self.stack_generation += 1;
        self.telnet_exchange = None;
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

    /// Drops the complete lines a detection pass already examined. Signals
    /// and prompts are matched per line, so only an unterminated trailing
    /// line, such as a banner or prompt split across reads, needs another
    /// look once the rest of it arrives.
    fn retain_unterminated_tail(&mut self) {
        if let Some(end) = self.output_window.rfind(is_line_terminator) {
            // ASCII lowercasing keeps byte offsets, so both windows share them.
            self.output_window.drain(..=end);
            self.output_window_lower.drain(..=end);
        }
        // Output without line terminators is not a prompt line; keep only
        // enough of it for a signal split across the next read.
        trim_to_recent_chars(&mut self.output_window, DETECTION_BYTE_INTERVAL);
        trim_to_recent_chars(&mut self.output_window_lower, DETECTION_BYTE_INTERVAL);
    }
}

fn profile_set(profile: &str) -> Vec<String> {
    vec!["generic".to_string(), profile.to_string()]
}

fn remote_attempt(line: &str, openssh_client: bool) -> Option<RemoteAttempt> {
    let words = split_shell_words(line);
    let command = words
        .as_ref()
        .and_then(|words| words.first())
        .map(|word| word.as_str())
        .or_else(|| line.split_whitespace().next())?
        .to_ascii_lowercase();
    let mut policy = match command.as_str() {
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
    let mut target_seen = false;
    let mut remote_command = false;
    let mut force_tty = false;
    let mut no_session = false;
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
            if command == "telnet" && telnet_option_changes_escape(word) {
                policy = ClosePolicy::TelnetOtherEscape;
            }
            skip_option_value = remote_option_takes_value(&command, word);
            no_session |= remote_option_opens_no_session(&command, word);
            force_tty |= command == "ssh" && ssh_option_flags(word).contains(&'t');
            continue;
        }
        if target_seen {
            // OpenSSH parses options again after the host, so the first
            // other word after it starts a remote command.
            remote_command = true;
            break;
        }
        target_seen = true;
        if is_literal_remote_target(word) {
            target = Some(normalize_remote_target(word));
        }
        if command != "ssh" {
            break;
        }
    }
    // A remote command without a forced tty, a session without a tty, or a
    // query or background invocation never opens an interactive session
    // to close again.
    if openssh_client && (no_session || (remote_command && !force_tty)) {
        return None;
    }
    Some(RemoteAttempt { policy, target })
}

/// `[Connection to X closed by foreign host]` naming `target`.
fn is_foreign_host_bracket(line: &str, target: &str) -> bool {
    observed_close_markers(&line.to_ascii_lowercase())
        .iter()
        .any(|marker| {
            marker.kind == CloseMarkerKind::ForeignHostBracket
                && marker.target.as_deref() == Some(target)
        })
}

/// A prompt line as it is compared: trailing blanks and leading control
/// characters dropped.
fn waiting_prompt_text(line: &str) -> &str {
    line.trim_end()
        .trim_start_matches(|ch: char| ch.is_ascii_control())
}

/// `-e` (another escape character), `-E` (none) and `-r` (rlogin-style `~`)
/// among a telnet option word's flags, up to the first flag that takes a
/// value.
fn telnet_option_changes_escape(option: &str) -> bool {
    let Some(flags) = option.strip_prefix('-') else {
        return false;
    };
    for flag in flags.chars() {
        if matches!(flag, 'e' | 'E' | 'r') {
            return true;
        }
        if remote_option_word_takes_value("telnet", &format!("-{flag}")) {
            return false;
        }
    }
    false
}

fn remote_option_opens_no_session(command: &str, option: &str) -> bool {
    match command {
        "ssh" => ssh_option_flags(option)
            .iter()
            .any(|flag| matches!(flag, 'V' | 'G' | 'N' | 'f' | 'Q' | 'O' | 'T')),
        "screen" => matches!(
            option,
            "-ls" | "-list" | "-wipe" | "-v" | "-version" | "-Q" | "-X"
        ),
        _ => false,
    }
}

/// The getopt flags of one ssh option word, up to the first flag that takes
/// a value (the rest of the word or the next word).
fn ssh_option_flags(option: &str) -> Vec<char> {
    let mut flags = Vec::new();
    let Some(cluster) = option.strip_prefix('-') else {
        return flags;
    };
    if cluster.starts_with('-') {
        return flags;
    }
    for flag in cluster.chars() {
        flags.push(flag);
        if remote_option_word_takes_value("ssh", &format!("-{flag}")) {
            break;
        }
    }
    flags
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
        "telnet" => matches!(option, "-e" | "-k" | "-l" | "-n" | "-S" | "-s" | "-X"),
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
    text.split(is_line_terminator)
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
            } else if let Some(target) = line
                .strip_prefix("[connection to ")
                .and_then(|rest| rest.strip_suffix(" closed by foreign host]"))
                .filter(|target| !target.is_empty() && !target.contains(char::is_whitespace))
            {
                Some(ObservedCloseMarker {
                    kind: CloseMarkerKind::ForeignHostBracket,
                    target: Some(normalize_remote_target(target)),
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

/// A targeted ssh close naming no tracked connection's target.
fn is_untracked_ssh_close(marker: &ObservedCloseMarker, stack: &[ProfileFrame]) -> bool {
    marker.kind == CloseMarkerKind::SshLike
        && marker.target.as_ref().is_some_and(|target| {
            !stack
                .iter()
                .any(|frame| frame.target.as_ref() == Some(target))
        })
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
            ClosePolicy::Telnet | ClosePolicy::TelnetOtherEscape => {
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

/// The trailing line of `text` that no line terminator has ended yet.
fn unterminated_tail(text: &str) -> &str {
    text.rfind(is_line_terminator)
        .map_or(text, |end| &text[end + 1..])
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

/// Line terminators for close-marker inspection. A bare carriage return
/// counts too: some devices end every line with `\r` only, and their teardown
/// lines must still be seen as complete.
fn is_line_terminator(character: char) -> bool {
    matches!(character, '\n' | '\r')
}

/// `text` without the lines whose first word is `prompt`.
fn without_prompt_lines(text: &str, prompt: &str) -> String {
    text.split_inclusive(is_line_terminator)
        .filter(|line| line.split_whitespace().next() != Some(prompt))
        .collect()
}

/// The most recent terminated lines of `text`, at most `limit` bytes.
///
/// A trailing unterminated line is excluded: a read boundary can split
/// `Connection to X closed by administrator.` right after `closed`, and that
/// prefix must not count as a teardown marker before the line is complete.
/// A leading line cut by the byte limit is excluded for the same reason.
fn recent_complete_lines(text: &str, limit: usize) -> &str {
    let Some(last_terminator) = text.rfind(is_line_terminator) else {
        return "";
    };
    let terminated = &text[..=last_terminator];
    if terminated.len() <= limit {
        return terminated;
    }
    let recent = recent_chars(terminated, limit);
    let start = terminated.len() - recent.len();
    if start == 0 || matches!(terminated.as_bytes()[start - 1], b'\n' | b'\r') {
        return recent;
    }
    recent
        .split_once(is_line_terminator)
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

    // A detection pass that changes nothing must not leave its examined lines
    // in the window, or every later read rescans up to 64 KiB of old output.
    #[test]
    fn no_change_detection_does_not_rescan_examined_lines() {
        let store = ProfileStore::builtin();
        let filler = "plain log line without vendor words 0123456789\n".repeat(2048);

        // Generic-only detection.
        let mut runtime = ProfileRuntime::new(names(&["generic"]));
        assert_eq!(runtime.observe_output(filler.as_bytes(), &store), None);
        assert!(runtime.output_window.is_empty());
        assert!(runtime.output_window_lower.is_empty());

        // Weak hints with no specific profile active.
        let mut runtime = ProfileRuntime::new(names(&["generic"]));
        assert_eq!(
            runtime.observe_output(b"Gi1/0/1 is up, line protocol is up\n", &store),
            None
        );
        assert!(runtime.output_window.is_empty());

        // Detection that only confirms the active profile.
        let mut runtime = ProfileRuntime::new(names(&["generic", "juniper"]));
        assert_eq!(runtime.observe_output(b"--- JUNOS 22.4R3\n", &store), None);
        assert!(runtime.output_window.is_empty());

        // An unterminated tail survives so a split banner still completes.
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        assert_eq!(runtime.observe_output(filler.as_bytes(), &store), None);
        assert_eq!(runtime.observe_output(b"x\nCisco I", &store), None);
        assert_eq!(runtime.output_window, "Cisco I");
        assert_eq!(runtime.output_window_lower, "cisco i");
        assert_eq!(
            runtime.observe_output(b"OS Software\n", &store),
            Some(names(&["generic", "cisco"]))
        );

        // Output without line terminators keeps only a bounded tail.
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        assert_eq!(runtime.observe_output(&[b'a'; 70_000], &store), None);
        assert!(runtime.output_window.len() <= super::DETECTION_BYTE_INTERVAL);
        assert_eq!(
            runtime.output_window.len(),
            runtime.output_window_lower.len()
        );

        // An armed remote attempt at an unrecognised prompt stays bounded too,
        // although every echoed keystroke runs a detection pass.
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        runtime.observe_input(b"ssh web01\r");
        assert_eq!(runtime.observe_output(filler.as_bytes(), &store), None);
        assert_eq!(runtime.observe_output(b"[admin@web01 ~]$ ", &store), None);
        assert_eq!(runtime.observe_output(b"l", &store), None);
        assert!(runtime.pending_remote.is_some());
        assert_eq!(runtime.output_window, "[admin@web01 ~]$ l");
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

    // These commands never open an interactive session, so nothing would ever
    // close a frame pushed for them.
    #[test]
    fn non_interactive_remote_commands_do_not_leave_profile_frames() {
        let store = ProfileStore::builtin();
        for command in [
            "ssh backup01 uptime\r",
            "ssh backup01 -p 2222 uptime\r",
            "ssh -V\r",
            "ssh -G router-a\r",
            "ssh -fN -L 8080:localhost:80 jump\r",
            "ssh -O check router-a\r",
            "ssh -Q cipher\r",
            "ssh -T git@git.example.com\r",
            "screen -ls\r",
        ] {
            let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
            runtime.observe_input(command.as_bytes());
            assert_eq!(runtime.pending_remote, None, "{command:?}");
            runtime.observe_output(b"some output\r\nuser@laptop:~$ ", &store);
            assert_eq!(runtime.stack_len(), 0, "{command:?} left a profile frame");
            runtime.observe_input(b"fw\r");
            assert_eq!(
                runtime.observe_output(b"fw\r\nFW-EDGE # ", &store),
                Some(names(&["generic", "fortinet"])),
                "{command:?}"
            );
            assert_eq!(
                runtime.observe_output(b"Connection to fw closed.\n", &store),
                Some(names(&["generic", "linux-unix"])),
                "{command:?}"
            );
        }
    }

    // A network device's own ssh takes keywords after the host, so a nested
    // jump typed there still opens a session that closes again.
    #[test]
    fn device_ssh_keywords_after_the_host_still_track_the_nested_session() {
        let store = ProfileStore::builtin();
        for (login, outer, jump, banner, nested, close) in [
            (
                &b"--- JUNOS 22.4R3 Kernel 64-bit\r\nlab@mx1> "[..],
                "juniper",
                &b"ssh 192.0.2.1 routing-instance mgmt_junos\r"[..],
                &b"Cisco IOS Software, C3750E Software\r\ncore-sw#"[..],
                "cisco",
                &b"Connection to 192.0.2.1 closed.\r\nlab@mx1> "[..],
            ),
            (
                b"Cisco Nexus Operating System (NX-OS) Software\r\ncore-sw# ",
                "cisco",
                b"ssh admin@192.0.2.5 vrf management\r",
                b"--- JUNOS 22.4R3 Kernel 64-bit\r\nlab@mx1> ",
                "juniper",
                b"Connection to 192.0.2.5 closed.\r\ncore-sw# ",
            ),
        ] {
            let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
            runtime.observe_input(b"ssh outer\r");
            assert_eq!(
                runtime.observe_output(login, &store),
                Some(names(&["generic", outer]))
            );
            runtime.accept_transition();
            runtime.observe_input(jump);
            assert!(
                runtime.pending_remote.is_some(),
                "{:?}",
                String::from_utf8_lossy(jump)
            );
            assert_eq!(
                runtime.observe_output(banner, &store),
                Some(names(&["generic", nested]))
            );
            runtime.accept_transition();
            assert_eq!(runtime.stack_len(), 2);
            assert_eq!(
                runtime.observe_output(close, &store),
                Some(names(&["generic", outer]))
            );
            assert_eq!(runtime.stack_len(), 1);
        }
    }

    // OpenSSH parses options again after the host, and a forced tty keeps a
    // remote command interactive.
    #[test]
    fn interactive_ssh_commands_keep_their_close_target() {
        for line in [
            "ssh router-a -p 2222\r",
            "ssh router-a -l admin -o ServerAliveInterval=30\r",
            "ssh -t router-a ssh core-sw\r",
            "ssh router-a -tt ssh core-sw\r",
            "ssh -vt router-a show version\r",
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

        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        runtime.observe_input(b"ssh -t jump ssh router-a\r");
        assert_eq!(
            runtime.observe_output(b"Cisco IOS Software\n", &store),
            Some(names(&["generic", "cisco"]))
        );
        assert_eq!(
            runtime.observe_output(b"Connection to jump closed.\n", &store),
            Some(names(&["generic", "linux-unix"]))
        );
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

    // Some devices end lines with a bare carriage return; a CR-terminated
    // marker is complete, and a CR-split benign line still is not.
    #[test]
    fn carriage_return_terminates_close_marker_lines() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh router-a\r");
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\r", &store),
            Some(names(&["generic", "juniper"]))
        );

        assert_eq!(
            runtime.observe_output(b"Connection to router-a closed", &store),
            None
        );
        assert_eq!(
            runtime.observe_output(b" by administrator.\r", &store),
            None
        );
        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));

        assert_eq!(
            runtime.observe_output(b"Connection to router-a closed.\r", &store),
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

    // A prompt-only device prints its second prompt only after Enter, so
    // that keystroke must not drop the typed remote hint.
    #[test]
    fn nested_prompt_only_device_survives_the_enter_that_redraws_its_prompt() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh mx1\r");
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 21.4R3.15 Kernel 64-bit\r\nlab@mx1> ", &store),
            Some(names(&["generic", "juniper"]))
        );
        runtime.accept_transition();
        runtime.observe_input(b"ssh core-sw\r");
        assert_eq!(runtime.observe_output(b"\r\ncore-sw#", &store), None);
        runtime.observe_input(b"\r");
        assert_eq!(
            runtime.observe_output(b"\r\ncore-sw#", &store),
            Some(names(&["generic", "cisco"]))
        );
        runtime.accept_transition();
        assert_eq!(
            runtime.observe_output(b"Connection to core-sw closed.\r\n", &store),
            Some(names(&["generic", "juniper"]))
        );
    }

    #[test]
    fn password_line_keeps_the_ssh_close_target() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh mx1\r");
        assert_eq!(
            runtime.observe_output(b"lab@mx1's password: ", &store),
            None
        );
        runtime.observe_input(b"hunter2\r");
        assert_eq!(
            runtime.observe_output(
                b"\r\n--- JUNOS 21.4R3.15 Kernel 64-bit\r\nlab@mx1> ",
                &store
            ),
            Some(names(&["generic", "juniper"]))
        );
        runtime.accept_transition();
        assert_eq!(
            runtime
                .stack
                .last()
                .and_then(|frame| frame.target.as_deref()),
            Some("mx1")
        );

        // An attempt that no output resolves expires after a few lines.
        runtime.observe_input(b"ssh core-sw\r");
        for line in [
            &b"show clock\r"[..],
            b"exit\r",
            b"show version\r",
            b"show route\r",
        ] {
            runtime.observe_input(line);
            assert!(runtime.pending_remote.is_some());
        }
        runtime.observe_input(b"show arp\r");
        assert_eq!(runtime.pending_remote, None, "a stale attempt must expire");

        assert_eq!(
            runtime.observe_output(b"Connection to core-sw closed.\r\nlab@mx1> ", &store),
            None
        );
        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));
        assert_eq!(
            runtime.observe_output(b"exit\r\n\r\nConnection to mx1 closed.\r\n", &store),
            Some(names(&["generic", "linux-unix"]))
        );
    }

    // Compiler arrows, `C#` and `word # comment` lines look like vendor
    // prompts, but one such line of local output must not switch the shell.
    #[test]
    fn single_prompt_shaped_local_lines_do_not_switch_local_baseline() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        for chunk in [
            &b"user@host:~$ "[..],
            b"cargo build\r\nwarning: unused variable: `x`\r\n  --> src/main.rs:3:9\r\n",
            b"user@host:~$ ",
            b"echo 'C# rocks'\r\nC# rocks\r\n",
            b"user@host:~$ ",
            b"printf 'C# rocks\\ndone\\n'\r\nC# rocks\r\n",
            b"done\r\n",
            b"user@host:~$ ",
            b"cat build.sh\r\nfor target in a b; do\r\n  make \"$target\"\r\ndone # build every target\r\n",
            b"user@host:~$ ",
        ] {
            assert_eq!(
                runtime.observe_output(chunk, &store),
                None,
                "{:?}",
                String::from_utf8_lossy(chunk)
            );
        }
        assert_eq!(runtime.active_profiles(), names(&["generic", "linux-unix"]));
        assert_eq!(runtime.stack_len(), 0);

        // A Fortinet prompt waiting for input still switches on first sight.
        runtime.observe_input(b"fw\r");
        assert_eq!(
            runtime.observe_output(b"fw\r\nFW-EDGE # ", &store),
            Some(names(&["generic", "fortinet"]))
        );
    }

    // `sftp> ` and `mysql> ` have the shape of a device prompt. Started by
    // name from the local shell, the client is known, and its prompt is its
    // own: the local shell used to become a Cisco session and stay one.
    #[test]
    fn local_sftp_and_mysql_prompts_are_not_device_prompts() {
        let store = ProfileStore::builtin();
        for (launch, banner, prompt, command, leave) in [
            ("sftp host", "Connected to host.", "sftp> ", "ls", "bye"),
            (
                "mysql -u root",
                "Welcome to the MySQL monitor.  Commands end with ; or \\g.",
                "mysql> ",
                "show databases;",
                "quit",
            ),
        ] {
            let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
            assert_eq!(runtime.observe_output(b"user@laptop:~$ ", &store), None);
            runtime.observe_input(format!("{launch}\r").as_bytes());
            assert_eq!(
                runtime.observe_output(
                    format!("{launch}\r\n{banner}\r\n{prompt}").as_bytes(),
                    &store
                ),
                None,
                "{launch}"
            );
            for _ in 0..2 {
                runtime.observe_input(format!("{command}\r").as_bytes());
                assert_eq!(
                    runtime.observe_output(
                        format!("{command}\r\nfirst  second\r\n{prompt}").as_bytes(),
                        &store
                    ),
                    None,
                    "{launch}"
                );
            }
            assert_eq!(runtime.active_profiles(), names(&["generic", "linux-unix"]));
            assert_eq!(runtime.stack_len(), 0, "{launch}");

            runtime.observe_input(format!("{leave}\r").as_bytes());
            assert_eq!(
                runtime.observe_output(format!("{leave}\r\nuser@laptop:~$ ").as_bytes(), &store),
                None,
                "{launch}"
            );
            assert_eq!(runtime.active_profiles(), names(&["generic", "linux-unix"]));
            assert_eq!(runtime.stack_len(), 0, "{launch}");
            assert_eq!(runtime.local_client, None, "{launch}");

            // The local shell still finds a device behind a wrapper command.
            runtime.observe_input(b"fw\r");
            assert_eq!(
                runtime.observe_output(b"fw\r\nFW-EDGE # ", &store),
                Some(names(&["generic", "fortinet"])),
                "{launch}"
            );
        }
    }

    // The same prompt from a device still counts: a host named like the
    // client and reached with a typed ssh, and any other prompt while the
    // client is remembered.
    #[test]
    fn a_remembered_local_client_hides_only_its_own_prompt() {
        let store = ProfileStore::builtin();

        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        runtime.observe_input(b"ssh mysql\r");
        assert_eq!(runtime.local_client, None);
        assert_eq!(runtime.observe_output(b"\r\nmysql> ", &store), None);
        runtime.observe_input(b"\r");
        assert_eq!(
            runtime.observe_output(b"\r\nmysql> ", &store),
            Some(names(&["generic", "cisco"]))
        );
        runtime.accept_transition();
        runtime.observe_input(b"exit\r");
        assert_eq!(
            runtime.observe_output(b"exit\r\nConnection to mysql closed.\r\n", &store),
            Some(names(&["generic", "linux-unix"]))
        );

        // A local prompt the profile does not recognize never ends the
        // client, so it is still remembered when a device is reached.
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        runtime.observe_input(b"mysql\r");
        assert_eq!(
            runtime.observe_output(
                b"mysql\r\nERROR 2002 (HY000): Can't connect to local MySQL server\r\n\xe2\x9d\xaf ",
                &store
            ),
            None
        );
        assert_eq!(runtime.local_client, Some(super::LocalClient::Mysql));
        runtime.observe_input(b"sw\r");
        assert_eq!(runtime.observe_output(b"sw\r\nsw1#", &store), None);
        runtime.observe_input(b"show clock\r");
        assert_eq!(
            runtime.observe_output(b"show clock\r\n12:00:00.000 UTC\r\nsw1#", &store),
            Some(names(&["generic", "cisco"]))
        );
        assert_eq!(
            runtime.local_client, None,
            "a new connection ends the client"
        );
    }

    #[test]
    fn a_local_client_is_remembered_only_from_its_own_name_at_the_local_shell() {
        let store = ProfileStore::builtin();
        for (line, client) in [
            ("sftp host", Some(super::LocalClient::Sftp)),
            ("  sftp -P 2222 user@host", Some(super::LocalClient::Sftp)),
            ("mysql", Some(super::LocalClient::Mysql)),
            ("sudo mysql", None),
            ("mysqldump db", None),
            ("man sftp", None),
            ("SFTP host", None),
        ] {
            let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
            runtime.observe_input(format!("{line}\r").as_bytes());
            assert_eq!(runtime.local_client, client, "{line:?}");
        }

        // An edited line is not known, and a typed ssh is a connection.
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        runtime.observe_input(b"sft\tp host\r");
        assert_eq!(runtime.local_client, None);
        runtime.observe_input(b"mysql\r");
        runtime.observe_input(b"ssh sw1\r");
        assert_eq!(runtime.local_client, None);

        // Inside a connection the shell is not the local one.
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        runtime.observe_input(b"ssh router-a\r");
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\r\nuser@router-a> ", &store),
            Some(names(&["generic", "juniper"]))
        );
        runtime.accept_transition();
        runtime.observe_input(b"mysql\r");
        assert_eq!(runtime.local_client, None);
    }

    // Neighbor tables (LLDP/CDP) print other vendors' banners as command
    // output once a banner-identified device is at its prompt.
    #[test]
    fn locked_remote_ignores_vendor_strong_signals_in_neighbor_output() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh mx1\r");
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
            Some(names(&["generic", "juniper"]))
        );
        runtime.accept_transition();
        assert_eq!(runtime.observe_output(b"lab@mx1> ", &store), None);
        runtime.observe_input(b"show lldp neighbors interface ge-0/0/0\r");
        assert_eq!(
            runtime.observe_output(
                b"show lldp neighbors interface ge-0/0/0\r\nSystem Description : Cisco IOS Software, C3750E Software (C3750E-UNIVERSALK9-M), Version 15.0(2)SE11\r\n\r\nlab@mx1> ",
                &store
            ),
            None
        );
        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));
        assert_eq!(
            runtime.observe_output(b"Connection to mx1 closed.\n", &store),
            Some(names(&["generic", "linux-unix"]))
        );
    }

    #[test]
    fn locked_remote_with_banner_and_prompt_in_one_read_ignores_neighbor_output() {
        let store = ProfileStore::builtin();
        for (neighbor, profile) in [
            (
                &b"System Description: \r\nJuniper Networks, Inc. mx480 internet router, kernel JUNOS 18.4R1-S5\r\ncore-sw1#"[..],
                "juniper",
            ),
            (b"Gi1/0/2  up  up  ubuntu-build01 eth0\r\ncore-sw1#", "linux-unix"),
        ] {
            let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
            runtime.observe_input(b"ssh core-sw1\r");
            assert_eq!(
                runtime.observe_output(
                    b"Cisco Nexus Operating System (NX-OS) Software\r\ncore-sw1# ",
                    &store
                ),
                Some(names(&["generic", "cisco"]))
            );
            runtime.accept_transition();
            runtime.observe_input(b"show lldp neighbors detail\r");
            assert_eq!(
                runtime.observe_output(neighbor, &store),
                None,
                "{profile} neighbor output switched the profile"
            );
            assert_eq!(runtime.active_profiles(), names(&["generic", "cisco"]));
            assert_eq!(runtime.stack_len(), 1);

            // A typed remote command still lets the next banner switch.
            runtime.observe_input(b"ssh mx1\r");
            assert_eq!(
                runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
                Some(names(&["generic", "juniper"]))
            );
            assert_eq!(runtime.stack_len(), 2);
        }
    }

    // A prompt alone cannot tell apart vendors that share a prompt shape, so
    // `show version` output may still refine a prompt-detected profile.
    #[test]
    fn prompt_detected_remote_still_refines_from_show_version() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh lab-aruba-01\r");
        assert_eq!(runtime.observe_output(b"\r\nLAB-ARUBA-01# ", &store), None);
        runtime.observe_input(b"\r");
        assert_eq!(
            runtime.observe_output(b"\r\nLAB-ARUBA-01# ", &store),
            Some(names(&["generic", "cisco"]))
        );
        runtime.accept_transition();
        runtime.observe_input(b"show version\r");
        assert_eq!(
            runtime.observe_output(
                b"show version\r\nArubaOS-CX 10.13.1000\r\nLAB-ARUBA-01# ",
                &store
            ),
            Some(names(&["generic", "arubacx"]))
        );
        assert_eq!(runtime.stack_len(), 1);
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

    // Leaving two nested connections at once puts both teardown lines in one
    // read. The first one popped its connection and the second was dropped.
    #[test]
    fn two_nested_closes_in_one_read_unwind_both_connections() {
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
            runtime.observe_output(
                b"Connection to core-sw closed.\r\nConnection to router-a closed.\r\nuser@laptop:~$ ",
                &store
            ),
            Some(names(&["generic", "linux-unix"]))
        );
        assert_eq!(runtime.stack_len(), 0);
    }

    // A profile file may not set runtime metadata, so a profiles.d profile
    // that took a built-in's name, to change its colors, lost that vendor's
    // dynamic detection with no way to get it back.
    #[test]
    fn a_user_profile_that_shadows_a_builtin_keeps_its_dynamic_detection() {
        let mut store = ProfileStore::builtin();
        assert!(
            store
                .try_insert_user_profile(
                    "cisco".to_string(),
                    vec!["generic".to_string()],
                    Vec::new(),
                    Vec::new(),
                )
                .expect("metadata within limits"),
            "the user profile shadows the built-in"
        );

        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        runtime.observe_input(b"ssh core-sw\r");
        assert_eq!(
            runtime.observe_output(b"Cisco Nexus Operating System (NX-OS) Software\n", &store),
            Some(names(&["generic", "cisco"]))
        );
    }

    // One ssh exit prints two lines, `logout` and the close. With an alias
    // the close names the host, not what was typed, so `logout` is what ends
    // the inner connection; the close line must not go on to end the outer.
    #[test]
    fn one_teardown_of_two_lines_ends_one_connection() {
        let store = ProfileStore::builtin();
        for close in [
            "Connection to core-sw closed.",
            "Connection to 192.0.2.9 closed.",
        ] {
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
                runtime.observe_output(format!("logout\r\n{close}\r\n").as_bytes(), &store),
                Some(names(&["generic", "juniper"])),
                "{close}"
            );
            assert_eq!(runtime.stack_len(), 1, "{close}");
        }
    }

    // The close line after `logout` belongs to the same exit, also when the
    // host it names is the one the outer connection went to.
    #[test]
    fn a_close_line_after_logout_does_not_end_the_outer_connection_it_names() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        runtime.observe_input(b"ssh router-a\r");
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
            Some(names(&["generic", "juniper"]))
        );
        // An alias for the same router, typed on it.
        runtime.observe_input(b"ssh alias-a\r");
        runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store);
        assert_eq!(runtime.stack_len(), 2);

        runtime.observe_output(b"logout\r\nConnection to router-a closed.\r\n", &store);
        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));
        assert_eq!(runtime.stack_len(), 1);
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

    // The shell completes a TAB-finished word on screen, so the typed text
    // before the TAB is not the host that the close message will name.
    #[test]
    fn tab_completed_ssh_target_unwinds_on_the_completed_host_close() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh rou\t\r");
        assert_eq!(
            runtime.pending_remote,
            Some(super::RemoteAttempt {
                policy: super::ClosePolicy::SshLike,
                target: None,
            })
        );
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
            Some(names(&["generic", "juniper"]))
        );
        runtime.accept_transition();
        assert_eq!(
            runtime.observe_output(b"Connection to router-a closed.\n", &store),
            Some(names(&["generic", "linux-unix"]))
        );
        assert_eq!(runtime.stack_len(), 0);
    }

    // OpenSSH reports the HostName an ssh_config alias resolves to, not the
    // alias that was typed.
    #[test]
    fn ssh_alias_unwinds_on_its_hostname_close() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh rtr1\r");
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
            Some(names(&["generic", "juniper"]))
        );
        runtime.accept_transition();
        assert_eq!(
            runtime.observe_output(b"Connection to 192.0.2.1 closed.\n", &store),
            Some(names(&["generic", "linux-unix"]))
        );
        assert_eq!(runtime.stack_len(), 0);
    }

    // An alias session whose device no profile recognises closes under its
    // HostName while its attempt is still pending. That close ends the
    // attempt, or the next prompt records a connection that never closes.
    #[test]
    fn untracked_alias_close_ends_its_pending_attempt() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh web\r");
        assert_eq!(
            runtime.observe_output(b"ssh web\r\n[admin@web01 ~]$ ", &store),
            None
        );
        runtime.observe_input(b"uptime\r");
        runtime.observe_input(b"exit\r");
        assert_eq!(
            runtime.observe_output(
                b"exit\r\nConnection to 192.0.2.10 closed.\r\nuser@laptop:~$ ",
                &store
            ),
            None
        );
        assert_eq!(runtime.pending_remote, None);
        assert_eq!(runtime.observe_output(b"user@laptop:~$ ", &store), None);
        assert_eq!(runtime.stack_len(), 0);
        runtime.observe_input(b"fw\r");
        assert_eq!(
            runtime.observe_output(b"fw\r\nFW-EDGE # ", &store),
            Some(names(&["generic", "fortinet"]))
        );

        // Nested in a tracked connection, the parent stays open.
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));
        runtime.observe_input(b"ssh router-a\r");
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\r\nlab@mx1> ", &store),
            Some(names(&["generic", "juniper"]))
        );
        runtime.accept_transition();
        runtime.observe_input(b"ssh web\r");
        assert_eq!(
            runtime.observe_output(b"ssh web\r\n[admin@web01 ~]$ ", &store),
            None
        );
        runtime.observe_input(b"exit\r");
        assert_eq!(
            runtime.observe_output(
                b"exit\r\nConnection to 192.0.2.10 closed.\r\nlab@mx1> ",
                &store
            ),
            None
        );
        assert_eq!(runtime.pending_remote, None);
        assert_eq!(runtime.stack_len(), 1);
        assert_eq!(
            runtime.observe_output(b"exit\r\nConnection to router-a closed.\r\n", &store),
            Some(names(&["generic", "linux-unix"]))
        );
        assert_eq!(runtime.stack_len(), 0);
    }

    #[test]
    fn nested_ssh_alias_unwinds_only_its_own_connection() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh router-a\r");
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
            Some(names(&["generic", "juniper"]))
        );
        runtime.accept_transition();
        runtime.observe_input(b"ssh rtr2\r");
        assert_eq!(
            runtime.observe_output(b"Cisco IOS Software\n", &store),
            Some(names(&["generic", "cisco"]))
        );
        runtime.accept_transition();
        assert_eq!(runtime.stack_len(), 2);

        // A close naming the parent's target is not the child's alias close.
        assert_eq!(
            runtime.observe_output(b"Connection to router-a closed.\n", &store),
            None
        );
        assert_eq!(runtime.stack_len(), 2);

        assert_eq!(
            runtime.observe_output(b"Connection to 192.0.2.2 closed.\n", &store),
            Some(names(&["generic", "juniper"]))
        );
        runtime.accept_transition();
        assert_eq!(runtime.stack_len(), 1);
        // The parent saw a nested attempt, so only its own target closes it.
        assert_eq!(
            runtime.observe_output(b"Connection to 192.0.2.1 closed.\n", &store),
            None
        );
        assert_eq!(runtime.stack_len(), 1);
        assert_eq!(
            runtime.observe_output(b"Connection to router-a closed.\n", &store),
            Some(names(&["generic", "linux-unix"]))
        );
        assert_eq!(runtime.stack_len(), 0);
    }

    // Once a nested attempt was typed, a close naming another host may belong
    // to that attempt, so it must not unwind the parent connection.
    #[test]
    fn unmatched_close_after_a_nested_attempt_keeps_the_parent_connection() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh router-a\r");
        assert_eq!(
            runtime.observe_output(b"--- JUNOS 22.4R3 Kernel 64-bit\n", &store),
            Some(names(&["generic", "juniper"]))
        );
        runtime.accept_transition();
        runtime.observe_input(b"ssh hostb\r");
        assert_eq!(runtime.observe_output(b"hello\n", &store), None);
        for line in [
            &b"show version\r"[..],
            b"show route\r",
            b"show arp\r",
            b"show log\r",
            b"show chassis\r",
        ] {
            runtime.observe_input(line);
        }
        assert_eq!(runtime.pending_remote, None);

        assert_eq!(
            runtime.observe_output(b"Connection to hostb closed.\n", &store),
            None
        );
        assert_eq!(runtime.active_profiles(), names(&["generic", "juniper"]));
        assert_eq!(runtime.stack_len(), 1);
        assert_eq!(
            runtime.observe_output(b"Connection to router-a closed.\n", &store),
            Some(names(&["generic", "linux-unix"]))
        );
        assert_eq!(runtime.stack_len(), 0);
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

/// Leaving a nested device: C-2b (`exit`, a device's own bracketed close and
/// the parent's waiting prompt) and C-2c (Ctrl-], `quit`, telnet's own echo
/// and close line). Witnesses from design reviews 44, 54 and 68, replayed at
/// the runtime's seam the way a PTY delivers them.
#[cfg(test)]
mod leave_tests {
    use super::ProfileRuntime;
    use crate::profiles::ProfileStore;

    const LOCAL: &[&str] = &["generic", "linux-unix"];
    const CISCO: &[&str] = &["generic", "cisco"];
    const JUNIPER: &[&str] = &["generic", "juniper"];
    const ARISTA: &[&str] = &["generic", "arista"];
    const BRACKET: &str = "[Connection to sw2 closed by foreign host]";

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    /// Output as a PTY passes it on: `\n` becomes `\r\n`.
    fn crlf(text: &str) -> String {
        text.replace("\r\n", "\n").replace('\n', "\r\n")
    }

    /// How a terminal in canonical mode echoes typed bytes: control bytes as
    /// `^X`.
    fn echo(typed: &str) -> String {
        typed
            .chars()
            .map(|ch| match ch {
                ch if (ch as u32) < 0x20 => format!("^{}", ((ch as u8) + 0x40) as char),
                ch => ch.to_string(),
            })
            .collect()
    }

    /// A local shell waiting at `user@laptop:~$ `.
    fn local(store: &ProfileStore) -> ProfileRuntime {
        let mut runtime = ProfileRuntime::new(names(LOCAL));
        runtime.observe_output(b"user@laptop:~$ ", store);
        runtime
    }

    /// Types `typed` and Enter, the way a person does: the keys are read
    /// with their echo, Enter with its line end. Then each chunk of the reply
    /// arrives in its own read. Returns the last profile change.
    fn step(
        runtime: &mut ProfileRuntime,
        store: &ProfileStore,
        typed: &str,
        reply: &[&str],
    ) -> Option<Vec<String>> {
        let mut last = None;
        runtime.observe_input(typed.as_bytes());
        last = runtime
            .observe_output(echo(typed).as_bytes(), store)
            .or(last);
        runtime.observe_input(b"\r");
        last = runtime.observe_output(b"\r\n", store).or(last);
        for chunk in reply {
            last = runtime
                .observe_output(crlf(chunk).as_bytes(), store)
                .or(last);
        }
        last
    }

    /// A raw-mode program: it echoes nothing itself.
    fn step_raw(
        runtime: &mut ProfileRuntime,
        store: &ProfileStore,
        keys: &[u8],
        reply: &[&str],
    ) -> Option<Vec<String>> {
        let mut last = None;
        runtime.observe_input(keys);
        for chunk in reply {
            last = runtime.observe_output(chunk.as_bytes(), store).or(last);
        }
        last
    }

    /// `ssh sw1` (Cisco, `sw1#`), then `ssh -l admin sw2` (Junos).
    fn cisco_then_juniper(store: &ProfileStore) -> ProfileRuntime {
        let mut runtime = local(store);
        assert_eq!(
            step(
                &mut runtime,
                store,
                "ssh sw1",
                &["Cisco IOS Software\nsw1#"]
            ),
            Some(names(CISCO))
        );
        assert_eq!(
            step(
                &mut runtime,
                store,
                "ssh -l admin sw2",
                &["--- JUNOS 22.4R3 Kernel 64-bit\nlab@sw2> "]
            ),
            Some(names(JUNIPER))
        );
        runtime
    }

    /// The same, with a nested shell entered on sw2 first.
    fn cisco_then_juniper_shell(store: &ProfileStore) -> ProfileRuntime {
        let mut runtime = cisco_then_juniper(store);
        step(&mut runtime, store, "start shell", &["[admin@sw2 ~]$ "]);
        runtime
    }

    /// Types `exit` and feeds `reply` (already in PTY form) split into reads
    /// at `splits`.
    fn exit_with(
        runtime: &mut ProfileRuntime,
        store: &ProfileStore,
        reply: &str,
        splits: &[usize],
    ) -> Option<Vec<String>> {
        let mut last = None;
        runtime.observe_input(b"exit");
        runtime.observe_output(b"exit", store);
        runtime.observe_input(b"\r");
        let bytes = reply.as_bytes();
        let mut start = 0;
        for &end in splits.iter().chain(std::iter::once(&bytes.len())) {
            last = runtime.observe_output(&bytes[start..end], store).or(last);
            start = end;
        }
        last
    }

    /// A key typed at the waiting prompt, read with its echo.
    fn key(runtime: &mut ProfileRuntime, store: &ProfileStore) -> Option<Vec<String>> {
        runtime.observe_input(b"s");
        runtime.observe_output(b"s", store)
    }

    /// Asserts the child is still the active connection and answers a
    /// command in its own colors.
    fn assert_child_kept(runtime: &mut ProfileRuntime, store: &ProfileStore, child: &[&str]) {
        step(
            runtime,
            store,
            "show version",
            &["Hostname: sw2\nlab@sw2> "],
        );
        assert_eq!(runtime.active_profiles(), names(child));
    }

    fn genuine_reply() -> String {
        format!("\r\n{BRACKET}\r\nsw1#")
    }

    // C-2b, genuine closes.

    #[test]
    fn c2b_parent_prompt_waiting_after_the_bracket_returns_at_the_next_key() {
        let store = ProfileStore::builtin();
        let mut runtime = cisco_then_juniper(&store);
        assert_eq!(exit_with(&mut runtime, &store, &genuine_reply(), &[]), None);
        // The parent prompt waits; the key typed at it proves it.
        assert_eq!(key(&mut runtime, &store), Some(names(CISCO)));
        assert_eq!(runtime.stack_len(), 1);
        step(
            &mut runtime,
            &store,
            "how version",
            &["Hostname: sw1\nsw1#"],
        );
        assert_eq!(runtime.active_profiles(), names(CISCO));
    }

    #[test]
    fn c2b_genuine_close_bytes_split_at_every_position_return_alike() {
        let store = ProfileStore::builtin();
        let reply = genuine_reply();
        for split in 1..reply.len() {
            let mut runtime = cisco_then_juniper(&store);
            assert_eq!(
                exit_with(&mut runtime, &store, &reply, &[split]),
                None,
                "split at {split}"
            );
            assert_eq!(
                key(&mut runtime, &store),
                Some(names(CISCO)),
                "split at {split}"
            );
            assert_eq!(runtime.stack_len(), 1, "split at {split}");
        }
    }

    #[test]
    fn c2b_nested_exits_return_one_level_each() {
        let store = ProfileStore::builtin();
        let mut runtime = cisco_then_juniper(&store);
        assert_eq!(
            step(
                &mut runtime,
                &store,
                "ssh leaf3",
                &["Arista Networks EOS version 4.31\nleaf3#"]
            ),
            Some(names(ARISTA))
        );
        exit_with(
            &mut runtime,
            &store,
            "\r\n[Connection to leaf3 closed by foreign host]\r\nlab@sw2> ",
            &[],
        );
        assert_eq!(key(&mut runtime, &store), Some(names(JUNIPER)));
        runtime.observe_input(b"\x7f");
        runtime.observe_output(b"\x08 \x08", &store);
        exit_with(&mut runtime, &store, &genuine_reply(), &[]);
        assert_eq!(key(&mut runtime, &store), Some(names(CISCO)));
        assert_eq!(runtime.stack_len(), 1);
    }

    #[test]
    fn c2b_parent_prompt_split_across_reads_is_the_parent_prompt() {
        // The read loop passes visible bytes, so a colored prompt arrives
        // here as its text; the scenario runs cover the colors.
        let store = ProfileStore::builtin();
        // The prompt itself split across reads.
        let mut runtime = cisco_then_juniper(&store);
        exit_with(
            &mut runtime,
            &store,
            &format!("\r\n{BRACKET}\r\nsw1#"),
            &[47],
        );
        assert_eq!(key(&mut runtime, &store), Some(names(CISCO)));
    }

    #[test]
    fn c2b_complete_parent_prompt_line_followed_by_output_returns_at_once() {
        let store = ProfileStore::builtin();
        let filler = format!("{}\r\n", "x".repeat(380));
        for splits in [&[][..], &[50][..]] {
            let mut runtime = cisco_then_juniper(&store);
            assert_eq!(
                exit_with(
                    &mut runtime,
                    &store,
                    &format!("\r\n{BRACKET}\r\nsw1#\r\n{filler}"),
                    splits
                ),
                Some(names(CISCO)),
                "{splits:?}"
            );
        }
    }

    // C-2b, the unfinished line (review 68, must-change 1).

    #[test]
    fn c2b_unfinished_line_that_starts_like_the_parent_prompt_is_not_it() {
        let store = ProfileStore::builtin();
        let reply = format!("\r\n{BRACKET}\r\nsw1#backup\r\nlab@sw2> ");
        let prefix_end = reply.find("backup").expect("prefix");
        let mut splits: Vec<Vec<usize>> = vec![vec![], vec![prefix_end]];
        splits.extend((1..reply.len()).map(|split| vec![split]));
        for split in splits {
            let mut runtime = cisco_then_juniper_shell(&store);
            assert_eq!(
                exit_with(&mut runtime, &store, &reply, &split),
                None,
                "split at {split:?}"
            );
            assert_child_kept(&mut runtime, &store, JUNIPER);
            assert_eq!(runtime.stack_len(), 2, "split at {split:?}");
        }
    }

    #[test]
    fn c2b_key_typed_while_the_line_still_goes_on_is_not_a_waiting_prompt() {
        let store = ProfileStore::builtin();
        let mut runtime = cisco_then_juniper_shell(&store);
        exit_with(&mut runtime, &store, &genuine_reply(), &[]);
        // A key typed while `sw1#` is the last output, but the next output
        // is the rest of a line still in flight, not the key's echo.
        runtime.observe_input(b"s");
        assert_eq!(
            runtime.observe_output(b"backup\r\nlab@sw2> s", &store),
            None
        );
        assert_child_kept(&mut runtime, &store, JUNIPER);
        assert_eq!(runtime.stack_len(), 2);
    }

    #[test]
    fn c2b_pasted_exit_returns_like_a_typed_one() {
        // The paste-end marker comes after the Enter inside the paste; it is
        // not a key typed ahead of the output.
        let store = ProfileStore::builtin();
        let mut runtime = cisco_then_juniper(&store);
        runtime.observe_input(b"\x1b[200~exit\r\x1b[201~");
        runtime.observe_output(format!("exit{}", genuine_reply()).as_bytes(), &store);
        assert_eq!(key(&mut runtime, &store), Some(names(CISCO)));
    }

    // C-2b, the 512-byte bound (review 68, must-change 2).

    /// The reply whose parent prompt ends at byte `end` after the echo of
    /// the exit line, with `filler` repeated on a line between.
    fn reply_ending_at(end: usize, filler: &str) -> String {
        let head = format!("{BRACKET}\r\n");
        let tail = "\r\nsw1#";
        let room = end - head.len() - tail.len();
        let mut line = filler.repeat(room / filler.len());
        line.push_str(&"x".repeat(room - line.len()));
        let body = format!("{head}{line}{tail}");
        assert_eq!(body.len(), end);
        format!("\r\n{body}")
    }

    #[test]
    fn c2b_parent_prompt_must_end_within_512_bytes_in_any_read_grouping() {
        let store = ProfileStore::builtin();
        for filler in ["x", "\u{e9}", "\u{2500}"] {
            for (end, returns) in [(511, true), (512, true), (513, false)] {
                let reply = reply_ending_at(end, filler);
                let last = reply.len();
                for splits in [
                    &[][..],
                    &[3][..],
                    &[last - 4][..],
                    &[last - 1][..],
                    &[100, 300][..],
                ] {
                    let mut runtime = cisco_then_juniper(&store);
                    exit_with(&mut runtime, &store, &reply, splits);
                    let expected = returns.then(|| names(CISCO));
                    assert_eq!(
                        key(&mut runtime, &store),
                        expected,
                        "prompt ends at byte {end}, filler {filler:?}, splits {splits:?}"
                    );
                }
                // Typed fast or pasted: the Enter is read before the echo
                // of the word, which then arrives with the reply.
                let mut runtime = cisco_then_juniper(&store);
                runtime.observe_input(b"exit\r");
                runtime.observe_output(format!("exit{reply}").as_bytes(), &store);
                assert_eq!(
                    key(&mut runtime, &store),
                    returns.then(|| names(CISCO)),
                    "typed fast, prompt ends at byte {end}, filler {filler:?}"
                );
            }
        }
    }

    // C-2b, time (review 54, must-change 7): real time, as the rule uses it.

    fn close_shown_after(delay_ms: u64) -> Option<Vec<String>> {
        let store = ProfileStore::builtin();
        let mut runtime = cisco_then_juniper(&store);
        runtime.observe_input(b"exit");
        runtime.observe_output(b"exit", &store);
        runtime.observe_input(b"\r");
        runtime.observe_output(b"\r\n", &store);
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        runtime.observe_output(format!("{BRACKET}\r\nsw1#").as_bytes(), &store);
        key(&mut runtime, &store)
    }

    #[test]
    fn c2b_close_shown_within_three_seconds_returns() {
        assert_eq!(close_shown_after(2_500), Some(names(CISCO)));
    }

    #[test]
    fn c2b_close_shown_after_three_seconds_does_not_return() {
        assert_eq!(close_shown_after(3_500), None);
    }

    #[test]
    fn c2b_bracket_in_time_but_parent_prompt_after_three_seconds_does_not_return() {
        let store = ProfileStore::builtin();
        let mut runtime = cisco_then_juniper(&store);
        runtime.observe_input(b"exit");
        runtime.observe_output(b"exit", &store);
        runtime.observe_input(b"\r");
        runtime.observe_output(format!("\r\n{BRACKET}\r\n").as_bytes(), &store);
        std::thread::sleep(std::time::Duration::from_millis(3_200));
        runtime.observe_output(b"sw1#", &store);
        assert_eq!(key(&mut runtime, &store), None);
        assert_eq!(runtime.stack_len(), 2);
    }

    // C-2b, what must not return (pinned to 1.4.1: the child stays).

    #[test]
    fn c2b_child_output_without_the_whole_evidence_keeps_the_child() {
        let store = ProfileStore::builtin();
        for (name, shell, reply) in [
            // Review 44: a nested shell's logout text, then the child's prompt.
            (
                "subshell_logout",
                true,
                vec![format!("{BRACKET}\nlab@sw2> ")],
            ),
            (
                "delayed_log",
                true,
                vec!["lab@sw2> ".to_string(), format!("\n{BRACKET}\nlab@sw2> ")],
            ),
            // Review 54: the child's prompt before or after the bracket.
            (
                "child_prompt_before_bracket",
                true,
                vec![format!("lab@sw2> \n{BRACKET}\nsw1#")],
            ),
            (
                "child_prompt_after_bracket",
                true,
                vec![format!("{BRACKET}\nlab@sw2> \nsw1#")],
            ),
            (
                "child_prompt_split",
                false,
                vec![format!("{BRACKET}\nlab@sw2> "), "\nsw1#".to_string()],
            ),
            (
                "parent_prompt_changed",
                false,
                vec![format!("{BRACKET}\nsw1(config)#")],
            ),
            (
                "renamed_hostname",
                false,
                vec![format!("{BRACKET}\ncore1#")],
            ),
            (
                "show_run_hostname_only",
                true,
                vec![format!(
                    "{BRACKET}\nCurrent configuration:\nhostname sw1\nlab@sw2> "
                )],
            ),
            (
                "long_logout_banner",
                false,
                vec![format!("Goodbye\n{}\n{BRACKET}\nsw1#", "x".repeat(500))],
            ),
            // Review 68: the natural declines.
            (
                "natural_hostname_only",
                false,
                vec![format!("{BRACKET}\nhostname sw1\nlab@sw2> ")],
            ),
            ("natural_parent_only", false, vec!["sw1#".to_string()]),
            (
                "natural_wrong_bracket",
                false,
                vec!["[Connection to sw3 closed by foreign host]\nsw1#".to_string()],
            ),
        ] {
            let mut runtime = if shell {
                cisco_then_juniper_shell(&store)
            } else {
                cisco_then_juniper(&store)
            };
            let chunks: Vec<&str> = reply.iter().map(String::as_str).collect();
            assert_eq!(step(&mut runtime, &store, "exit", &chunks), None, "{name}");
            assert_child_kept(&mut runtime, &store, JUNIPER);
            assert_eq!(runtime.stack_len(), 2, "{name}");
        }
    }

    #[test]
    fn c2b_bracket_before_the_exit_is_stale() {
        let store = ProfileStore::builtin();
        let mut runtime = cisco_then_juniper(&store);
        step(
            &mut runtime,
            &store,
            "show run",
            &[&format!("{BRACKET}\nlab@sw2> ")],
        );
        assert_eq!(step(&mut runtime, &store, "exit", &["sw1#"]), None);
        assert_child_kept(&mut runtime, &store, JUNIPER);
    }

    #[test]
    fn c2b_typeahead_and_same_hostname_children_keep_the_child() {
        let store = ProfileStore::builtin();
        for (name, leaf_banner) in [
            // A line typed into the child before any prompt of it.
            ("residual_typeahead", "Arista Networks EOS version 4.31\n"),
            // A child showing its parent's exact prompt.
            (
                "same_hostname_live",
                "Arista Networks EOS version 4.31\nRouter#",
            ),
        ] {
            let mut runtime = local(&store);
            step(
                &mut runtime,
                &store,
                "ssh core",
                &["Cisco IOS Software\nRouter#"],
            );
            assert_eq!(
                step(&mut runtime, &store, "ssh leaf", &[leaf_banner]),
                Some(names(ARISTA)),
                "{name}"
            );
            step(&mut runtime, &store, "bash", &["[admin@Router ~]$ "]);
            step(
                &mut runtime,
                &store,
                "exit",
                &["[Connection to leaf closed by foreign host]\nRouter#"],
            );
            step(
                &mut runtime,
                &store,
                "show version",
                &["Arista DCS-7050\nRouter#"],
            );
            assert_eq!(runtime.active_profiles(), names(ARISTA), "{name}");
            assert_eq!(runtime.stack_len(), 2, "{name}");
        }
    }

    #[test]
    fn c2b_compares_the_exact_parent_prompt_not_the_vendor() {
        let store = ProfileStore::builtin();
        let cisco_pair = |store: &ProfileStore| {
            let mut runtime = local(store);
            step(
                &mut runtime,
                store,
                "ssh sw1",
                &["Cisco IOS Software\nsw1#"],
            );
            step(
                &mut runtime,
                store,
                "ssh sw2",
                &["Cisco IOS Software\nsw2#"],
            );
            assert_eq!(runtime.stack_len(), 2);
            runtime
        };
        let mut runtime = cisco_pair(&store);
        exit_with(&mut runtime, &store, &format!("\r\n{BRACKET}\r\nsw2#"), &[]);
        key(&mut runtime, &store);
        assert_eq!(runtime.stack_len(), 2, "the child's own prompt came back");
        let mut runtime = cisco_pair(&store);
        exit_with(&mut runtime, &store, &genuine_reply(), &[]);
        key(&mut runtime, &store);
        assert_eq!(runtime.stack_len(), 1, "the parent's prompt came back");
    }

    #[test]
    fn c2b_parent_prompt_survives_an_edited_command_line() {
        let store = ProfileStore::builtin();
        let mut runtime = local(&store);
        step(
            &mut runtime,
            &store,
            "ssh sw1",
            &["Cisco IOS Software\nsw1#"],
        );
        runtime.observe_input(b"s");
        runtime.observe_output(b"s", &store);
        runtime.observe_input(b"sh sw3\x7f");
        runtime.observe_output(b"sh sw3\x08 \x08", &store);
        runtime.observe_input(b"2\r");
        assert_eq!(
            runtime.observe_output(b"2\r\n--- JUNOS 22.4R3 Kernel 64-bit\r\nlab@sw2> ", &store),
            Some(names(JUNIPER))
        );
        exit_with(&mut runtime, &store, &genuine_reply(), &[]);
        assert_eq!(key(&mut runtime, &store), Some(names(CISCO)));
    }

    // C-2b, the accepted limit: a still-connected child that prints the
    // whole evidence. These pin the early switch, so it cannot drift
    // silently.

    #[test]
    fn c2b_accepted_limit_child_printing_bracket_and_parent_prompt_line() {
        let store = ProfileStore::builtin();
        for (name, reply) in [
            (
                "saved_log_split_read",
                [format!("{BRACKET}\nsw1#"), "\nlab@sw2> ".to_string()],
            ),
            (
                "show_run_parent_line",
                [
                    format!("{BRACKET}\nCurrent configuration:\nhostname sw1\nbanner motd ^\nsw1#"),
                    "\n^\nlab@sw2> ".to_string(),
                ],
            ),
        ] {
            let mut runtime = cisco_then_juniper_shell(&store);
            let chunks: Vec<&str> = reply.iter().map(String::as_str).collect();
            assert_eq!(
                step(&mut runtime, &store, "exit", &chunks),
                Some(names(CISCO)),
                "{name}: the complete parent prompt line after the bracket"
            );
            assert_eq!(runtime.stack_len(), 1, "{name}");
        }
    }

    // C-2c, genuine closes.

    fn telnet_to_juniper(store: &ProfileStore, command: &str) -> ProfileRuntime {
        let mut runtime = local(store);
        assert_eq!(
            step(
                &mut runtime,
                store,
                command,
                &[
                    "Connected to r1.\nEscape character is '^]'.\n--- JUNOS 22.4R3 Kernel 64-bit\nlab@r1> "
                ]
            ),
            Some(names(JUNIPER)),
            "{command}"
        );
        runtime
    }

    #[test]
    fn c2c_ctrl_bracket_quit_returns_to_the_profile_before_telnet() {
        let store = ProfileStore::builtin();
        for command in ["telnet r1", "telnet r1 2323", "telnet -l admin r1"] {
            let mut runtime = telnet_to_juniper(&store, command);
            assert_eq!(
                step(
                    &mut runtime,
                    &store,
                    "\x1dq",
                    &["\ntelnet> q\nConnection closed.\nuser@laptop:~$ "]
                ),
                Some(names(LOCAL)),
                "{command}"
            );
            assert_eq!(runtime.stack_len(), 0, "{command}");
        }
    }

    #[test]
    fn c2c_quit_pops_the_telnet_and_what_was_opened_inside_it() {
        let store = ProfileStore::builtin();
        let mut runtime = telnet_to_juniper(&store, "telnet a1");
        assert_eq!(
            step(&mut runtime, &store, "ssh b1", &["Cisco IOS Software\nb1#"]),
            Some(names(CISCO))
        );
        assert_eq!(
            step(
                &mut runtime,
                &store,
                "\x1dq",
                &["\ntelnet> q\nConnection closed.\nuser@laptop:~$ "]
            ),
            Some(names(LOCAL))
        );
        assert_eq!(runtime.stack_len(), 0);
    }

    #[test]
    fn c2c_escape_announcement_split_at_every_position_is_still_read() {
        let store = ProfileStore::builtin();
        let banner = crlf(
            "Trying 192.0.2.1...\nConnected to r1.\nEscape character is '^]'.\n--- JUNOS 22.4R3 Kernel 64-bit\nlab@r1> ",
        );
        let announcement = banner.find("Escape").expect("announcement");
        let announcement_end = banner.find("--- JUNOS").expect("banner");
        for split in announcement..announcement_end {
            let mut runtime = local(&store);
            runtime.observe_input(b"telnet r1\r");
            runtime.observe_output(b"telnet r1\r\n", &store);
            runtime.observe_output(&banner.as_bytes()[..split], &store);
            runtime.observe_output(&banner.as_bytes()[split..], &store);
            assert_eq!(
                runtime.active_profiles(),
                names(JUNIPER),
                "split at {split}"
            );
            assert_eq!(
                step(
                    &mut runtime,
                    &store,
                    "\x1dq",
                    &["\ntelnet> q\nConnection closed.\nuser@laptop:~$ "]
                ),
                Some(names(LOCAL)),
                "split at {split}"
            );
        }
    }

    #[test]
    fn c2c_echo_and_close_line_bounds_are_counted_in_bytes() {
        let store = ProfileStore::builtin();
        // From the Ctrl-]: `^]` echo, then output in flight, then telnet's
        // `telnet> q` line, whose line end must come within 4,096 bytes.
        for (echo_end, returns) in [(4_095, true), (4_096, true), (4_097, false)] {
            let head = "^]";
            let echo_line = "\r\ntelnet> q\r";
            let room = echo_end - head.len() - echo_line.len();
            let flight = format!("{}\r\n", "y".repeat(room - 2));
            let output =
                format!("{head}{flight}{echo_line}\nConnection closed.\r\nuser@laptop:~$ ");
            assert_eq!(
                output.find("telnet> q\r").expect("echo") + "telnet> q\r".len(),
                echo_end
            );
            for splits in [&[][..], &[1_000][..], &[echo_end - 1][..]] {
                let mut runtime = telnet_to_juniper(&store, "telnet r1");
                runtime.observe_input(b"\x1dq\r");
                let expected = returns.then(|| names(LOCAL));
                assert_eq!(
                    step_split(&mut runtime, &store, &output, splits),
                    expected,
                    "echo ends at byte {echo_end}, splits {splits:?}"
                );
            }
        }
        // After the echo's line end, the close line must start within 256
        // bytes; blank lines may come between.
        for (start, returns) in [(255, true), (256, true), (257, false)] {
            let head = "^]\r\ntelnet> q\r";
            let blanks = format!("\n{}\n", " ".repeat(start - 2));
            assert_eq!(blanks.len(), start);
            let output = format!("{head}{blanks}Connection closed.\r\nuser@laptop:~$ ");
            for splits in [&[][..], &[head.len() + 10][..], &[head.len() + start][..]] {
                let mut runtime = telnet_to_juniper(&store, "telnet r1");
                runtime.observe_input(b"\x1dq\r");
                let expected = returns.then(|| names(LOCAL));
                assert_eq!(
                    step_split(&mut runtime, &store, &output, splits),
                    expected,
                    "close line starts {start} bytes after the echo, splits {splits:?}"
                );
            }
        }
    }

    fn step_split(
        runtime: &mut ProfileRuntime,
        store: &ProfileStore,
        output: &str,
        splits: &[usize],
    ) -> Option<Vec<String>> {
        let bytes = output.as_bytes();
        let mut last = None;
        let mut start = 0;
        for &end in splits.iter().chain(std::iter::once(&bytes.len())) {
            last = runtime.observe_output(&bytes[start..end], store).or(last);
            start = end;
        }
        last
    }

    // C-2c, what must not return (pinned to 1.4.1).

    #[test]
    fn c2c_without_telnet_s_own_exchange_the_telnet_stays() {
        let store = ProfileStore::builtin();
        for (name, command, steps) in [
            (
                "no_telnet_command_prompt",
                "telnet r1",
                vec![("\x1dq", "Connection closed.\nlab@r1> ")],
            ),
            (
                "rlogin_escape",
                "telnet -r r1",
                vec![("\x1dq", "Connection closed.\nlab@r1> ")],
            ),
            (
                "other_escape",
                "telnet -e ^X r1",
                vec![("\x1dq", "\ntelnet> q\nConnection closed.\nlab@r1> ")],
            ),
            (
                "over_budget_same_read",
                "telnet r1",
                vec![("\x1dq", "\ntelnet> q\n<300>\nConnection closed.\nlab@r1> ")],
            ),
            (
                "telnet_subshell",
                "telnet r1",
                vec![
                    ("\x1d!", "\ntelnet> !\nuser@laptop:~$ "),
                    ("\x1dq", "\ntelnet> q\nConnection closed.\nuser@laptop:~$ "),
                ],
            ),
            (
                "suspended",
                "telnet r1",
                vec![
                    (
                        "\x1dz",
                        "\ntelnet> z\n[1]+  Stopped telnet r1\nuser@laptop:~$ ",
                    ),
                    ("\x1dq", "Connection closed.\nuser@laptop:~$ "),
                ],
            ),
            (
                "stale_close",
                "telnet r1",
                vec![
                    ("cat notes", "Connection closed.\nlab@r1> "),
                    ("\x1dq", "\ntelnet> q\nlab@r1> "),
                ],
            ),
            (
                "stale_partial",
                "telnet r1",
                vec![("cat notes", "Connection closed."), ("\x1dq", "\nlab@r1> ")],
            ),
            (
                "unsolicited_after_armed",
                "telnet r1",
                vec![
                    ("\x1dq", "\ntelnet> q\nNo connection to close.\nlab@r1> "),
                    ("cat notes", "Connection closed.\nlab@r1> "),
                ],
            ),
            (
                "close_command",
                "telnet r1",
                vec![("\x1dclose", "\ntelnet> close\nConnection closed.\ntelnet> ")],
            ),
        ] {
            let mut runtime = telnet_to_juniper(&store, command);
            for (typed, reply) in steps {
                let reply = reply.replace("<300>", &"x".repeat(300));
                step(&mut runtime, &store, typed, &[&reply]);
            }
            assert_eq!(runtime.active_profiles(), names(JUNIPER), "{name}");
            assert_eq!(runtime.stack_len(), 1, "{name}");
        }
    }

    #[test]
    fn c2c_telnet_started_with_another_escape_is_not_closed_by_ctrl_bracket() {
        let store = ProfileStore::builtin();
        for command in [
            "telnet -r r1",
            "telnet -E r1",
            "telnet -e ^X r1",
            "telnet -8e^X r1",
        ] {
            let mut runtime = telnet_to_juniper(&store, command);
            step(
                &mut runtime,
                &store,
                "\x1dq",
                &["\ntelnet> q\nConnection closed.\nuser@laptop:~$ "],
            );
            assert_eq!(runtime.active_profiles(), names(JUNIPER), "{command}");
        }
    }

    #[test]
    fn c2c_telnet_option_values_keep_the_target() {
        for (command, policy) in [
            ("telnet -e ^X r1", super::ClosePolicy::TelnetOtherEscape),
            ("telnet -l admin r1 2323", super::ClosePolicy::Telnet),
            ("telnet -r r1", super::ClosePolicy::TelnetOtherEscape),
            ("telnet -n trace.log r1", super::ClosePolicy::Telnet),
        ] {
            assert_eq!(
                super::remote_attempt(command, false),
                Some(super::RemoteAttempt {
                    policy,
                    target: Some("r1".to_string()),
                }),
                "{command}"
            );
        }
    }

    #[test]
    fn c2c_echo_and_close_line_split_across_reads() {
        let store = ProfileStore::builtin();
        let mut runtime = telnet_to_juniper(&store, "telnet r1");
        runtime.observe_input(b"\x1d");
        assert_eq!(runtime.observe_output(b"\r\ntel", &store), None);
        assert_eq!(runtime.observe_output(b"net> qu", &store), None);
        runtime.observe_input(b"qu\r");
        assert_eq!(runtime.observe_output(b"\r\nConnection clo", &store), None);
        assert_eq!(
            runtime.observe_output(b"sed.\r\n", &store),
            Some(names(LOCAL))
        );
    }

    #[test]
    fn c2c_nested_telnets_need_every_escape_announced() {
        let store = ProfileStore::builtin();
        for (inner_announcement, returns) in [("Escape character is '^X'.\n", true), ("", false)] {
            let mut runtime = telnet_to_juniper(&store, "telnet a1");
            step(
                &mut runtime,
                &store,
                "telnet b1",
                &[&format!(
                    "Connected to b1.\n{inner_announcement}Cisco IOS Software\nb1#"
                )],
            );
            assert_eq!(runtime.active_profiles(), names(CISCO));
            let quit = step(
                &mut runtime,
                &store,
                "\x1dq",
                &["\ntelnet> q\nConnection closed.\nuser@laptop:~$ "],
            );
            // The local client's Ctrl-] closes a1 and b1 inside it, when
            // every telnet said which escape it uses.
            assert_eq!(
                quit,
                returns.then(|| names(LOCAL)),
                "{inner_announcement:?}"
            );
        }
    }

    #[test]
    fn c2c_announced_other_escape_keeps_the_telnet_in_any_read_grouping() {
        let store = ProfileStore::builtin();
        for reply in [
            &["\ntelnet> q\nConnection closed.\nlab@r1> "][..],
            &["\ntel", "net> q\nConnec", "tion closed.\nlab@r1> "][..],
        ] {
            let mut runtime = local(&store);
            step(
                &mut runtime,
                &store,
                "telnet r1",
                &[
                    "Trying 192.0.2.1...\nConnected to r1.\nEscape character is '^X'.\n--- JUNOS 22.4R3 Kernel 64-bit\nlab@r1> ",
                ],
            );
            step(&mut runtime, &store, "\x1dq", reply);
            assert_eq!(runtime.active_profiles(), names(JUNIPER));
        }
    }

    #[test]
    fn c2c_telnet_echo_before_the_enter_is_not_the_echo() {
        // audit-echo-before-enter: a raw-mode program prints `telnet> q` at
        // the Ctrl-], before `q` and Enter are typed.
        let store = ProfileStore::builtin();
        let mut runtime = local(&store);
        runtime.observe_input(b"telnet r1\r");
        runtime.observe_output(
            b"\r\nConnected to r1.\r\nEscape character is '^]'.\r\n--- JUNOS 22.4R3 Kernel 64-bit\r\nlab@r1> ",
            &store,
        );
        step_raw(&mut runtime, &store, b"\x1d", &["\r\ntelnet> q\r\n"]);
        step_raw(
            &mut runtime,
            &store,
            b"q\r",
            &["\r\nConnection closed.\r\nlab@r1> "],
        );
        assert_eq!(runtime.active_profiles(), names(JUNIPER));
    }

    #[test]
    fn c2c_a_stack_change_earlier_in_the_exchange_cancels_it() {
        let store = ProfileStore::builtin();
        let two_telnets = |store: &ProfileStore| {
            let mut runtime = local(store);
            step(
                &mut runtime,
                store,
                "telnet a1",
                &[
                    "Connected to a1.\nEscape character is '^]'.\n--- JUNOS 22.4R3 Kernel 64-bit\nlab@a1> ",
                ],
            );
            assert_eq!(
                step(
                    &mut runtime,
                    store,
                    "telnet b1",
                    &["Connected to b1.\nEscape character is '^]'.\nCisco IOS Software\nb1#"]
                ),
                Some(names(CISCO))
            );
            runtime
        };
        // audit-wrong-frame: b1's close in its own read, then the exchange.
        let mut runtime = two_telnets(&store);
        step_raw(
            &mut runtime,
            &store,
            b"\x1d",
            &["\r\nConnection closed by foreign host.\r\n"],
        );
        step_raw(
            &mut runtime,
            &store,
            b"q\r",
            &["\r\ntelnet> q\r\nConnection closed.\r\nlab@a1> "],
        );
        assert_eq!(runtime.active_profiles(), names(JUNIPER), "separate reads");
        assert_eq!(runtime.stack_len(), 1, "separate reads");
        // same_read_transition: all of it in one read, and split anywhere.
        let output = "^]q\r\nConnection closed by foreign host.\r\ntelnet> q\r\nConnection closed.\r\nlab@a1> ";
        for split in 0..output.len() {
            let mut runtime = two_telnets(&store);
            runtime.observe_input(b"\x1dq\r");
            let splits: &[usize] = if split == 0 { &[] } else { &[split] };
            step_split(&mut runtime, &store, output, splits);
            assert_eq!(
                runtime.active_profiles(),
                names(JUNIPER),
                "split at {split}"
            );
            assert_eq!(runtime.stack_len(), 1, "split at {split}");
        }
    }

    #[test]
    fn c2c_budget_after_an_ordinary_pop_is_spent() {
        let store = ProfileStore::builtin();
        let mut runtime = local(&store);
        step(
            &mut runtime,
            &store,
            "telnet a1",
            &[
                "Connected to a1.\nEscape character is '^]'.\n--- JUNOS 22.4R3 Kernel 64-bit\nlab@a1> ",
            ],
        );
        step(
            &mut runtime,
            &store,
            "telnet b1",
            &["Connected to b1.\nEscape character is '^]'.\nCisco IOS Software\nb1#"],
        );
        step(
            &mut runtime,
            &store,
            "\x1dq",
            &["\ntelnet> q\nConnection closed by foreign host.\nlab@a1> "],
        );
        step(
            &mut runtime,
            &store,
            "cat notes",
            &["Connection closed.\nlab@a1> "],
        );
        assert_eq!(runtime.active_profiles(), names(JUNIPER));
        assert_eq!(runtime.stack_len(), 1);
    }

    #[test]
    fn c2c_outermost_telnet_behind_a_subshell_is_not_closed() {
        let store = ProfileStore::builtin();
        let mut runtime = local(&store);
        step(
            &mut runtime,
            &store,
            "telnet a1",
            &[
                "Connected to a1.\nEscape character is '^]'.\n--- JUNOS 22.4R3 Kernel 64-bit\nlab@a1> ",
            ],
        );
        step(
            &mut runtime,
            &store,
            "\x1d!",
            &["\ntelnet> !\nuser@laptop:~$ "],
        );
        step(
            &mut runtime,
            &store,
            "telnet b1",
            &["Connected to b1.\nEscape character is '^]'.\nCisco IOS Software\nb1#"],
        );
        step(
            &mut runtime,
            &store,
            "\x1dq",
            &["\ntelnet> q\nConnection closed.\nuser@laptop:~$ "],
        );
        assert!(runtime.stack_len() >= 1, "a1's frame must survive");
    }

    #[test]
    fn c2c_an_exchange_armed_across_a_rejected_switch_does_not_close() {
        let store = ProfileStore::builtin();
        let mut runtime = telnet_to_juniper(&store, "telnet r1");
        runtime.observe_input(b"\x1d");
        // The telnet's close is shown while the exchange is armed, and the
        // session rejects the switch back (its rules failed to build).
        assert_eq!(
            runtime.observe_output(b"^]\r\nConnection closed by foreign host.\r\n", &store),
            Some(names(LOCAL))
        );
        runtime.reject_transition();
        assert_eq!(runtime.active_profiles(), names(JUNIPER));
        assert_eq!(runtime.stack_len(), 1);
        runtime.observe_input(b"q\r");
        runtime.observe_output(
            b"\r\ntelnet> q\r\nConnection closed.\r\nuser@laptop:~$ ",
            &store,
        );
        assert_eq!(runtime.active_profiles(), names(JUNIPER));
    }
}
