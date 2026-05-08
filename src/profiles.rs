use crate::config::{ConfigError, RuleSpec, RuleStyle};
use crate::style::Style;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub struct Profile {
    pub name: String,
    pub inherits: Vec<String>,
    pub detection: Vec<String>,
    pub rules: Vec<RuleSpec>,
}

#[derive(Clone, Debug, Default)]
pub struct ProfileStore {
    profiles: BTreeMap<String, Profile>,
}

impl ProfileStore {
    pub fn builtin() -> Self {
        let mut store = Self::default();
        for profile in builtin_profiles() {
            store.profiles.insert(profile.name.clone(), profile);
        }
        store
    }

    pub fn names(&self) -> Vec<&str> {
        self.profiles.keys().map(String::as_str).collect()
    }

    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    pub fn insert_profile(
        &mut self,
        name: String,
        inherits: Vec<String>,
        detection: Vec<String>,
        rules: Vec<RuleSpec>,
    ) {
        self.profiles.insert(
            name.clone(),
            Profile {
                name,
                inherits,
                detection,
                rules,
            },
        );
    }

    pub fn detect_profiles(&self, sample: &str) -> Vec<String> {
        let lower = sample.to_ascii_lowercase();
        let mut detected = vec!["generic".to_string()];

        for (name, profile) in &self.profiles {
            if name == "generic" {
                continue;
            }
            if profile_matches_detection(name, profile, sample, &lower)
                || profile
                    .detection
                    .iter()
                    .any(|hint| lower.contains(&hint.to_ascii_lowercase()))
            {
                detected.push(name.clone());
            }
        }

        detected
    }

    pub fn append_profile_rules(
        &self,
        profile_name: &str,
        loaded: &mut BTreeSet<String>,
        rules: &mut Vec<RuleSpec>,
    ) -> Result<(), ConfigError> {
        if loaded.contains(profile_name) {
            return Ok(());
        }

        let profile = self
            .profiles
            .get(profile_name)
            .ok_or_else(|| ConfigError::UnknownProfile(profile_name.to_string()))?;

        for parent in &profile.inherits {
            self.append_profile_rules(parent, loaded, rules)?;
        }

        loaded.insert(profile.name.clone());
        rules.extend(profile.rules.clone());
        Ok(())
    }
}

fn profile_matches_detection(name: &str, _profile: &Profile, sample: &str, lower: &str) -> bool {
    match name {
        "juniper" => {
            lower.contains("junos")
                || lower.contains("commit check")
                || (looks_like_juniper_prompt(sample)
                    && !has_palo_alto_signal(lower)
                    && !has_versa_signal(lower))
        }
        "cisco" => {
            lower.contains("cisco ios")
                || lower.contains("ios xe")
                || lower.contains("ios-xe")
                || lower.contains("asa version")
                || lower.contains("nx-os")
                || lower.contains("nexus operating system")
                || lower.contains("cisco nexus")
                || lower.contains("show ip ")
                || lower.contains("line protocol is")
                || lower.contains("router ospf")
                || lower.contains("router bgp")
                || (looks_like_cisco_prompt(sample) && !has_arubacx_signal(lower))
        }
        "arubacx" => has_arubacx_signal(lower),
        "arista" => {
            lower.contains("arista ceoslab")
                || lower.contains("arista networks eos")
                || lower.contains("show interfaces status")
                || looks_like_arista_prompt(sample)
        }
        "fortinet" => {
            lower.contains("fortigate")
                || lower.contains("fortinet")
                || lower.contains("fortios")
                || lower.contains("get system status")
                || lower.contains("config system")
                || lower.contains("diagnose")
                || looks_like_fortinet_prompt(sample)
        }
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
                || lower.contains("shell: /")
                || lower.contains("terminal: /dev/")
                || lower.contains("/dev/pts/")
                || lower.contains("systemd")
                || looks_like_unix_prompt(sample)
        }
        "palo-alto" => has_palo_alto_signal(lower) || looks_like_palo_alto_prompt(sample),
        "versa" => {
            has_versa_signal(lower)
                || lower.contains("vni-")
                || lower.contains("tvi-")
                || lower.contains("show orgs org-services")
        }
        _ => false,
    }
}

fn looks_like_cisco_prompt(sample: &str) -> bool {
    sample.lines().any(|line| {
        let prompt = line
            .split_whitespace()
            .next()
            .unwrap_or(line)
            .trim_matches(|ch: char| ch.is_ascii_control());
        let marker = prompt.find(['>', '#']);
        let Some(marker) = marker else {
            return false;
        };
        if marker == 0 {
            return false;
        }
        let body = &prompt[..marker];
        !body.contains('@')
            && !body.contains(':')
            && body.bytes().all(is_cisco_prompt_byte)
            && prompt[marker + 1..]
                .bytes()
                .all(|byte| byte.is_ascii_alphabetic() || matches!(byte, b' ' | b'\t'))
    })
}

fn has_arubacx_signal(lower: &str) -> bool {
    lower.contains("arubaos-cx") || lower.contains("aos-cx") || lower.contains("hpe-restd")
}

fn has_palo_alto_signal(lower: &str) -> bool {
    lower.contains("pan-os") || lower.contains("pa-vm") || lower.contains("palo alto")
}

fn has_versa_signal(lower: &str) -> bool {
    lower.contains("versa-") || lower.contains("versa networks") || lower.contains("versa director")
}

fn looks_like_fortinet_prompt(sample: &str) -> bool {
    sample.lines().any(|line| {
        let trimmed = line.trim_matches(|ch: char| ch.is_ascii_control());
        let Some((host, rest)) = trimmed.split_once(" #") else {
            return false;
        };
        let host = fortinet_prompt_host(host.trim_end());
        !host.is_empty()
            && !host.contains('@')
            && !host.contains(':')
            && host.bytes().all(is_prompt_name_byte)
            && rest
                .bytes()
                .next()
                .is_none_or(|byte| byte.is_ascii_whitespace() || byte.is_ascii_alphabetic())
    })
}

fn fortinet_prompt_host(host: &str) -> &str {
    if !host.ends_with(')') {
        return host;
    }
    host.rsplit_once(" (")
        .map(|(base, _context)| base.trim_end())
        .unwrap_or(host)
}

fn looks_like_unix_prompt(sample: &str) -> bool {
    sample.lines().any(|line| {
        let prompt = line
            .split_whitespace()
            .next()
            .unwrap_or(line)
            .trim_matches(|ch: char| ch.is_ascii_control());
        let Some((user, rest)) = prompt.split_once('@') else {
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
    })
}

fn looks_like_juniper_prompt(sample: &str) -> bool {
    sample.lines().any(|line| {
        let prompt = line
            .split_whitespace()
            .next()
            .unwrap_or(line)
            .trim_matches(|ch: char| ch.is_ascii_control());
        let Some(marker) = prompt.rfind(['>', '%']) else {
            return false;
        };
        if marker + 1 != prompt.len() {
            return false;
        }
        let body = &prompt[..marker];
        let Some((user, host)) = body.split_once('@') else {
            return false;
        };
        !user.is_empty()
            && !host.is_empty()
            && !host.contains(':')
            && user.bytes().all(is_prompt_name_byte)
            && host.bytes().all(is_prompt_name_byte)
    })
}

fn looks_like_arista_prompt(sample: &str) -> bool {
    sample.lines().any(|line| {
        let prompt = line
            .split_whitespace()
            .next()
            .unwrap_or(line)
            .trim_matches(|ch: char| ch.is_ascii_control());
        let Some(marker) = prompt.rfind(['>', '#']) else {
            return false;
        };
        if marker == 0 {
            return false;
        }
        let host = &prompt[..marker];
        let command_tail = &prompt[marker + 1..];
        let arista_like_host = host.to_ascii_lowercase();
        let has_arista_host_hint = arista_like_host.contains("arista")
            || arista_like_host.contains("ceos")
            || arista_like_host.starts_with("eos")
            || arista_like_host.starts_with("leaf")
            || arista_like_host.starts_with("spine");

        has_arista_host_hint
            && !host.contains('@')
            && !host.contains(':')
            && host.bytes().all(is_prompt_name_byte)
            && command_tail
                .bytes()
                .all(|byte| byte.is_ascii_alphabetic() || matches!(byte, b' ' | b'\t'))
    })
}

fn looks_like_palo_alto_prompt(sample: &str) -> bool {
    sample.lines().any(|line| {
        let prompt = line
            .split_whitespace()
            .next()
            .unwrap_or(line)
            .trim_matches(|ch: char| ch.is_ascii_control());
        let Some(marker) = prompt.rfind(['>', '#']) else {
            return false;
        };
        if marker == 0 {
            return false;
        }
        let body = &prompt[..marker];
        let Some((user, host)) = body.split_once('@') else {
            return false;
        };
        let host_lower = host.to_ascii_lowercase();
        let has_pan_host_hint = host_lower.starts_with("pa-")
            || host_lower.starts_with("fw-")
            || host_lower.contains("pan")
            || host_lower.contains("palo");

        has_pan_host_hint
            && !user.is_empty()
            && !host.is_empty()
            && !host.contains(':')
            && user.bytes().all(is_prompt_name_byte)
            && host.bytes().all(is_prompt_name_byte)
    })
}

fn is_prompt_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-')
}

fn is_cisco_prompt_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-' | b'(' | b')')
}

fn builtin_profiles() -> Vec<Profile> {
    vec![
        Profile {
            name: "generic".to_string(),
            inherits: vec![],
            detection: vec![],
            rules: vec![
                rule(
                    "IPv4 address",
                    r"\b(?<!\.)((25[0-5]|(2[0-4]|[0-1]?\d)?\d)\.){3}(25[0-5]|(2[0-4]|[0-1]?\d)?\d)(/\d+)?(?!\.)\b",
                    "f#00ffff",
                ),
                rule(
                    "MAC address",
                    r"(?i)\b((?<!:)([\da-f]{1,2}:){5}[\da-f]{1,2}(?!:)|(?<!\.)([\da-f]{4}\.){2}[\da-f]{4}(?!\.))\b",
                    "f#ff9aff",
                ),
                rule(
                    "bad operational state",
                    r"(?i)\b(password|abnormal(ly)?|down|los(t|s|ing)|err(or(s)?)?|den(y|ies|ied)?|reject(ing|ed)?|drop(ped|s)?|fail(s|ed|ure)?|disconnect(ed)?|unreachable|invalid|bad|notconnect|unusable|blocking|blocked|collision(s)?|unsynchronized|mismatch|runts|CRC|resets)\b",
                    "f#ff0000 bold",
                ),
                rule(
                    "warning state",
                    r"(?i)\b(warning(s)?|degraded|standby|learning|listening|passive)\b",
                    "f#ffff00",
                ),
                rule(
                    "good operational state",
                    r"(?i)\b(up|ok(ay)?|permit(ed|s)?|accept(s|ed)?|enable(d)?|online|succe((ss(ful|fully)?)|ed(ed)?)?|connect(ed)?|reachable|valid|forwarding|synchronized|active)\b",
                    "f#00ff00",
                ),
                rule("syslog severe", r"\b(%\w+-[0-3]-\w+)\b", "f#ff3333 bold"),
                rule("syslog warning", r"\b(%\w+-[4-5]-\w+)\b", "f#ffff00"),
                rule("syslog info", r"\b(%\w+-[6-7]-\w+)\b", "f#65d7fd"),
            ],
        },
        Profile {
            name: "juniper".to_string(),
            inherits: vec!["generic".to_string()],
            detection: vec!["junos".to_string(), "commit check".to_string()],
            rules: vec![
                rule(
                    "Juniper prompt",
                    r"(?m)^(\w+)(@)([^>%\n]+)([>%])",
                    "f#00bfff",
                ),
                rule(
                    "Juniper interface",
                    r"(?i)\b(((fe|ge|xe|et|gr|ip|lt|lsq|mt|sp|vcp)-\d*/\d*/\d*)|(((b)?me|em|fab|fxp|fti|lo|pp(d|e)?|st|swfab)[0-2]|dsc|gre|ipip|irb|jsrv|lsi|mtun|pimd|pime|tap|vlan|vme|vtep)|((ae|reth)\d*))(\.\d*)?\b",
                    "f#0099ff",
                ),
                rule("Juniper compare added", r"(?m)^\+ .*$", "f#00dc1a"),
                rule("Juniper compare removed", r"(?m)^- .*$", "f#ff3333"),
            ],
        },
        Profile {
            name: "cisco".to_string(),
            inherits: vec!["generic".to_string()],
            detection: vec![
                "router#".to_string(),
                "switch#".to_string(),
                "ios".to_string(),
                "ios xe".to_string(),
                "nx-os".to_string(),
                "nexus operating system".to_string(),
                "line protocol is".to_string(),
                "show ip ".to_string(),
            ],
            rules: vec![
                rule(
                    "Cisco prompt",
                    r"(?m)^[A-Za-z0-9_.-]+(\([^)]+\))?[>#]",
                    "f#ffffff",
                ),
                rule(
                    "Cisco interface",
                    r"(?i)\b(((Hu(ndredGigabit)?|Fo(rtyGigabit)?|Te(nGigabit)?|Gi(gabit)?|Fa(st)?)(Ethernet)?)|Eth|Se(rial)?|Lo(opback)?|Tu(nnel)?|VL(AN)?|Po(rt-channel)?|Vi(rtual-(Template|Access))?|Mu(ltilink)?|Di(aler)?|(B|N)VI)((\d*/){0,2}\d*)(\.\d*)?\b",
                    "f#0099ff",
                ),
                rule(
                    "OSPF state",
                    r"\b(ATTEMPT|INIT|EXCHANGE|LOADING|2WAY|FULL|DR|BDR|DROTHER)\b",
                    "f#ffa500",
                ),
                rule(
                    "BGP state",
                    r"\b(Idle|Connect|Active|OpenSent|OpenConfirm|Established)\b",
                    "f#4da6ff bold",
                ),
                rule(
                    "Cisco Nexus bad interface status",
                    r"(?i)\b(suspended|notconnec)\b",
                    "f#ff0000 bold",
                ),
                rule(
                    "Cisco Nexus warning interface status",
                    r"\b(xcvrAbsen|noOperMem)\b",
                    "f#ffa500",
                ),
            ],
        },
        Profile {
            name: "versa".to_string(),
            inherits: vec!["generic".to_string()],
            detection: vec![
                "versa-".to_string(),
                "vsh".to_string(),
                "vni-".to_string(),
                "tvi-".to_string(),
                "show orgs org-services".to_string(),
            ],
            rules: vec![
                rule(
                    "Versa prompt",
                    r"(?m)^[A-Za-z0-9_.-]+@(versa|voss)[^>#\n]*[>#]",
                    "f#00bfff",
                ),
                rule(
                    "Versa object",
                    r"(?i)\b(vni-\d+|tvi-\d+/\d+|sdwan|tenant|appliance|org|controller|branch)\b",
                    "f#ff00ff",
                ),
                rule(
                    "Versa interface",
                    r"(?i)\b((vni-\d+/\d+)|(tvi-\d+/\d+)|(ptvi-\d+))(\.\d+)?\b",
                    "f#0099ff",
                ),
                rule(
                    "Versa BGP state",
                    r"\b(Idle|Connect|Active|OpenSent|OpenConfirm|Established)\b",
                    "f#4da6ff bold",
                ),
            ],
        },
        Profile {
            name: "arubacx".to_string(),
            inherits: vec!["generic".to_string()],
            detection: vec![
                "arubaos-cx".to_string(),
                "aos-cx".to_string(),
                "hpe-restd".to_string(),
            ],
            rules: vec![
                rule("ArubaCX prompt", r"(?m)^[A-Za-z0-9_.-]+[>#]", "f#ffffff"),
                rule(
                    "ArubaCX interface",
                    r"(?i)\b((\d+/\d+/\d+)|(lag\d+)|(vlan\d+)|(loopback\d+)|(mgmt))(\.\d+)?\b",
                    "f#0099ff",
                ),
                rule(
                    "ArubaCX event severity",
                    r"\bLOG_(EMERG|ALERT|CRIT|ERR|ERROR|WARNING|NOTICE|INFO|DEBUG)\b",
                    "f#65d7fd",
                ),
                rule(
                    "ArubaCX platform terms",
                    r"(?i)\b(arubaos-cx|aos-cx|hpe-restd|vsx|vsf|checkpoint|event)\b",
                    "f#ff00ff",
                ),
            ],
        },
        Profile {
            name: "arista".to_string(),
            inherits: vec!["cisco".to_string()],
            detection: vec!["arista".to_string(), "ceoslab".to_string()],
            rules: vec![
                rule(
                    "Arista interface",
                    r"(?i)\b(Ethernet|Et|Management|Ma|Port-Channel|Po|Vlan|Loopback|Lo)\d+(/\d+)?(\.\d+)?\b",
                    "f#0099ff",
                ),
                rule(
                    "MLAG",
                    r"(?i)\b(mlag|peer-link|peer-address|reload-delay)\b",
                    "f#ff00ff",
                ),
            ],
        },
        Profile {
            name: "fortinet".to_string(),
            inherits: vec!["generic".to_string()],
            detection: vec![
                "fortigate".to_string(),
                "fortinet".to_string(),
                "fortios".to_string(),
                "fgt".to_string(),
                "diagnose".to_string(),
                "get system status".to_string(),
            ],
            rules: vec![
                rule("Fortinet prompt", r"(?m)^[A-Za-z0-9_.-]+ #", "f#ffffff"),
                rule(
                    "Fortinet terms",
                    r"(?i)\b(vdom|policyid|srcintf|dstintf|utm|ipsec|phase1|phase2|ha|diagnose|vd-root)\b",
                    "f#ff00ff",
                ),
            ],
        },
        Profile {
            name: "palo-alto".to_string(),
            inherits: vec!["generic".to_string()],
            detection: vec![
                "pa-vm".to_string(),
                "pan-os".to_string(),
                "palo alto".to_string(),
            ],
            rules: vec![
                rule(
                    "PAN-OS prompt",
                    r"(?m)^[A-Za-z0-9_.-]+@[^>#\n]+[>#]",
                    "f#00bfff",
                ),
                rule(
                    "PAN-OS interface",
                    r"(?i)\b(ethernet\d+/\d+(\.\d+)?|tunnel\.\d+|loopback\.\d+)\b",
                    "f#0099ff",
                ),
                rule(
                    "PAN-OS terms",
                    r"(?i)\b(vsys\d*|security-policy|nat-policy|globalprotect|wildfire|threat|url-filtering|ha\d?|panorama)\b",
                    "f#ff00ff",
                ),
            ],
        },
        Profile {
            name: "linux-unix".to_string(),
            inherits: vec!["generic".to_string()],
            detection: vec![
                "systemctl".to_string(),
                "root@".to_string(),
                "journalctl".to_string(),
                "sshd".to_string(),
                "sudo".to_string(),
                "ubuntu".to_string(),
                "debian".to_string(),
                "kernel: linux".to_string(),
                "terminal: /dev/".to_string(),
            ],
            rules: vec![
                rule(
                    "Unix prompt",
                    r"(?m)^([A-Za-z0-9_.-]+)(@)([A-Za-z0-9_.-]+)(:[^#$\n]*)?([#$])",
                    "f#ffffff",
                ),
                rule("root user", r"\broot\b", "f#ff0000 bold"),
                rule(
                    "systemd states",
                    r"(?i)\b(active|inactive|failed|dead|running|exited|enabled|disabled|masked|loaded)\b",
                    "f#00ff00",
                ),
                rule(
                    "log priority",
                    r"(?i)\b(emerg|alert|crit|critical|error|err|warning|warn|notice|info|debug)\b",
                    "f#ffff00",
                ),
                rule(
                    "port",
                    r"(?i)\b(tcp|udp)/\d{1,5}\b|((?<=[A-Za-z0-9_.-]{3})|(?<=\])):\d{1,5}\b",
                    "f#00ffc0",
                ),
            ],
        },
    ]
}

fn rule(description: &str, regex: &str, style: &str) -> RuleSpec {
    RuleSpec {
        description: description.to_string(),
        regex: regex.to_string(),
        style: RuleStyle::Whole(Style::parse(style).expect("built-in style is valid")),
        exclusive: false,
    }
}
