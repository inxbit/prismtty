use prismtty::config::{ConfigError, RuleSpec, RuleStyle};
use prismtty::style::{Rgb, Style};
use prismtty::{Highlighter, PrismConfig, ProfileStore, StyledSpan};

const CYAN: Rgb = Rgb {
    r: 0,
    g: 255,
    b: 255,
};
const BLUE: Rgb = Rgb {
    r: 0,
    g: 153,
    b: 255,
};
const WHITE: Rgb = Rgb {
    r: 255,
    g: 255,
    b: 255,
};
const PROMPT_BLUE: Rgb = Rgb {
    r: 0,
    g: 191,
    b: 255,
};
const PORT_GREEN: Rgb = Rgb {
    r: 0,
    g: 255,
    b: 192,
};
const RED: Rgb = Rgb { r: 255, g: 0, b: 0 };
const GREEN: Rgb = Rgb { r: 0, g: 255, b: 0 };

fn spans_for(profile: &str, input: &str) -> Vec<StyledSpan> {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &[profile]).expect("profile resolves");
    Highlighter::from_config(config)
        .expect("profile compiles")
        .style_spans(input.as_bytes())
}

fn span_with_color(spans: &[StyledSpan], color: Rgb) -> Option<&StyledSpan> {
    spans
        .iter()
        .find(|span| span.style.foreground == Some(color))
}

fn assert_colored(profile: &str, input: &str, color: Rgb) {
    let spans = spans_for(profile, input);
    assert!(
        span_with_color(&spans, color).is_some(),
        "{profile} should color {input:?} with {color:?}; spans={spans:?}"
    );
}

fn assert_not_colored(profile: &str, input: &str, color: Rgb) {
    let spans = spans_for(profile, input);
    assert!(
        span_with_color(&spans, color).is_none(),
        "{profile} must not color {input:?} with {color:?}; spans={spans:?}"
    );
}

#[test]
fn empty_and_partial_detection_hints_do_not_select_profiles() {
    let mut store = ProfileStore::builtin();
    store.insert_profile(
        "empty-hint".to_string(),
        Vec::new(),
        vec![String::new(), "   ".to_string()],
        Vec::new(),
    );

    for (sample, unexpected) in [
        ("ordinary terminal output", "empty-hint"),
        ("BIOS firmware settings", "cisco"),
        ("precommit checker finished", "juniper"),
        ("sudoers policy updated", "linux-unix"),
        ("vshard service ready", "versa"),
        ("show ipsec status", "cisco"),
        ("show interfaces statuses", "arista"),
    ] {
        let detected = store.detect_profiles(sample);
        assert_eq!(
            detected,
            vec!["generic"],
            "{sample:?} unexpectedly selected {unexpected}: {detected:?}"
        );
    }
}

#[test]
fn strong_detection_signals_and_priority_remain_intact() {
    let store = ProfileStore::builtin();

    for (sample, expected) in [
        ("Cisco IOS XE Software", "cisco"),
        ("JUNOS 23.4R2", "juniper"),
        ("Versa Networks FlexVNF", "versa"),
        ("Ubuntu 24.04 LTS", "linux-unix"),
    ] {
        assert!(
            store
                .detect_profiles(sample)
                .contains(&expected.to_string()),
            "strong signal {sample:?} should select {expected}"
        );
    }

    assert_eq!(
        store.detect_profiles("Cisco IOS XE Software\nJUNOS 23.4R2\n"),
        vec!["generic", "juniper", "cisco"]
    );
}

#[test]
fn address_prefix_lengths_enforce_protocol_boundaries() {
    let documentation_ipv6 = "2001:db8::1";
    for valid in [
        "192.0.2.1/0".to_string(),
        "192.0.2.1/32".to_string(),
        format!("{documentation_ipv6}/0"),
        format!("{documentation_ipv6}/128"),
    ] {
        assert_colored("generic", &valid, CYAN);
    }
    for invalid in [
        "192.0.2.1/33".to_string(),
        "192.0.2.1/999".to_string(),
        format!("{documentation_ipv6}/129"),
        format!("{documentation_ipv6}/999"),
    ] {
        assert_not_colored("generic", &invalid, CYAN);
    }
}

#[test]
fn interface_subunits_require_at_least_one_digit() {
    assert_colored("cisco", "Gi1/0/1.0", BLUE);
    assert_colored("juniper", "ge-0/0/0.0", BLUE);

    for invalid in ["Gi1/0/1.", "Gi1/0/1.unit"] {
        assert_not_colored("cisco", invalid, BLUE);
    }
    for invalid in ["ge-0/0/0.", "ge-0/0/0.unit", "ge-0/0/0/1"] {
        assert_not_colored("juniper", invalid, BLUE);
    }
}

#[test]
fn port_numbers_and_host_boundaries_are_validated() {
    let link_local_fixture = "fe80::1";
    for valid in [
        "tcp/0".to_string(),
        "udp/1".to_string(),
        "tcp/65535".to_string(),
        "a:0".to_string(),
        "ab:1".to_string(),
        "host:65535".to_string(),
        "host-name:22".to_string(),
        "[2001:db8::1]:443".to_string(),
        format!("[{link_local_fixture}%en0]:22"),
    ] {
        assert_colored("linux-unix", &valid, PORT_GREEN);
    }
    for invalid in [
        "tcp/65536".to_string(),
        "udp/99999".to_string(),
        "a:65536".to_string(),
        "left:right:80".to_string(),
        "at 12:34 today".to_string(),
        "10:07:20".to_string(),
        "[fe80::1%]:22".to_string(),
        format!("[{link_local_fixture}%bad/zone]:22"),
    ] {
        assert_not_colored("linux-unix", &invalid, PORT_GREEN);
    }
}

#[test]
fn not_connected_takes_precedence_over_connected() {
    let spans = spans_for("generic", "interface is not connected\n");
    let connected_start = "interface is not ".len();
    let connected_end = connected_start + "connected".len();
    let covering: Vec<&StyledSpan> = spans
        .iter()
        .filter(|span| span.start < connected_end && span.end > connected_start)
        .collect();

    assert!(
        covering
            .iter()
            .any(|span| span.style.foreground == Some(RED) && span.style.bold),
        "not connected should be a red fault state: {spans:?}"
    );
    assert!(
        covering
            .iter()
            .all(|span| span.style.foreground != Some(GREEN)),
        "connected must not be green inside not connected: {spans:?}"
    );
}

#[test]
fn cisco_prompts_stay_on_one_line_and_accept_same_line_commands() {
    for valid in [
        "Router#show version",
        "Router#Show version",
        "Router(config-if)# show interfaces",
        "Router#\r\n",
    ] {
        assert_colored("cisco", valid, WHITE);
    }

    assert_not_colored("cisco", "Router(config\r\n-if)#", WHITE);
}

#[test]
fn juniper_prompt_highlighting_uses_the_detection_charset() {
    let prompt = "ops.user-name@edge-router.example>";
    let store = ProfileStore::builtin();
    assert!(
        store
            .detect_profiles(prompt)
            .contains(&"juniper".to_string())
    );
    assert_colored("juniper", prompt, PROMPT_BLUE);
}

#[test]
fn versa_prompt_rejects_an_undelimited_tail() {
    assert_colored("versa", "admin@versa-lab-01>", PROMPT_BLUE);
    assert_not_colored("versa", "admin@versa-lab-01!>", PROMPT_BLUE);
}

#[test]
fn inheritance_walks_deep_graphs_without_duplicate_expansion() {
    let mut store = ProfileStore::default();
    let chain_len = 256;
    for index in 0..chain_len {
        let inherits = if index > 0 {
            vec![format!("profile-{}", index - 1)]
        } else {
            Vec::new()
        };
        store.insert_profile(format!("profile-{index}"), inherits, Vec::new(), Vec::new());
    }

    let leaf = format!("profile-{}", chain_len - 1);
    let config = PrismConfig::from_profiles(&store, &[leaf.as_str()])
        .expect("deep acyclic inheritance resolves iteratively");
    assert_eq!(config.enabled_profiles.len(), chain_len);

    let style = RuleStyle::Whole(Style::parse("f#ffffff").expect("style parses"));
    for (name, parents) in [
        ("base", vec![]),
        ("left", vec!["base"]),
        ("right", vec!["base"]),
        ("leaf", vec!["left", "right"]),
    ] {
        store.insert_profile(
            name.to_string(),
            parents.into_iter().map(str::to_string).collect(),
            Vec::new(),
            vec![RuleSpec {
                description: name.to_string(),
                regex: format!(r"\b{name}\b"),
                style: style.clone(),
                exclusive: false,
            }],
        );
    }

    let config = PrismConfig::from_profiles(&store, &["leaf"]).expect("diamond graph resolves");
    let descriptions: Vec<&str> = config
        .rules
        .iter()
        .map(|rule| rule.description.as_str())
        .collect();
    assert_eq!(descriptions, vec!["leaf", "left", "base", "right"]);
}

#[test]
fn iterative_inheritance_preserves_cycle_diagnostics() {
    let mut store = ProfileStore::default();
    store.insert_profile(
        "alpha".to_string(),
        vec!["beta".to_string()],
        Vec::new(),
        Vec::new(),
    );
    store.insert_profile(
        "beta".to_string(),
        vec!["gamma".to_string()],
        Vec::new(),
        Vec::new(),
    );
    store.insert_profile(
        "gamma".to_string(),
        vec!["alpha".to_string()],
        Vec::new(),
        Vec::new(),
    );

    match PrismConfig::from_profiles(&store, &["alpha"]) {
        Err(ConfigError::CyclicProfileInheritance(cycle)) => {
            assert_eq!(cycle, "alpha -> beta -> gamma -> alpha");
        }
        Err(other) => panic!("unexpected inheritance error: {other}"),
        Ok(_) => panic!("cycle should not resolve"),
    }
}
