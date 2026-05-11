use crate::profiles::ProfileStore;

const OUTPUT_WINDOW_LIMIT: usize = 64 * 1024;

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

        if self.should_learn_baseline(&detected, store) {
            self.baseline_profiles = detected.clone();
            self.active_profiles = detected.clone();
            self.baseline_locked = true;
            self.output_window.clear();
            self.clear_pending_prompt();
            return Some(detected);
        }

        let active_profile = store.active_specific_profile(&self.active_profiles);
        if let Some(profile) = store.strong_transition_profile(&detected, &text, active_profile) {
            self.remote_candidate = false;
            self.output_window.clear();
            self.clear_pending_prompt();
            return self.switch_to(profile_set(&profile));
        }

        let at_local_baseline = self.at_local_baseline(store);
        if (self.remote_candidate || active_profile.is_none() || at_local_baseline)
            && let Some(profile) = store.prompt_transition_profile(&detected, &text, active_profile)
        {
            if store.prompt_switches_on_first_detection(
                &profile,
                self.remote_candidate,
                at_local_baseline,
            ) {
                self.remote_candidate = false;
                self.output_window.clear();
                self.clear_pending_prompt();
                return self.switch_to(profile_set(&profile));
            }
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

    fn at_local_baseline(&self, store: &ProfileStore) -> bool {
        self.stack.is_empty()
            && self.active_profiles == self.baseline_profiles
            && store.profiles_are_local_baseline(&self.active_profiles)
    }

    fn should_learn_baseline(&self, detected: &[String], store: &ProfileStore) -> bool {
        !self.baseline_locked
            && self.stack.is_empty()
            && is_generic_only(&self.active_profiles)
            && !is_generic_only(detected)
            && store.profiles_are_local_baseline(detected)
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

fn profile_set(profile: &str) -> Vec<String> {
    vec!["generic".to_string(), profile.to_string()]
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
    fn remote_hint_promotes_fortinet_on_first_prompt() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"ssh firewall-a\r");
        let changed = runtime.observe_output(b"FW-EDGE # ", &store);

        assert_eq!(changed, Some(names(&["generic", "fortinet"])));
        assert_eq!(runtime.active_profiles(), names(&["generic", "fortinet"]));
    }

    #[test]
    fn local_shell_wrapper_command_promotes_fortinet_on_first_prompt() {
        let store = ProfileStore::builtin();
        let mut runtime = ProfileRuntime::new(names(&["generic", "linux-unix"]));

        runtime.observe_input(b"fw\r");
        let changed = runtime.observe_output(b"FW-EDGE # ", &store);

        assert_eq!(changed, Some(names(&["generic", "fortinet"])));
        assert_eq!(runtime.active_profiles(), names(&["generic", "fortinet"]));
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
