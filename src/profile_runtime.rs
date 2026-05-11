use crate::profiles::ProfileStore;

const OUTPUT_WINDOW_LIMIT: usize = 64 * 1024;
const PROFILE_PRIORITY: &[&str] = &[
    "juniper",
    "fortinet",
    "arubacx",
    "arista",
    "cisco",
    "palo-alto",
    "versa",
    "linux-unix",
];

#[derive(Clone, Debug)]
pub(crate) struct ProfileRuntime {
    active_profiles: Vec<String>,
    baseline_profiles: Vec<String>,
    stack: Vec<Vec<String>>,
    output_window: String,
    input_line: String,
    remote_candidate: bool,
    baseline_locked: bool,
    pending_prompt_profiles: Option<Vec<String>>,
    pending_prompt_hits: usize,
}

impl ProfileRuntime {
    pub(crate) fn new(initial_profiles: Vec<String>) -> Self {
        let baseline_locked = !is_generic_only(&initial_profiles);
        Self {
            active_profiles: initial_profiles.clone(),
            baseline_profiles: initial_profiles,
            stack: Vec::new(),
            output_window: String::new(),
            input_line: String::new(),
            remote_candidate: false,
            baseline_locked,
            pending_prompt_profiles: None,
            pending_prompt_hits: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn active_profiles(&self) -> Vec<String> {
        self.active_profiles.clone()
    }

    pub(crate) fn observe_input(&mut self, input: &[u8]) {
        for byte in input {
            match byte {
                b'\r' | b'\n' => {
                    self.submit_input_line();
                    self.input_line.clear();
                }
                0x08 | 0x7f => {
                    self.input_line.pop();
                }
                byte if byte.is_ascii_graphic() || *byte == b' ' || *byte == b'\t' => {
                    self.input_line.push(*byte as char);
                }
                _ => {}
            }
        }
    }

    pub(crate) fn observe_output(
        &mut self,
        visible_output: &[u8],
        store: &ProfileStore,
    ) -> Option<Vec<String>> {
        let text = String::from_utf8_lossy(visible_output);
        if text.is_empty() {
            return None;
        }

        if contains_close_marker(&text) {
            self.output_window.clear();
            self.remote_candidate = false;
            self.clear_pending_prompt();
            return self.pop_profile();
        }

        self.output_window.push_str(&text);
        trim_to_recent_chars(&mut self.output_window, OUTPUT_WINDOW_LIMIT);

        let detected = store.detect_profiles(&self.output_window);
        if is_generic_only(&detected) {
            self.clear_pending_prompt();
            return None;
        }
        if detected == self.active_profiles {
            self.clear_pending_prompt();
            return None;
        }

        if self.should_learn_baseline(&detected) {
            self.baseline_profiles = detected.clone();
            self.active_profiles = detected.clone();
            self.baseline_locked = true;
            self.output_window.clear();
            self.clear_pending_prompt();
            return Some(detected);
        }

        let active_profile = active_specific_profile(&self.active_profiles);
        if let Some(profile) = strong_transition_profile(&detected, &text, active_profile) {
            self.remote_candidate = false;
            self.output_window.clear();
            self.clear_pending_prompt();
            return self.switch_to(profile_set(&profile));
        }

        if (self.remote_candidate || active_profile.is_none())
            && let Some(profile) = prompt_transition_profile(&detected, &text, active_profile)
        {
            return self.note_prompt_detection(profile_set(&profile));
        }

        if active_profile.is_some() {
            self.output_window.clear();
        }
        self.clear_pending_prompt();
        None
    }

    fn submit_input_line(&mut self) {
        let trimmed = self.input_line.trim_start();
        let first_word = trimmed
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(
            first_word.as_str(),
            "ssh" | "telnet" | "mosh" | "screen" | "cu" | "minicom" | "picocom"
        ) {
            self.remote_candidate = true;
            self.baseline_locked = true;
            self.output_window.clear();
            self.clear_pending_prompt();
        }
    }

    fn should_learn_baseline(&self, detected: &[String]) -> bool {
        !self.baseline_locked
            && self.stack.is_empty()
            && is_generic_only(&self.active_profiles)
            && detected.iter().any(|profile| profile == "linux-unix")
    }

    fn note_prompt_detection(&mut self, detected: Vec<String>) -> Option<Vec<String>> {
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
            self.remote_candidate = false;
            self.output_window.clear();
            self.clear_pending_prompt();
            self.switch_to(detected)
        } else {
            None
        }
    }

    fn switch_to(&mut self, profiles: Vec<String>) -> Option<Vec<String>> {
        if profiles == self.active_profiles {
            return None;
        }
        if is_generic_only(&profiles) {
            return None;
        }
        if self
            .stack
            .last()
            .is_some_and(|previous| previous == &profiles)
        {
            self.active_profiles = self.stack.pop().expect("last checked as present");
            return Some(self.active_profiles.clone());
        }
        if profiles == self.baseline_profiles {
            self.stack.clear();
            self.active_profiles = self.baseline_profiles.clone();
            return Some(self.active_profiles.clone());
        }

        self.stack.push(self.active_profiles.clone());
        self.active_profiles = profiles;
        Some(self.active_profiles.clone())
    }

    fn pop_profile(&mut self) -> Option<Vec<String>> {
        let next = self
            .stack
            .pop()
            .unwrap_or_else(|| self.baseline_profiles.clone());
        if next == self.active_profiles {
            None
        } else {
            self.active_profiles = next;
            Some(self.active_profiles.clone())
        }
    }

    fn clear_pending_prompt(&mut self) {
        self.pending_prompt_profiles = None;
        self.pending_prompt_hits = 0;
    }
}

fn is_generic_only(profiles: &[String]) -> bool {
    profiles.len() == 1 && profiles.first().is_some_and(|profile| profile == "generic")
}

fn active_specific_profile(profiles: &[String]) -> Option<&str> {
    ordered_specific_profiles(profiles)
        .into_iter()
        .next()
        .map(String::as_str)
}

fn profile_set(profile: &str) -> Vec<String> {
    vec!["generic".to_string(), profile.to_string()]
}

fn ordered_specific_profiles(profiles: &[String]) -> Vec<&String> {
    let mut ordered = Vec::new();
    for priority in PROFILE_PRIORITY {
        if let Some(profile) = profiles
            .iter()
            .find(|profile| profile.as_str() == *priority)
        {
            ordered.push(profile);
        }
    }
    for profile in profiles {
        if profile != "generic" && !ordered.contains(&profile) {
            ordered.push(profile);
        }
    }
    ordered
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

fn contains_close_marker(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("closed by remote host")
        || lower.contains("connection closed")
        || (lower.contains("connection to ") && lower.contains(" closed"))
        || lower.lines().any(|line| line.trim() == "logout")
}

fn strong_transition_profile(
    detected: &[String],
    text: &str,
    active_profile: Option<&str>,
) -> Option<String> {
    ordered_specific_profiles(detected)
        .into_iter()
        .filter(|profile| Some(profile.as_str()) != active_profile)
        .find(|profile| has_strong_profile_signal(profile, text))
        .cloned()
}

fn prompt_transition_profile(
    detected: &[String],
    text: &str,
    active_profile: Option<&str>,
) -> Option<String> {
    ordered_specific_profiles(detected)
        .into_iter()
        .filter(|profile| Some(profile.as_str()) != active_profile)
        .find(|profile| has_prompt_profile_signal(profile, text))
        .cloned()
}

fn has_strong_profile_signal(profile: &str, text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    match profile {
        "juniper" => lower.contains("junos"),
        "cisco" => {
            lower.contains("cisco ios")
                || lower.contains("ios xe")
                || lower.contains("ios-xe")
                || lower.contains("asa version")
                || lower.contains("nx-os")
                || lower.contains("nexus operating system")
                || lower.contains("cisco nexus")
        }
        "fortinet" => has_strong_fortinet_signal(text),
        "linux-unix" => {
            lower.contains("ubuntu")
                || lower.contains("debian")
                || lower.contains("centos")
                || lower.contains("rocky linux")
                || lower.contains("alma linux")
                || lower.contains("red hat")
                || lower.contains("rhel")
                || lower.contains("fedora")
                || lower.contains("kernel: linux")
                || lower.contains("terminal: /dev/")
                || lower.contains("/dev/pts/")
        }
        "arubacx" => {
            lower.contains("arubaos-cx") || lower.contains("aos-cx") || lower.contains("hpe-restd")
        }
        "arista" => lower.contains("arista ceoslab") || lower.contains("arista networks eos"),
        "palo-alto" => {
            lower.contains("pan-os") || lower.contains("pa-vm") || lower.contains("palo alto")
        }
        "versa" => {
            lower.contains("versa-")
                || lower.contains("versa networks")
                || lower.contains("versa director")
        }
        _ => false,
    }
}

fn has_strong_fortinet_signal(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        let lower = line.to_ascii_lowercase();
        lower.starts_with("version:")
            && (lower.contains("fortigate")
                || lower.contains("fortios")
                || lower.contains("fortinet"))
    })
}

fn has_prompt_profile_signal(profile: &str, text: &str) -> bool {
    match profile {
        "juniper" => text.lines().any(looks_like_juniper_prompt_line),
        "cisco" | "arista" | "arubacx" => text.lines().any(looks_like_network_prompt_line),
        "fortinet" => text.lines().any(looks_like_fortinet_prompt_line),
        "linux-unix" => text.lines().any(looks_like_unix_prompt_line),
        "palo-alto" | "versa" => text.lines().any(looks_like_network_prompt_line),
        _ => false,
    }
}

fn prompt_token(line: &str) -> &str {
    line.split_whitespace()
        .next()
        .unwrap_or(line)
        .trim_matches(|ch: char| ch.is_ascii_control())
}

fn looks_like_juniper_prompt_line(line: &str) -> bool {
    let token = prompt_token(line);
    token.contains('@') && (token.ends_with('>') || token.ends_with('%'))
}

fn looks_like_network_prompt_line(line: &str) -> bool {
    let token = prompt_token(line);
    let Some(marker) = token.find(['>', '#']) else {
        return false;
    };
    if marker == 0 {
        return false;
    }
    let body = &token[..marker];
    !body.contains('@') && !body.contains(':') && body.bytes().all(is_prompt_name_byte)
}

fn looks_like_fortinet_prompt_line(line: &str) -> bool {
    let trimmed = line.trim_matches(|ch: char| ch.is_ascii_control());
    let Some((host, _rest)) = trimmed.split_once(" #") else {
        return false;
    };
    let host = host.trim_end();
    !host.is_empty()
        && !host.contains('@')
        && !host.contains(':')
        && host.bytes().all(is_prompt_name_byte)
}

fn looks_like_unix_prompt_line(line: &str) -> bool {
    let token = prompt_token(line);
    let Some((user, rest)) = token.split_once('@') else {
        return false;
    };
    let Some((host, tail)) = rest.split_once(':') else {
        return false;
    };
    let marker = tail
        .bytes()
        .position(|byte| matches!(byte, b'#' | b'$' | b'%'));
    !user.is_empty()
        && !host.is_empty()
        && marker.is_some()
        && user.bytes().all(is_prompt_name_byte)
        && host.bytes().all(is_prompt_name_byte)
}

fn is_prompt_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

#[cfg(test)]
mod tests {
    use super::ProfileRuntime;
    use crate::profiles::ProfileStore;

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
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
    fn prompt_only_detection_requires_repetition_without_input_hint() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic"]));

        assert_eq!(runtime.observe_output(b"labuser@mx480>\n", &store), None);
        assert_eq!(
            runtime.observe_output(b"labuser@mx480>\n", &store),
            Some(names(&["generic", "juniper"]))
        );
    }
}
