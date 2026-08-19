use prismtty::highlight::strip_ansi;
use prismtty::style::{ColorMode, Rgb, Style};
use prismtty::{Highlighter, PrismConfig, ProfileStore, StreamingHighlighter};

fn new_interactive_streaming(highlighter: Highlighter) -> StreamingHighlighter {
    let mut streaming = StreamingHighlighter::new_interactive(highlighter);
    streaming.set_no_minimal_resets(false);
    streaming
}

#[test]
fn loads_chromaterm_yaml_with_lookarounds_and_capture_styles() {
    let yaml = r##"
rules:
  - description: advanced lookaround
    regex: (?<=foo)bar(?=baz)
    color: f#ff0000 bold
  - description: grouped prompt
    regex: "(user)(@)(host>)"
    color:
      1: f#00bfff bold
      2: f#00ffc0
      3: f#ffffff bold
"##;

    let config = PrismConfig::from_chromaterm_yaml(yaml).expect("chromaterm config loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");

    let output = highlighter.highlight_str("foobarbaz user@host>\n");

    assert!(output.contains("\x1b[1;38;2;255;0;0mbar\x1b[0m"));
    assert!(output.contains("\x1b[1;38;2;0;191;255muser\x1b[0m"));
    assert!(output.contains("\x1b[38;2;0;255;192m@\x1b[0m"));
    assert!(output.contains("\x1b[1;38;2;255;255;255mhost>\x1b[0m"));
}

#[test]
fn capture_rules_style_every_match_and_step_past_empty_matches() {
    // Capture-styled rules must find every match on a line, and a pattern that
    // can match empty must still advance (never loop, never double-style).
    let yaml = r##"
rules:
  - description: every number
    regex: (\d+)
    color:
      1: f#00ffff
  - description: possibly empty
    regex: (x*)
    color:
      1: underline
"##;

    let config = PrismConfig::from_chromaterm_yaml(yaml).expect("chromaterm config loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");

    let output = highlighter.highlight_str("1 22 333 axxb\n");

    assert_eq!(
        output,
        "\x1b[38;2;0;255;255m1\x1b[0m \x1b[38;2;0;255;255m22\x1b[0m \x1b[38;2;0;255;255m333\x1b[0m a\x1b[4mxx\x1b[0mb\n"
    );
}

#[test]
fn duplicate_capture_names_resolve_like_pcre2() {
    // PCRE2 allows duplicate group names with (?J); its name lookup returns the
    // last group carrying the name, so a style keyed on that name must follow
    // the same resolution.
    let yaml = r##"
rules:
  - description: either branch
    regex: "(?J)(?P<word>alpha)|(?P<word>beta)"
    color:
      word: f#00ffff
"##;

    let config = PrismConfig::from_chromaterm_yaml(yaml).expect("chromaterm config loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");

    let output = highlighter.highlight_str("beta\n");

    assert_eq!(output, "\x1b[38;2;0;255;255mbeta\x1b[0m\n");
}

#[test]
fn ansi16_color_mode_keeps_core_prismtty_colors_distinct_with_short_sgr() {
    let interface = Style::parse("f#0099ff").expect("interface style parses");
    let ip = Style::parse("f#00ffff").expect("ip style parses");
    let up = Style::parse("f#00ff00").expect("up style parses");
    let down = Style::parse("f#ff0000").expect("down style parses");
    let warning = Style::parse("f#ff9900").expect("warning style parses");
    let mac = Style::parse("f#ff9aff").expect("mac style parses");

    assert_eq!(
        interface.ansi_start_with_mode(ColorMode::Ansi16),
        "\x1b[94m"
    );
    assert_eq!(ip.ansi_start_with_mode(ColorMode::Ansi16), "\x1b[96m");
    assert_eq!(up.ansi_start_with_mode(ColorMode::Ansi16), "\x1b[92m");
    assert_eq!(down.ansi_start_with_mode(ColorMode::Ansi16), "\x1b[91m");
    assert_eq!(warning.ansi_start_with_mode(ColorMode::Ansi16), "\x1b[33m");
    assert_eq!(mac.ansi_start_with_mode(ColorMode::Ansi16), "\x1b[95m");
}

#[test]
fn loads_chromaterm_palette_named_captures_group_zero_and_extra_styles() {
    let yaml = r##"
palette:
  prompt-user: "#00bfff"
  prompt-host: "#ffffff"
  banner: "#333333"
rules:
  - description: whole prompt fallback
    regex: (?P<user>\w+)(@)(?P<host>[\w-]+>)
    color:
      user: f.prompt-user bold underline
      host: f.prompt-host italic
  - description: whole line background
    regex: ^banner$
    color:
      0: b.banner invert strike blink
"##;

    let config = PrismConfig::from_chromaterm_yaml(yaml).expect("chromaterm config loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");

    let output = highlighter.highlight_str("admin@router1>\nbanner\n");

    assert!(output.contains("\x1b[1;4;38;2;0;191;255madmin\x1b[0m"));
    assert!(output.contains("\x1b[3;38;2;255;255;255mrouter1>\x1b[0m"));
    assert!(output.contains("\x1b[5;7;9;48;2;51;51;51mbanner\x1b[0m"));
}

#[test]
fn non_exclusive_chromaterm_rules_merge_attributes_by_type() {
    let yaml = r##"
rules:
  - description: error foreground
    regex: error
    color: f#ff0000
  - description: error underline
    regex: error
    color: underline
"##;

    let config = PrismConfig::from_chromaterm_yaml(yaml).expect("chromaterm config loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");

    let output = highlighter.highlight_str("error\n");

    assert!(output.contains("\x1b[4;38;2;255;0;0merror\x1b[0m"));
}

#[test]
fn exclusive_chromaterm_rules_prevent_later_rules_inside_their_span() {
    let yaml = r##"
rules:
  - description: quoted string
    regex: '"[^"]+"'
    exclusive: true
    color: f#ffffff
  - description: bad status
    regex: error
    color: f#ff0000 bold
"##;

    let config = PrismConfig::from_chromaterm_yaml(yaml).expect("chromaterm config loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");

    let output = highlighter.highlight_str(r#""error" error"#);

    assert!(output.contains("\x1b[38;2;255;255;255m\"error\"\x1b[0m"));
    assert!(output.contains("\x1b[1;38;2;255;0;0merror\x1b[0m"));
    assert!(!output.contains("\x1b[1;38;2;255;0;0merror\"\x1b[0m"));
}

#[test]
fn exclusive_chromaterm_rules_block_later_matches_that_cross_their_span() {
    let yaml = r##"
rules:
  - description: protected word
    regex: error
    exclusive: true
    color: f#ffffff
  - description: crossing phrase
    regex: error next
    color: f#ff0000 bold
"##;

    let config = PrismConfig::from_chromaterm_yaml(yaml).expect("chromaterm config loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");

    let output = highlighter.highlight_str("error next");

    assert!(output.contains("\x1b[38;2;255;255;255merror\x1b[0m next"));
    assert!(!output.contains("\x1b[1;38;2;255;0;0mnext\x1b[0m"));
}

#[test]
fn chromaterm_prompt_rules_match_each_line_and_override_builtin_prompt_color() {
    let store = ProfileStore::builtin();
    let builtin = PrismConfig::from_profiles(&store, &["juniper"]).expect("juniper loads");
    let user = PrismConfig::from_chromaterm_yaml(
        r##"
rules:
  - description: User highlight Juniper
    regex: (^\w+)(@)(.*>)
    color:
      1: f#00bfff bold
      2: f#00ffc0 bold
      3: f#ffffff bold
"##,
    )
    .expect("chromaterm config loads");
    let highlighter = Highlighter::from_config(builtin.merge(user)).expect("rules compile");

    let output = highlighter.highlight_str("JUNOS banner\nlabuser@LAB-MX-01>\n");

    assert!(output.contains("\x1b[1;38;2;0;191;255mlabuser\x1b[0m"));
    assert!(output.contains("\x1b[1;38;2;0;255;192m@\x1b[0m"));
    assert!(output.contains("\x1b[1;38;2;255;255;255mLAB-MX-01>\x1b[0m"));
}

#[test]
fn preserves_existing_ansi_when_highlighting_visible_text() {
    let yaml = r##"
rules:
  - description: bad status
    regex: (?i)\berror\b
    color: f#ff0000 bold
"##;

    let config = PrismConfig::from_chromaterm_yaml(yaml).expect("chromaterm config loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");

    let output = highlighter.highlight_str("\x1b[32mservice error tail\x1b[0m\n");

    assert!(output.contains("\x1b[32m"));
    assert!(output.contains("\x1b[1;38;2;255;0;0merror\x1b[0m"));
    assert!(output.contains("service "));
    assert!(output.contains("error\x1b[0m\x1b[32m tail"));
}

#[test]
fn preserves_composed_native_sgr_after_highlighted_prompt_time() {
    let yaml = r##"
rules:
  - description: prompt time
    regex: \b\d{2}:\d{2}:\d{2}\b
    color: f#ffff80
"##;

    let config = PrismConfig::from_chromaterm_yaml(yaml).expect("chromaterm config loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let input = "\x1b[48;2;20;30;40m\x1b[38;2;200;210;220m10:29:58 PM\x1b[0m\n";

    let output = highlighter.highlight_str(input);

    assert!(output.contains("\x1b[38;2;255;255;128m10:29:58\x1b[0m"));
    assert!(
        output.contains("\x1b[38;2;200;210;220;48;2;20;30;40m PM")
            || output.contains("\x1b[48;2;20;30;40;38;2;200;210;220m PM"),
        "{output:?}"
    );
}

#[test]
fn streaming_highlighter_bypasses_alternate_screen_apps() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = StreamingHighlighter::new(highlighter);
    let htop_like = "\x1b[?1049h\x1b[38;2;77;166;255m192.0.2.1 down\x1b[0m\x1b[?1049l";

    let output = streaming.push_str(&format!(
        "before 198.51.100.10 {htop_like} after 198.51.100.11"
    ));
    let output = format!(
        "{output}{}",
        String::from_utf8(streaming.finish()).expect("output remains UTF-8")
    );

    assert!(output.contains("\x1b[38;2;0;255;255m198.51.100.10\x1b[0m"));
    assert!(output.contains(htop_like));
    assert!(output.contains("\x1b[38;2;0;255;255m198.51.100.11\x1b[0m"));
    assert!(!output.contains("\x1b[38;2;0;255;255m192.0.2.1"));
}

#[test]
fn streaming_highlighter_keeps_alternate_screen_bypass_across_chunks() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = StreamingHighlighter::new(highlighter);

    let first = streaming.push_str("\x1b[?1049h\x1b[38;2;77;166;255m192.0.");
    let second = streaming.push_str("2.1 down\x1b[0m");
    let third = streaming.push_str("\x1b[?1049l 198.51.100.11");
    let output = format!(
        "{first}{second}{third}{}",
        String::from_utf8(streaming.finish()).expect("output remains UTF-8")
    );

    assert!(output.contains("\x1b[38;2;77;166;255m192.0.2.1 down\x1b[0m"));
    assert!(output.contains("\x1b[38;2;0;255;255m198.51.100.11\x1b[0m"));
    assert!(!output.contains("\x1b[38;2;0;255;255m192.0.2.1"));
}

#[test]
fn interactive_streaming_highlighter_does_not_token_buffer_alternate_screen_chunks() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let enter = "\x1b[?1049hPID USER Command";
    let redraw = "\x1b[H1234 admin running";

    assert_eq!(streaming.push_str(enter), enter);
    assert_eq!(streaming.push_str(redraw), redraw);
}

#[test]
fn interactive_streaming_highlighter_buffers_only_incomplete_alternate_screen_escapes() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let first = streaming.push_str("\x1b[?1049hCPU%\x1b[");
    let second = streaming.push_str("39mPID USER Command");

    assert_eq!(first, "\x1b[?1049hCPU%");
    assert_eq!(second, "\x1b[39mPID USER Command");
}

#[test]
fn interactive_streaming_highlighter_preserves_htop_header_sgr_in_alternate_screen() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let enter = "\x1b[?1049h\x1b[1;40r\x1b(B\x1b[m\x1b[4l\x1b[?7h\x1b[?1h\x1b=";
    assert_eq!(streaming.push_str(enter), enter);

    let header = concat!(
        "\x1b[8;61HUptime: \x1b(B\x1b[0;1m\x1b[36m21 days, 16:11:14",
        "\x1b[10;3H\x1b(B\x1b[0m\x1b[32m\x1b[42m[",
        "\x1b[30m\x1b[42mMain",
        "\x1b[32m\x1b[42m]",
        "\x1b[11;5H\x1b[30m\x1b[42m\x1b[1K PID USER       PRI",
        "\x1b[30m\x1b[46m CPU%\x1b[30m\x1b[42mMEM%   TIME+  Command",
    );

    assert_eq!(streaming.push_str(header), header);
}

#[test]
fn interactive_streaming_highlighter_applies_user_rules_in_alternate_screen() {
    let config = PrismConfig::from_chromaterm_yaml(
        r##"
rules:
  - description: process owner
    regex: \boperator\b
    color: f#00ffff
"##,
    )
    .expect("chromaterm config loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let enter = "\x1b[?1049h\x1b[1;40r\x1b(B\x1b[m\x1b[?7h";
    assert_eq!(streaming.push_str(enter), enter);

    let row = "\x1b[13;5H\x1b[39;49m\x1b(B\x1b[m1234 operator\x1b[22G17";
    let output = streaming.push_str(row);

    assert!(output.contains("\x1b[13;5H"));
    assert!(
        output.contains("\x1b[38;2;0;255;255moperator"),
        "{output:?}"
    );
    assert_eq!(strip_ansi(output.as_bytes()), strip_ansi(row.as_bytes()));
}

#[test]
fn interactive_streaming_highlighter_keeps_charset_sequences_intact_inside_overlay() {
    let config = PrismConfig::from_chromaterm_yaml(
        r##"
rules:
  - description: process owner
    regex: \boperator\b
    color: f#00ffff
"##,
    )
    .expect("chromaterm config loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let enter = "\x1b[?1049h\x1b[1;40r\x1b(B\x1b[m\x1b[?7h";
    assert_eq!(streaming.push_str(enter), enter);

    let row = "1234 operator \x1b(B running";
    let output = streaming.push_str(row);

    assert!(output.contains("operator \x1b(B"), "{output:?}");
    assert!(!output.contains("\x1b(\x1b["), "{output:?}");
    assert_eq!(strip_ansi(output.as_bytes()), strip_ansi(row.as_bytes()));
}

#[test]
fn highlighter_preserves_utf8_terminal_glyphs_without_mojibake() {
    let config = PrismConfig::default();
    let highlighter = Highlighter::from_config(config).expect("empty config compiles");
    let input = "CPU: ━━━━━━━ ã é 🚀 \n";

    let output = highlighter.highlight_str(input);

    assert_eq!(output, input);
}

#[test]
fn detects_builtin_profiles_from_network_prompts_and_output() {
    let store = ProfileStore::builtin();

    let juniper = store.detect_profiles("admin@mx480> show route\n");
    assert!(juniper.iter().any(|profile| profile == "juniper"));

    let cisco = store.detect_profiles("Router#show ip interface brief\n");
    assert!(cisco.iter().any(|profile| profile == "cisco"));

    let palo_alto = store.detect_profiles("admin@PA-VM> show system info\n");
    assert!(palo_alto.iter().any(|profile| profile == "palo-alto"));

    let linux = store.detect_profiles("root@server:~# systemctl status sshd\n");
    assert!(linux.iter().any(|profile| profile == "linux-unix"));

    let ubuntu_banner = store.detect_profiles(
        "OS: Ubuntu 24.04.4 LTS x86_64\nKernel: Linux 6.8.0-110-generic\nTerminal: /dev/pts/0\n",
    );
    assert!(ubuntu_banner.iter().any(|profile| profile == "linux-unix"));

    let cisco_prompt = store.detect_profiles("CORE-SW01#show version\nCisco IOS XE Software\n");
    assert!(cisco_prompt.iter().any(|profile| profile == "cisco"));

    let nexus_banner = store.detect_profiles("Cisco Nexus Operating System Software\n");
    assert!(nexus_banner.iter().any(|profile| profile == "cisco"));

    let fortinet_prompt = store.detect_profiles("FGVM04TM22000000 # get system status\n");
    assert!(fortinet_prompt.iter().any(|profile| profile == "fortinet"));
}

#[test]
fn detects_and_highlights_arubacx_profile() {
    let store = ProfileStore::builtin();
    let detected = store.detect_profiles(
        "LAB-ARUBA-01# show version\nArubaOS-CX 10.13.1000\nhpe-restd Event|7708|LOG_INFO|AMM|1/1|\n",
    );
    assert!(detected.iter().any(|profile| profile == "arubacx"));
    assert!(!detected.iter().any(|profile| profile == "cisco"));

    let config = PrismConfig::from_profiles(&store, &["arubacx"]).expect("arubacx loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let output = highlighter.highlight_str("1/1/1 up\nlag10 up\nvlan1191 down\n");

    assert!(output.contains("\x1b[38;2;0;153;255m1/1/1"));
    assert!(output.contains("\x1b[38;2;0;153;255mlag10"));
    assert!(output.contains("\x1b[38;2;0;153;255mvlan1191"));
}

#[test]
fn detects_arista_from_ceos_banner_and_highlights_eos_interfaces() {
    let store = ProfileStore::builtin();
    let detected = store.detect_profiles(
        "ceos-lab-01#show version\nArista cEOSLab\nSoftware image version: 4.36.0F\n",
    );
    assert!(detected.iter().any(|profile| profile == "arista"));

    let config = PrismConfig::from_profiles(&store, &["arista"]).expect("arista loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let output =
        highlighter.highlight_str("Ethernet1 up\nEt2 down\nPort-Channel10 up\nVlan1191 up\n");

    assert!(output.contains("\x1b[38;2;0;153;255mEthernet1"));
    assert!(output.contains("\x1b[38;2;0;153;255mEt2"));
    assert!(output.contains("\x1b[38;2;0;153;255mPort-Channel10"));
    assert!(output.contains("\x1b[38;2;0;153;255mVlan1191"));
}

#[test]
fn cisco_nexus_interface_status_highlights_operational_reason_codes() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");

    let output = highlighter.highlight_str(
        "Eth1/31 LAB suspended trunk auto auto QSFP-40G-SR-BD\n\
         Eth1/50 LAB notconnec trunk auto auto 10Gbase-SR\n\
         Eth1/56 -- xcvrAbsen routed auto auto --\n\
         Po24 LAB noOperMem trunk auto auto --\n",
    );

    assert!(
        output.contains("\x1b[1;38;2;255;0;0msuspended\x1b[0m"),
        "{output:?}"
    );
    assert!(
        output.contains("\x1b[1;38;2;255;0;0mnotconnec\x1b[0m"),
        "{output:?}"
    );
    assert!(
        output.contains("\x1b[38;2;255;165;0mxcvrAbsen\x1b[0m"),
        "{output:?}"
    );
    assert!(
        output.contains("\x1b[38;2;255;165;0mnoOperMem\x1b[0m"),
        "{output:?}"
    );
}

#[test]
fn cisco_nexus_mac_table_keeps_mac_and_interface_colors_with_plus_line_rule() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"])
        .expect("cisco loads")
        .merge(
            PrismConfig::from_chromaterm_yaml(
                r##"
rules:
  - description: added line
    regex: '(?m)^\+ .*$'
    color: f#00dc1a
"##,
            )
            .expect("custom line rule parses"),
        );
    let highlighter = Highlighter::from_config(config).expect("rules compile");

    let spans = highlighter
        .style_spans(b"+   2      0018.2302.e255   dynamic   NA          F      F    Po27\n");

    let mac = spans
        .iter()
        .find(|span| span.text == "0018.2302.e255")
        .expect("MAC address span is highlighted");
    let port = spans
        .iter()
        .find(|span| span.text == "Po27")
        .expect("Nexus port-channel span is highlighted");

    assert_eq!(
        mac.style.foreground,
        Some(Rgb {
            r: 255,
            g: 154,
            b: 255
        })
    );
    assert_eq!(
        port.style.foreground,
        Some(Rgb {
            r: 0,
            g: 153,
            b: 255
        })
    );
}

#[test]
fn palo_alto_profile_highlights_interfaces() {
    let store = ProfileStore::builtin();
    let detected = store
        .detect_profiles("admin@pa-lab-01> show system info\nmodel: PA-VM\nsw-version: 11.1.0\n");
    assert!(detected.iter().any(|profile| profile == "palo-alto"));
    assert!(!detected.iter().any(|profile| profile == "juniper"));

    let config = PrismConfig::from_profiles(&store, &["palo-alto"]).expect("palo alto loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let output = highlighter
        .highlight_str("ethernet1/1 up\nethernet1/2.1191 up\ntunnel.10 down\nloopback.1 up\n");

    assert!(output.contains("\x1b[38;2;0;153;255methernet1/1"));
    assert!(output.contains("\x1b[38;2;0;153;255methernet1/2.1191"));
    assert!(output.contains("\x1b[38;2;0;153;255mtunnel.10"));
    assert!(output.contains("\x1b[38;2;0;153;255mloopback.1"));
}

#[test]
fn versa_profile_highlights_interfaces_and_bgp_state() {
    let store = ProfileStore::builtin();
    let detected = store
        .detect_profiles("admin@versa-lab-01> show interfaces brief\nSoftware Version: 22.1.4\n");
    assert!(detected.iter().any(|profile| profile == "versa"));
    assert!(!detected.iter().any(|profile| profile == "juniper"));

    let config = PrismConfig::from_profiles(&store, &["versa"]).expect("versa loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let output = highlighter.highlight_str("vni-0/0 up\ntvi-0/332 up\nptvi-1 down\nEstablished\n");

    assert!(output.contains("\x1b[38;2;0;153;255mvni-0/0"));
    assert!(output.contains("\x1b[38;2;0;153;255mtvi-0/332"));
    assert!(output.contains("\x1b[38;2;0;153;255mptvi-1"));
    assert!(output.contains("\x1b[1;38;2;77;166;255mEstablished"));
}

#[test]
fn arista_detection_does_not_match_generic_eos_substring() {
    let store = ProfileStore::builtin();
    let detected = store.detect_profiles("stereos service ok\n");
    assert!(!detected.iter().any(|profile| profile == "arista"));
}

#[test]
fn arista_detection_requires_vendor_context_for_software_image_version() {
    let store = ProfileStore::builtin();
    let detected =
        store.detect_profiles("router# show version\nSoftware image version: generic 1.0\n");

    assert!(detected.iter().any(|profile| profile == "cisco"));
    assert!(!detected.iter().any(|profile| profile == "arista"));
}

#[test]
fn panos_and_versa_detection_do_not_match_cisco_interface_brief() {
    let store = ProfileStore::builtin();
    let detected = store.detect_profiles(
        "CORE# show interfaces brief\nInterface Ethernet1/1 is up\nCisco IOS XE Software\n",
    );
    assert!(detected.iter().any(|profile| profile == "cisco"));
    assert!(!detected.iter().any(|profile| profile == "palo-alto"));
    assert!(!detected.iter().any(|profile| profile == "versa"));
}

#[test]
fn palo_alto_detection_requires_vendor_context_for_show_system_info() {
    let store = ProfileStore::builtin();
    let detected = store.detect_profiles("router# show system info\nmodel: generic\n");

    assert!(detected.iter().any(|profile| profile == "cisco"));
    assert!(!detected.iter().any(|profile| profile == "palo-alto"));
}

#[test]
fn versa_detection_does_not_match_ordinary_words() {
    let store = ProfileStore::builtin();
    let detected = store.detect_profiles("universal serial console ready\n");

    assert!(!detected.iter().any(|profile| profile == "versa"));
}

#[test]
fn arubacx_detection_does_not_match_plain_cisco_prompt() {
    let store = ProfileStore::builtin();
    let detected = store.detect_profiles("CORE-SW01#show version\nCisco IOS XE Software\n");

    assert!(detected.iter().any(|profile| profile == "cisco"));
    assert!(!detected.iter().any(|profile| profile == "arubacx"));
}

#[test]
fn arubacx_detection_requires_aruba_specific_event_context() {
    let store = ProfileStore::builtin();
    let detected = store.detect_profiles("Application event|123|LOG_INFO|service\n");

    assert!(!detected.iter().any(|profile| profile == "arubacx"));
}

#[test]
fn arubacx_detection_does_not_match_generic_show_interface_brief() {
    let store = ProfileStore::builtin();
    let detected =
        store.detect_profiles("CORE-SW01# show interface brief\nCisco IOS XE Software\n");

    assert!(detected.iter().any(|profile| profile == "cisco"));
    assert!(!detected.iter().any(|profile| profile == "arubacx"));
}

#[test]
fn macos_zsh_prompt_does_not_detect_as_juniper() {
    let store = ProfileStore::builtin();

    let profiles = store.detect_profiles("labuser@mac-lab:~ %\n");

    assert!(profiles.iter().any(|profile| profile == "generic"));
    assert!(!profiles.iter().any(|profile| profile == "juniper"));
}

#[test]
fn builtins_highlight_common_network_and_vendor_terms() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic", "cisco", "juniper", "fortinet"])
        .expect("built-in profiles load");
    let highlighter = Highlighter::from_config(config).expect("built-in rules compile");

    let output = highlighter.highlight_str(
        "Gi0/1 is up, line protocol is down\nge-0/0/0 FULL 192.0.2.1\nFGT # diagnose vpn tunnel down\n",
    );

    assert!(output.contains("Gi0/1"));
    assert!(output.contains("ge-0/0/0"));
    assert!(output.contains("192.0.2.1"));
    assert!(output.contains("\x1b["));
}

fn whole_and_chunked_streaming_output(
    profile: &str,
    input: &str,
    chunk_size: usize,
) -> (String, String) {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &[profile]).expect("built-in profile loads");
    let highlighter = Highlighter::from_config(config).expect("profile rules compile");
    let whole = highlighter.highlight_str(input);
    let mut streaming = StreamingHighlighter::new(highlighter);
    let mut chunked = String::new();

    for chunk in input.as_bytes().chunks(chunk_size) {
        chunked.push_str(
            &String::from_utf8(streaming.push(chunk))
                .expect("synthetic fixture remains valid UTF-8"),
        );
    }
    chunked.push_str(
        &String::from_utf8(streaming.finish()).expect("synthetic fixture remains valid UTF-8"),
    );

    (whole, chunked)
}

#[test]
fn cisco_streaming_output_matches_whole_output_byte_for_byte_snapshot() {
    let input = "Router# show interfaces description\nEth1/1 up up Uplink to CORE\nVlan1191 up up Internal VLAN\n";
    let (whole, chunked) = whole_and_chunked_streaming_output("cisco", input, 7);

    assert_eq!(chunked.as_bytes(), whole.as_bytes());
}

#[test]
fn juniper_streaming_output_matches_whole_output_byte_for_byte_snapshot() {
    let input = "ge-0/0/0 up up Core uplink\nreth1.816 up up inet\nst0.1078 down down VPN tunnel\n";
    let (whole, chunked) = whole_and_chunked_streaming_output("juniper", input, 5);

    assert_eq!(chunked.as_bytes(), whole.as_bytes());
}

#[test]
fn streaming_highlighter_keeps_split_interface_tokens_consistently_colored() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["juniper"]).expect("juniper loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = StreamingHighlighter::new(highlighter);

    let first = streaming.push_str("gr-0");
    let second = streaming.push_str("/0/0.1 up up zscaler-primary\n");
    let flushed = streaming.finish();
    let output = format!(
        "{first}{second}{}",
        String::from_utf8(flushed).expect("ASCII test input remains valid UTF-8")
    );

    assert!(output.contains("\x1b[38;2;0;153;255mgr-0/0/0.1\x1b[0m"));
}

#[test]
fn streaming_highlighter_keeps_char_split_ipv4_addresses_colored() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = StreamingHighlighter::new(highlighter);

    let mut output = String::new();
    for byte in "router-id 192.0.2.22\r\nnetwork 198.51.100.0 0.0.0.255 area 0\r\n".bytes() {
        output.push_str(
            &String::from_utf8(streaming.push(&[byte]))
                .expect("ASCII test input remains valid UTF-8"),
        );
    }
    output.push_str(
        &String::from_utf8(streaming.finish()).expect("ASCII test input remains valid UTF-8"),
    );

    assert!(output.contains("\x1b[38;2;0;255;255m192.0.2.22\x1b[0m"));
    assert!(output.contains("\x1b[38;2;0;255;255m198.51.100.0\x1b[0m"));
    assert!(output.contains("\x1b[38;2;0;255;255m0.0.0.255\x1b[0m"));
}

#[test]
fn streaming_highlighter_bounds_oversized_unterminated_escapes() {
    let config = PrismConfig::default();
    let highlighter = Highlighter::from_config(config).expect("empty config compiles");

    for (prefix, terminator) in [
        (b"\x1b]52;".as_slice(), b"\x07".as_slice()),
        (b"\x1bP".as_slice(), b"\x1b\\".as_slice()),
    ] {
        let mut streaming = StreamingHighlighter::new(highlighter.clone());
        let mut input = prefix.to_vec();
        input.extend(std::iter::repeat_n(b'A', 20_000));

        let output = streaming.push(&input);
        assert!(output.is_empty(), "string payload leaked for {prefix:?}");
        let mut recovery = terminator.to_vec();
        recovery.extend_from_slice(b"visible\n");
        assert_eq!(streaming.push(&recovery), b"visible\n");
        assert!(streaming.finish().is_empty());
    }

    let mut streaming = StreamingHighlighter::new(highlighter);
    let mut csi = b"\x1b[".to_vec();
    csi.extend(std::iter::repeat_n(b'1', 20_000));
    let output = streaming.push(&csi);
    assert!(!output.is_empty(), "oversized CSI stayed buffered");
    assert!(!output.contains(&0x1b), "oversized CSI emitted raw ESC");
}

#[test]
fn streaming_highlighter_discards_earliest_oversized_unterminated_string() {
    let config = PrismConfig::default();
    let highlighter = Highlighter::from_config(config).expect("empty config compiles");
    let mut streaming = StreamingHighlighter::new(highlighter);
    let mut input = b"\x1b]52;".to_vec();
    input.extend(std::iter::repeat_n(b'A', 17_000));
    input.extend(b"\x1b]52;");
    input.extend(std::iter::repeat_n(b'B', 1_000));

    let output = streaming.push(&input);

    assert!(output.is_empty(), "unterminated OSC payload leaked");
    assert_eq!(streaming.push(b"\x07visible\n"), b"visible\n");
}

#[test]
fn streaming_highlighter_treats_nested_oversized_strings_as_payload() {
    let config = PrismConfig::default();
    let highlighter = Highlighter::from_config(config).expect("empty config compiles");
    let mut streaming = StreamingHighlighter::new(highlighter);
    let mut input = Vec::new();
    for idx in 0..20u8 {
        input.extend(b"\x1b]52;");
        input.extend(std::iter::repeat_n(b'A' + (idx % 26), 17_000));
    }

    let output = streaming.push(&input);

    assert!(output.is_empty(), "nested unterminated OSC payload leaked");
    assert!(streaming.finish().is_empty());
}

#[test]
fn streaming_highlighter_bounds_unterminated_controls_across_many_small_pushes() {
    let highlighter = Highlighter::from_config(PrismConfig::default()).expect("empty config");

    for (prefix, terminator) in [
        (b"\x1b]52;".as_slice(), b"\x07".as_slice()),
        (b"\x1bP".as_slice(), b"\x1b\\".as_slice()),
        (b"\x9d52;".as_slice(), b"\x07".as_slice()),
    ] {
        let mut streaming = StreamingHighlighter::new(highlighter.clone());
        let mut output = streaming.push(prefix);
        let payload = vec![b'x'; 17 * 1024];
        for chunk in payload.chunks(127) {
            output.extend(streaming.push(chunk));
        }

        assert!(output.is_empty(), "over-limit {prefix:?} payload leaked");
        let mut recovery = terminator.to_vec();
        recovery.extend_from_slice(b"visible\n");
        assert_eq!(streaming.push(&recovery), b"visible\n");
        assert!(streaming.finish().is_empty());
    }
}

#[test]
fn streaming_highlighter_preserves_split_terminated_osc_sequence() {
    let config = PrismConfig::default();
    let highlighter = Highlighter::from_config(config).expect("empty config compiles");
    let mut streaming = StreamingHighlighter::new(highlighter);

    let first = streaming.push(b"\x1b]0;router");
    let second = streaming.push(b"\x07ready\n");
    let mut output = first;
    output.extend(second);
    output.extend(streaming.finish());

    assert_eq!(output.as_slice(), b"\x1b]0;router\x07ready\n");
    assert_eq!(strip_ansi(&output), b"ready\n");
}

#[test]
fn interactive_streaming_highlighter_does_not_buffer_slow_typed_echoes() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    assert_eq!(streaming.push_str("router# "), "router# ");
    assert_eq!(streaming.push_str("s"), "s");
    assert_eq!(streaming.push_str("h"), "h");
    assert_eq!(streaming.push_str("o"), "o");
    assert_eq!(streaming.push_str("w"), "w");
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_does_not_buffer_coalesced_typed_echoes() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    assert_eq!(streaming.push_str("router# "), "router# ");
    assert_eq!(streaming.push_str("show"), "show");
    assert_eq!(streaming.push_str(" interfaces"), " interfaces");
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_does_not_buffer_unicode_prompt_typed_echoes() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    assert_eq!(streaming.push_str("○ "), "○ ");
    assert_eq!(streaming.push_str("m"), "m");
    assert_eq!(streaming.push_str("v"), "v");
    assert_eq!(
        streaming.push_str(" ISAD-61576-Security_Switches"),
        " ISAD-61576-Security_Switches"
    );
    assert_eq!(
        streaming.push_str("_Le_Bourget-Hartfors-v1.1.docx"),
        "_Le_Bourget-Hartfors-v1.1.docx"
    );
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_does_not_buffer_decorative_unicode_prompt_typed_echoes() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    assert_eq!(
        streaming.push_str("\r╭─user at host in ~\r\n╰─○ "),
        "\r╭─user at host in ~\r\n╰─○ "
    );
    assert_eq!(streaming.push_str("m"), "m");
    assert_eq!(streaming.push_str("v"), "v");
    assert_eq!(
        streaming.push_str(" ISAD-61576-Security_Switches"),
        " ISAD-61576-Security_Switches"
    );
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_keeps_decorative_unicode_prompt_echo_after_line_edit_redraw() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    assert_eq!(streaming.push_str("╰─○ "), "╰─○ ");
    assert_eq!(streaming.push_str("mv old new"), "mv old new");

    let redraw = "\x1b[10Dold-renamed\x1b[7D";
    assert_eq!(streaming.push_str(redraw), redraw);
    assert_eq!(streaming.push_str("s"), "s");
    assert_eq!(streaming.push_str("a"), "a");
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_does_not_buffer_coalesced_unicode_prompt_echoes() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let input = "○ mv ISAD-61576-Security_Switches_Le_Bourget-Hartfors-v1.1.docx ISAD-61576-Security_Switches";

    assert_eq!(streaming.push_str(input), input);
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_does_not_highlight_coalesced_prompt_and_echo_before_enter() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let input = "router# show interfaces up down";
    let output = streaming.push_str(input);
    assert_eq!(output, input);
    assert!(!output.contains("\x1b[1;38;2;255;0;0mdown"));
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_does_not_highlight_long_pasted_echo_before_enter() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);
    let paste = "show interfaces up down ".repeat(16);

    assert_eq!(streaming.push_str("router# "), "router# ");
    assert_eq!(streaming.push_str(&paste), paste);
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_does_not_highlight_ansi_line_edit_echo_before_enter() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    assert_eq!(streaming.push_str("router# "), "router# ");
    let output = streaming.push_str("\x1b[Kup down");

    assert!(output.contains("\x1b[Kup down"), "{output:?}");
    assert!(!output.contains("\x1b[38;2;0;255;0mup"), "{output:?}");
    assert!(!output.contains("\x1b[1;38;2;255;0;0mdown"), "{output:?}");
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_does_not_highlight_cr_redraw_echo_before_enter() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    assert_eq!(streaming.push_str("router# "), "router# ");
    let redraw = "\r\x1b[Kup down";
    let output = streaming.push_str(redraw);
    assert_eq!(output, redraw);
    assert!(!output.contains("\x1b[1;38;2;255;0;0mdown"));
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_highlights_device_prompts_without_bold_or_full_reset() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let output = streaming.push_str("LAB-N9K-01#");

    assert!(output.contains("\x1b[38;2;255;255;255mLAB-N9K-01#"));
    assert!(!output.contains("\x1b[1;38;2;255;255;255m"), "{output:?}");
    assert!(!output.contains("\x1b[0m"), "{output:?}");
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_highlights_trailing_device_prompt_without_full_reset() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let output = streaming.push_str("Vlan1107    Plant 3 Shopfloor Device Vlan\nLAB-N9K-01#");

    assert!(
        output.contains("\x1b[38;2;0;153;255mVlan1107"),
        "{output:?}"
    );
    assert!(output.contains("\x1b[38;2;255;255;255mLAB-N9K-01#"));
    assert!(!output.contains("\x1b[0m"), "{output:?}");
    assert!(
        !output.contains("\x1b[1;38;2;255;255;255mLAB-N9K-01#"),
        "{output:?}"
    );
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_highlights_repeated_device_prompts_without_full_reset() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let output = streaming.push_str("\r\nLAB-N9K-01#\r\nLAB-N9K-01#\r\nLAB-N9K-01#");

    assert_eq!(
        count_occurrences(&strip_ansi(output.as_bytes()), b"LAB-N9K-01#"),
        3
    );
    assert_all_token_occurrences_have_foreground(&output, "LAB-N9K-01#", "38;2;255;255;255");
    assert!(!output.contains("\x1b[0m"), "{output:?}");
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_highlights_question_mark_help_prompt_without_full_reset() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let output = streaming.push_str("% Type 'show ?' for a list of subcommands\r\nLAB-N9K-01#");

    assert!(output.contains("% Type 'show ?' for a list of subcommands"));
    assert!(output.contains("\x1b[38;2;255;255;255mLAB-N9K-01#"));
    assert!(!output.contains("\x1b[0m"), "{output:?}");
    assert!(
        !output.contains("\x1b[1;38;2;255;255;255mLAB-N9K-01#"),
        "{output:?}"
    );
}

#[test]
fn interactive_streaming_highlighter_keeps_cisco_help_redraw_command_tail_visible() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let _ = streaming.push_str("LAB-N9K-CORE-01# ");
    assert_eq!(streaming.push_str("sh mac"), "sh mac");

    let help_redraw = concat!(
        "?\r\n",
        "  mac       MAC configuration commands\r\n",
        "  mac-list  Show mac-lists\r\n",
        "  mac-move  Display mac-move policy\r\n",
        "\r\n",
        "\x1b[23D\x1b[J\rLAB-N9K-CORE-01# sh mac",
    );

    let output = streaming.push_str(help_redraw);
    let visible = strip_ansi(output.as_bytes());

    assert!(visible.ends_with(b"LAB-N9K-CORE-01# sh mac"), "{output:?}");
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_keeps_cisco_help_redraw_command_tail_visible_split() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let _ = streaming.push_str("LAB-N9K-CORE-01# ");
    assert_eq!(streaming.push_str("sh mac"), "sh mac");

    // Push the help menu part
    let help_menu = "?\r\n  mac       MAC configuration commands\r\n  mac-list  Show mac-lists\r\n  mac-move  Display mac-move policy\r\n\r\n";
    let out1 = streaming.push_str(help_menu);
    assert_eq!(strip_ansi(out1.as_bytes()), help_menu.as_bytes());

    // Push the cursor positioning sequence
    let cursor_pos = "\x1b[23D\x1b[J\r";
    let out2 = streaming.push_str(cursor_pos);
    assert_eq!(out2, cursor_pos);

    // Push the prompt + command tail
    let redraw = "LAB-N9K-CORE-01# sh mac";
    let out3 = streaming.push_str(redraw);
    assert_eq!(strip_ansi(out3.as_bytes()), redraw.as_bytes());

    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_keeps_cisco_help_redraw_command_tail_visible_split_2() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let _ = streaming.push_str("LAB-N9K-CORE-01# ");
    assert_eq!(streaming.push_str("sh mac"), "sh mac");

    // Push the help menu part
    let help_menu = "?\r\n  mac       MAC configuration commands\r\n  mac-list  Show mac-lists\r\n  mac-move  Display mac-move policy\r\n\r\n";
    let out1 = streaming.push_str(help_menu);
    assert_eq!(strip_ansi(out1.as_bytes()), help_menu.as_bytes());

    // Push the cursor positioning sequence + prompt
    let cursor_pos_prompt = "\x1b[23D\x1b[J\rLAB-N9K-CORE-01# sh ";
    let out2 = streaming.push_str(cursor_pos_prompt);
    assert_eq!(strip_ansi(out2.as_bytes()), b"\rLAB-N9K-CORE-01# sh ");

    // Push the command tail
    let redraw_tail = "mac";
    let out3 = streaming.push_str(redraw_tail);
    assert_eq!(strip_ansi(out3.as_bytes()), redraw_tail.as_bytes());

    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_keeps_cisco_help_redraw_command_tail_visible_split_3() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let _ = streaming.push_str("LAB-N9K-CORE-01# ");
    assert_eq!(streaming.push_str("sh mac"), "sh mac");

    // Push the help menu part
    let help_menu = "?\r\n  mac       MAC configuration commands\r\n  mac-list  Show mac-lists\r\n  mac-move  Display mac-move policy\r\n\r\n";
    let out1 = streaming.push_str(help_menu);
    assert_eq!(strip_ansi(out1.as_bytes()), help_menu.as_bytes());

    // Push the prompt + command tail with only a carriage return redraw
    let redraw = "\rLAB-N9K-CORE-01# sh mac";
    let out2 = streaming.push_str(redraw);
    assert_eq!(strip_ansi(out2.as_bytes()), redraw.as_bytes());

    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_keeps_cisco_help_redraw_command_tail_visible_split_4() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let _ = streaming.push_str("Core(config-if)# ");
    assert_eq!(streaming.push_str("no ip ospf he"), "no ip ospf he");

    // Cisco help response when "?" is typed, ending with a redraw of the prompt and the command tail
    let help_redraw = "?\r\nhello-interval\r\nCore(config-if)#no ip ospf he";
    let output = streaming.push_str(help_redraw);
    let visible = strip_ansi(output.as_bytes());

    assert_eq!(
        std::str::from_utf8(&visible).unwrap(),
        "?\r\nhello-interval\r\nCore(config-if)#no ip ospf he"
    );
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_cisco_help_redraw_does_not_leak_colors() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let _ = streaming.push_str("LAB-N9K-CORE-01(config-if)# ");
    assert_eq!(streaming.push_str("ip osp"), "ip osp");

    // Cisco help response when "?" is typed after typing "osp":
    // The OSPF state "FULL" is printed and highlighted orange (color: f#ffa500, which in ansi is \x1b[38;2;255;165;0m)
    // Then it redraws the prompt.
    let help_redraw = "?\r\nFULL\r\nLAB-N9K-CORE-01(config-if)#ip osp";
    let output = streaming.push_str(help_redraw);

    // Let's assert that "FULL" is highlighted in orange (\x1b[38;2;255;165;0m)
    assert!(
        output.contains("\x1b[38;2;255;165;0mFULL"),
        "expected 'FULL' to be highlighted orange, got: {output:?}"
    );

    // Let's assert that the prompt "LAB-N9K-CORE-01" is reset and does not have the leaked orange color.
    // The reset sequence should be emitted before the prompt.
    assert!(
        output.contains("\x1b[39mLAB-N9K-CORE-01"),
        "expected reset before prompt to prevent color leakage, got: {output:?}"
    );
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_cisco_help_redraw_uses_full_reset_when_configured() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);
    streaming.set_no_minimal_resets(true);

    let _ = streaming.push_str("LAB-N9K-CORE-01(config-if)# ");
    assert_eq!(streaming.push_str("ip osp"), "ip osp");

    let help_redraw = "?\r\nFULL\r\nLAB-N9K-CORE-01(config-if)#ip osp";
    let output = streaming.push_str(help_redraw);

    assert!(
        output.contains("\x1b[38;2;255;165;0mFULL"),
        "expected 'FULL' to be highlighted orange, got: {output:?}"
    );
    assert!(
        output.contains("\x1b[0mLAB-N9K-CORE-01"),
        "expected full reset before prompt to prevent color leakage, got: {output:?}"
    );
    assert!(
        !output.contains("\x1b[39mLAB-N9K-CORE-01"),
        "expected full reset to replace minimal foreground reset, got: {output:?}"
    );
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_highlights_juniper_prompt_without_bold_or_full_reset() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["juniper"]).expect("juniper loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let output = streaming.push_str("labuser@LAB-WD02>\n");

    assert!(
        output.contains("\x1b[38;2;0;191;255mlabuser@LAB-WD02>"),
        "{output:?}"
    );
    assert!(!output.contains("\x1b[1;38;2;0;191;255m"), "{output:?}");
    assert!(!output.contains("\x1b[0m"), "{output:?}");
}

#[test]
fn interactive_streaming_highlighter_highlights_output_after_cr_only_command_echo() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let mut output = String::new();
    output.push_str(&streaming.push_str("LAB-N9K-01# "));
    output.push_str(&streaming.push_str("show vlan\rVlan1107    Plant 3 Shopfloor Device Vlan\n"));
    output.push_str(
        &String::from_utf8(streaming.finish()).expect("ASCII test input remains valid UTF-8"),
    );

    assert!(output.contains("show vlan\r"), "{output:?}");
    assert!(
        output.contains("\x1b[38;2;0;153;255mVlan1107"),
        "{output:?}"
    );
}

#[test]
fn interactive_streaming_highlighter_highlights_output_after_pager_clear() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let mut output = String::new();
    output.push_str(&streaming.push_str("LAB-N9K-01# "));
    output.push_str(&streaming.push_str("show vlan\r"));
    output.push_str(&streaming.push_str("\x1b[KVlan1107    Plant 3 Shopfloor Device Vlan\n"));
    output.push_str(
        &String::from_utf8(streaming.finish()).expect("ASCII test input remains valid UTF-8"),
    );

    assert!(output.contains("\x1b[K"), "{output:?}");
    assert!(
        output.contains("\x1b[38;2;0;153;255mVlan1107"),
        "{output:?}"
    );
}

#[test]
fn interactive_streaming_highlighter_preserves_zsh_redraws_before_enter() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["linux-unix"]).expect("linux profile loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    assert_eq!(
        streaming.push_str("labuser@mac-lab % "),
        "labuser@mac-lab % "
    );
    assert_eq!(streaming.push_str("sho"), "sho");
    let redraw = "\r\x1b[Klabuser@mac-lab % \x1b[38;5;244mshow ip route\x1b[0m";
    let output = streaming.push_str(redraw);

    assert_eq!(output, redraw);
    assert!(!output.contains("\x1b[38;2;0;255;192m"));
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_does_not_buffer_echoes_after_completion_redraw() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["linux-unix"]).expect("linux profile loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    assert_eq!(streaming.push_str("❯ "), "❯ ");
    assert_eq!(streaming.push_str("dig ptt"), "dig ptt");

    let redraw = "\r\r\n[host]\r\nalpha  beta\r\n\x1b[J\x1b[2A\r\x1b[2Cdig ptt";
    assert_eq!(streaming.push_str(redraw), redraw);
    assert_eq!(streaming.push_str("m"), "m");
    assert_eq!(streaming.push_str("m"), "m");

    let promptless_redraw = "\r\r\n[host]\r\nalpha  beta\r\n\x1b[J\x1b[2A\r\x1b[2Cdig maneki\x1b[K";
    assert_eq!(streaming.push_str(promptless_redraw), promptless_redraw);

    let line_edit_repaint = "\x08\x08\x08\x1b[24mm\x1b[24ma\x1b[24mn";
    assert_eq!(streaming.push_str(line_edit_repaint), line_edit_repaint);

    assert_eq!(streaming.push_str("m"), "m");
    assert_eq!(streaming.push_str("h"), "h");
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_rearms_echo_after_promptless_completion_redraw() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["linux-unix"]).expect("linux profile loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    assert_eq!(streaming.push_str("❯ "), "❯ ");
    assert_eq!(streaming.push_str("ssh man"), "ssh man");

    let menu = "\r\r\n[remote host name]\r\nalpha  beta\r\n";
    assert_eq!(streaming.push_str(menu), menu);

    let promptless_redraw = "\x1b[J\x1b[2A\r\x1b[2Cssh manen\x1b[K";
    assert_eq!(streaming.push_str(promptless_redraw), promptless_redraw);

    assert_eq!(streaming.push_str("k"), "k");
    assert_eq!(streaming.push_str("i"), "i");
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_keeps_echo_after_cursor_only_completion_repaint() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["linux-unix"]).expect("linux profile loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    assert_eq!(streaming.push_str("❯ "), "❯ ");
    assert_eq!(streaming.push_str("ping j"), "ping j");

    let completion_redraw =
        "\r\r\n[host]\r\nalpha  beta\r\n\x1b[J\x1b[3A\r\x1b[2C\x1b[32mping\x1b[39m j";
    assert_eq!(streaming.push_str(completion_redraw), completion_redraw);
    assert_eq!(streaming.push_str("i"), "i");

    let cursor_only_repaint = "\r\r\n\x1b[J\x1b[A\x1b[9C";
    assert_eq!(streaming.push_str(cursor_only_repaint), cursor_only_repaint);

    assert_eq!(streaming.push_str("m"), "m");
    assert_eq!(streaming.push_str("m"), "m");
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_recovers_highlighting_after_progress_cursor_positioning() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["linux-unix"]).expect("linux profile loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    // Multi-line progress output that ends with cursor-up + non-prompt text on
    // the trailing line (e.g., `git pack-objects`-style progress refreshing the
    // top line). The promptless-redraw heuristic may flag this and arm
    // prompt-echo passthrough; verify the next \n-terminated chunk recovers and
    // ordinary highlighting resumes.
    let progress = "Resolving deltas:  10% (123/1234)\x1b[1A";
    let _ = streaming.push_str(progress);
    let _ = streaming.push_str("\n");

    let recovered = streaming.push_str("192.0.2.1 OK\n");
    assert!(
        recovered.contains("\x1b[38;2;0;255;255m192.0.2.1"),
        "expected IP highlighting to recover after cursor-positioning progress chunk, got: {recovered:?}"
    );
    assert_eq!(
        String::from_utf8(streaming.finish()).expect("finish output is UTF-8"),
        "\x1b[39m"
    );
}

#[test]
fn interactive_streaming_highlighter_bypasses_fastfetch_cursor_painting() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["linux-unix"]).expect("linux profile loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let priming = streaming.push_str("192.0.2.1 ");
    assert!(
        priming.contains("\x1b[38;2;0;255;255m192.0.2.1"),
        "{priming:?}"
    );

    let logo = "\x1b[1m\x1b[31m             --+oossssssoo+--\r\n\x1b[m\x1b[1G";
    let info = "\x1b[1A\x1b[m\x1b[?7l\x1b[44C\x1b[m\x1b[1m\x1b[31mlabuser@linux-host\x1b[m\r\n";
    let logo_output = streaming.push_str(logo);
    let info_output = streaming.push_str(info);

    assert_eq!(
        strip_ansi(logo_output.as_bytes()),
        strip_ansi(logo.as_bytes())
    );
    assert_eq!(
        strip_ansi(info_output.as_bytes()),
        strip_ansi(info.as_bytes())
    );
    assert!(
        !logo_output.contains("\x1b[38;2;0;255;255m"),
        "{logo_output:?}"
    );
    assert!(
        !info_output.contains("\x1b[38;2;0;255;255m"),
        "{info_output:?}"
    );
    assert!(
        !info_output.contains("\x1b[38;2;0;191;255m"),
        "{info_output:?}"
    );
}

#[test]
fn interactive_streaming_highlighter_highlights_promptless_device_chunks() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["juniper"]).expect("juniper loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let first = streaming.push_str("st0.9");
    let second = streaming.push_str("\n");
    let output = format!("{first}{second}");
    assert!(output.contains("\x1b[38;2;0;153;255mst0.9"));
}

#[test]
fn cisco_profile_highlights_vlan_svis_without_making_them_juniper_interfaces() {
    let store = ProfileStore::builtin();
    let cisco_config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let cisco_highlighter = Highlighter::from_config(cisco_config).expect("rules compile");
    let cisco_output = cisco_highlighter.highlight_str("Vlan1191    New TZ GW to Internal\n");

    assert!(cisco_output.contains("\x1b[38;2;0;153;255mVlan1191"));

    let juniper_config = PrismConfig::from_profiles(&store, &["juniper"]).expect("juniper loads");
    let juniper_highlighter = Highlighter::from_config(juniper_config).expect("rules compile");
    let juniper_output = juniper_highlighter.highlight_str("Vlan1191    New TZ GW to Internal\n");

    assert!(
        !juniper_output.contains("\x1b[38;2;0;153;255mVlan1191"),
        "{juniper_output:?}"
    );
}

#[test]
fn builtin_mac_addresses_use_magenta_address_color() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");

    let output = highlighter.highlight_str("0 0050.569d.175e ARPA 192.0.2.10\n");

    assert!(
        output.contains("\x1b[38;2;255;154;255m0050.569d.175e\x1b[0m"),
        "{output:?}"
    );
    assert!(
        output.contains("\x1b[38;2;0;255;255m192.0.2.10\x1b[0m"),
        "{output:?}"
    );
}

#[test]
fn style_probe_reports_visible_token_styles_without_ansi_snapshot() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");

    let spans = highlighter.style_spans("Vlan1191 up\n".as_bytes());
    let vlan = spans
        .iter()
        .find(|span| span.text == "Vlan1191")
        .expect("Vlan1191 span exists");
    let up = spans
        .iter()
        .find(|span| span.text == "up")
        .expect("up span exists");

    assert_eq!(
        vlan.style.foreground.map(|rgb| (rgb.r, rgb.g, rgb.b)),
        Some((0, 153, 255))
    );
    assert_eq!(
        up.style.foreground.map(|rgb| (rgb.r, rgb.g, rgb.b)),
        Some((0, 255, 0))
    );
}

#[test]
fn interactive_streaming_highlighter_keeps_split_cisco_vlan_svis_colored() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let first = streaming.push_str("Vlan11");
    let second = streaming.push_str("91    New TZ GW to Internal\n");
    let output = format!("{first}{second}");

    assert!(
        output.contains("\x1b[38;2;0;153;255mVlan1191"),
        "{output:?}"
    );
}

#[test]
fn interactive_streaming_highlighter_uses_minimal_resets_for_highlighted_tokens() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let output = streaming.push_str("Eth1/31 suspended\nVl528 up\n");

    assert!(output.contains("\x1b[38;2;0;153;255mEth1/31"), "{output:?}");
    assert!(output.contains("\x1b[38;2;0;153;255mVl528"), "{output:?}");
    assert!(!output.contains("\x1b[0m"), "{output:?}");
    assert!(!output.contains("\x1b[22m"), "{output:?}");
    assert!(!output.contains(";39m"), "{output:?}");
}

#[test]
fn interactive_streaming_highlighter_does_not_reset_between_colored_space_separated_columns() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["juniper"]).expect("juniper loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let input = "xe-0/0/6.906        up    up   aenet     --> reth6.906\n";
    let output = streaming.push_str(input);

    assert_eq!(strip_ansi(output.as_bytes()), input.as_bytes());
    assert!(
        output.contains("\x1b[38;2;0;153;255mxe-0/0/6.906"),
        "{output:?}"
    );
    assert!(
        output.contains("\x1b[38;2;0;153;255mreth6.906"),
        "{output:?}"
    );
    assert!(
        !output.contains("up\x1b[39m    \x1b[38;2;0;255;0mup"),
        "{output:?}"
    );
    assert!(
        !output.contains("xe-0/0/6.906\x1b[39m        \x1b[38;2;0;255;0mup"),
        "{output:?}"
    );
}

#[test]
fn interactive_streaming_highlighter_resets_overlay_after_trailing_prompt_segment() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["juniper"]).expect("juniper loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let input = "labuser@LAB-WD02>";
    let output = streaming.push_str(input);

    assert_eq!(strip_ansi(output.as_bytes()), input.as_bytes());
    assert!(
        output.contains("\x1b[38;2;0;191;255mlabuser@LAB-WD02>"),
        "{output:?}"
    );
    assert!(output.contains("\x1b[39m"), "{output:?}");
}

#[test]
fn interactive_streaming_highlighter_highlights_juniper_prompt_after_empty_enter_chunk() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["juniper"]).expect("juniper loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let first_prompt = streaming.push_str("labuser@LAB-WD02> ");
    let second_prompt = streaming.push_str("\r\n\r\n{primary:node0}\r\nlabuser@LAB-WD02> ");
    let output = format!("{first_prompt}{second_prompt}");

    assert_eq!(
        strip_ansi(output.as_bytes()),
        b"labuser@LAB-WD02> \r\n\r\n{primary:node0}\r\nlabuser@LAB-WD02> "
    );
    assert_eq!(output.matches("\x1b[38;2;0;191;255mlabuser").count(), 2);
}

#[test]
fn interactive_streaming_highlighter_keeps_juniper_interface_tokens_colored_across_chunk_sizes() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["juniper"]).expect("juniper loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let input = "\
xe-0/0/1.816        up    up   aenet     --> reth1.816
xe-0/0/2.0          up    up   aenet     --> reth2.0
xe-0/0/6.907        up    up   aenet     --> reth6.907
xe-0/0/7.491        up    up   aenet     --> reth7.491
";
    let expected_tokens = [
        "xe-0/0/1.816",
        "reth1.816",
        "xe-0/0/2.0",
        "reth2.0",
        "xe-0/0/6.907",
        "reth6.907",
        "xe-0/0/7.491",
        "reth7.491",
    ];

    for chunk_size in 1..=17 {
        let mut streaming = new_interactive_streaming(highlighter.clone());
        let mut output = String::new();
        for chunk in input.as_bytes().chunks(chunk_size) {
            output.push_str(
                &String::from_utf8(streaming.push(chunk))
                    .expect("ASCII test input remains valid UTF-8"),
            );
        }
        output.push_str(
            &String::from_utf8(streaming.finish()).expect("ASCII test input remains valid UTF-8"),
        );

        assert_eq!(strip_ansi(output.as_bytes()), input.as_bytes());
        for token in expected_tokens {
            assert_all_token_occurrences_have_foreground(&output, token, "38;2;0;153;255");
        }
    }
}

#[test]
fn interactive_streaming_highlighter_does_not_treat_juniper_route_marker_as_prompt() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["juniper"]).expect("juniper loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let first = streaming.push_str("203.0.113.0/24\n>");
    let second = streaming.push_str(" to 192.0.2.1 via st0.1055\n");
    let output = format!("{first}{second}");
    let visible = "203.0.113.0/24\n> to 192.0.2.1 via st0.1055\n";

    assert_eq!(strip_ansi(output.as_bytes()), visible.as_bytes());
    assert!(
        output.contains("\x1b[38;2;0;255;255m192.0.2.1"),
        "{output:?}"
    );
    assert!(
        output.contains("\x1b[38;2;0;153;255mst0.1055"),
        "{output:?}"
    );
}

#[test]
fn interactive_streaming_highlighter_does_not_reemit_same_color_after_newline() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["juniper"]).expect("juniper loads");
    let highlighter =
        Highlighter::from_config_with_color_mode(config, ColorMode::Ansi16).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let input = "xe-0/0/7.409        up    up   aenet     --> reth7.409\nxe-0/0/7.491        up    up   aenet     --> reth7.491\n";
    let output = streaming.push_str(input);

    assert_eq!(strip_ansi(output.as_bytes()), input.as_bytes());
    assert!(
        !output.contains("reth7.409\n\x1b[94mxe-0/0/7.491"),
        "{output:?}"
    );
}

#[test]
fn interactive_streaming_highlighter_does_not_leak_split_ansi16_prefix_as_text() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let first = streaming.push_str("\x1b[96m192");
    let second = streaming.push_str(".0.2.132/31\n");
    let output = format!("{first}{second}");

    assert_eq!(strip_ansi(first.as_bytes()), b"");
    assert_eq!(strip_ansi(second.as_bytes()), b"192.0.2.132/31\n");
    assert_eq!(strip_ansi(output.as_bytes()), b"192.0.2.132/31\n");
}

#[test]
fn interactive_streaming_highlighter_does_not_leak_split_truecolor_prefix_as_text() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let first = streaming.push_str("\x1b[38;2;0;255;255m192");
    let second = streaming.push_str(".0.2.132/31\n");
    let output = format!("{first}{second}");

    assert_eq!(strip_ansi(first.as_bytes()), b"");
    assert_eq!(strip_ansi(second.as_bytes()), b"192.0.2.132/31\n");
    assert_eq!(strip_ansi(output.as_bytes()), b"192.0.2.132/31\n");
}

#[test]
fn interactive_streaming_highlighter_does_not_emit_default_white_restore_between_device_tokens() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let input = "Gi2/13/27   admin down     down\n";
    let output = streaming.push_str(input);

    assert!(
        output.contains("\x1b[38;2;0;153;255mGi2/13/27"),
        "{output:?}"
    );
    assert!(output.contains("\x1b[38;2;255;0;0mdown"), "{output:?}");
    assert_eq!(strip_ansi(output.as_bytes()), input.as_bytes());
    assert!(
        !output.contains("\x1b[38;2;255;255;255m   admin"),
        "{output:?}"
    );
    assert!(!output.contains("\x1b[0m"), "{output:?}");
    assert!(!output.contains("255m   admin"), "{output:?}");
}

#[test]
fn interactive_streaming_highlighter_resets_before_unhighlighted_columns() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["juniper"]).expect("juniper loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let input = "st0.1025        up    up   vpn_MTL-POSTALPRST-I\n";
    let output = streaming.push_str(input);

    assert!(
        output.contains("\x1b[38;2;0;153;255mst0.1025"),
        "{output:?}"
    );
    assert!(output.contains("\x1b[38;2;0;255;0mup"), "{output:?}");
    assert_eq!(strip_ansi(output.as_bytes()), input.as_bytes());
    assert!(
        !output.contains("\x1b[38;2;0;255;0mup   vpn_MTL-POSTALPRST-I"),
        "{output:?}"
    );
    assert!(!output.contains("\x1b[38;2;255;255;255m"), "{output:?}");
}

#[test]
fn interactive_streaming_highlighter_restores_known_source_foreground_across_chunks() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let first = streaming.push_str("\x1b[38;2;200;210;220mpeer ");
    let second = streaming.push_str("192.0.2.1 plain\n");
    let output = format!("{first}{second}");

    assert!(
        output.contains("\x1b[38;2;0;255;255m192.0.2.1 \x1b[38;2;200;210;220mplain"),
        "{output:?}"
    );
    assert!(!output.contains("\x1b[38;2;255;255;255m"), "{output:?}");
    assert!(!output.contains("\x1b[39m"), "{output:?}");
}

#[test]
fn interactive_streaming_highlighter_keeps_arubacx_interface_colored_after_prompt_echo() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["arubacx"]).expect("arubacx loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let mut output = String::new();
    for chunk in [
        "LAB-ARUBA-01# ",
        "s",
        "how interface brief\nInterface  Status  Protocol  Description\n1/1",
        "/1      up      up        Core uplink\n",
    ] {
        output.push_str(&streaming.push_str(chunk));
    }
    output.push_str(
        &String::from_utf8(streaming.finish()).expect("ASCII test input remains valid UTF-8"),
    );

    assert!(output.contains("\x1b[38;2;0;153;255m1/1/1"), "{output:?}");
}

#[test]
fn interactive_streaming_highlighter_does_not_highlight_typed_words_after_prompt() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    assert_eq!(streaming.push_str("router# "), "router# ");
    assert_eq!(streaming.push_str("up"), "up");
    assert_eq!(streaming.push_str(" down"), " down");
    assert_eq!(streaming.push_str("\n"), "\n");
    assert!(streaming.finish().is_empty());
}

#[test]
fn interactive_streaming_highlighter_resets_overlay_after_linux_root_prompt_before_typed_command() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["linux-unix"]).expect("linux profile loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let mut output = String::new();
    output.push_str(&streaming.push_str("root@server:~# "));
    output.push_str(&streaming.push_str("ping 192.0.2.53"));

    assert_eq!(
        strip_ansi(output.as_bytes()),
        b"root@server:~# ping 192.0.2.53"
    );
    assert_all_token_occurrences_have_no_foreground(&output, "ping");
}

#[test]
fn interactive_streaming_highlighter_does_not_highlight_fortinet_typed_diagnose_after_prompt() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["fortinet"]).expect("fortinet loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let mut output = String::new();
    output.push_str(&streaming.push_str("command list\r\nFGT01 # "));
    output.push_str(&streaming.push_str("diagnose"));

    assert_eq!(
        strip_ansi(output.as_bytes()),
        b"command list\r\nFGT01 # diagnose"
    );
    assert_all_token_occurrences_have_no_foreground(&output, "diagnose");
}

#[test]
fn interactive_streaming_highlighter_neutralizes_source_sgr_on_fortinet_prompt_echo() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["fortinet"]).expect("fortinet loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let input = concat!(
        "\r         \ron-demand-sniffer         On-demand sniffer command.\r\n",
        "\r\n \r\n",
        "\x1b[38;2;255;255;255mFGT01 # ",
        "\x1b[38;2;255;0;255mdiagnose "
    );
    let output = streaming.push_str(input);

    assert_eq!(strip_ansi(output.as_bytes()), strip_ansi(input.as_bytes()));
    assert_all_token_occurrences_have_no_foreground(&output, "diagnose");
}

#[test]
fn interactive_streaming_highlighter_uses_full_reset_when_neutralizing_prompt_echo_sgr() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["fortinet"]).expect("fortinet loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = StreamingHighlighter::new_interactive(highlighter);
    streaming.set_no_minimal_resets(true);

    let input = concat!(
        "\r         \ron-demand-sniffer         On-demand sniffer command.\r\n",
        "\r\n \r\n",
        "\x1b[38;2;255;255;255mFGT01 # ",
        "\x1b[38;2;255;0;255mdiagnose "
    );
    let output = streaming.push_str(input);

    let diagnose_idx = output.find("diagnose").expect("typed command is present");
    let before_diagnose = &output[..diagnose_idx];
    assert_eq!(strip_ansi(output.as_bytes()), strip_ansi(input.as_bytes()));
    assert!(
        before_diagnose.contains("\x1b[0m"),
        "expected full reset before typed command: {output:?}"
    );
    assert!(
        !before_diagnose.contains("\x1b[39m"),
        "minimal foreground reset should not be used with --no-minimal-reset: {output:?}"
    );
}

#[test]
fn interactive_streaming_highlighter_still_highlights_complete_chunks() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let output = streaming.push_str("198.51.100.7 down\n");

    assert!(output.contains("\x1b[38;2;0;255;255m198.51.100.7"));
    assert!(output.contains("\x1b[38;2;255;0;0mdown"));
    assert!(!output.contains("\x1b[1;38;2;255;0;0mdown"));
    assert!(!output.contains("\x1b[0m"), "{output:?}");
}

#[test]
fn interactive_streaming_highlighter_resets_active_style_on_finish() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    let mut output = streaming.push_str("198.51.100.7 down\n");
    output.push_str(&String::from_utf8(streaming.finish()).expect("finish output is UTF-8"));

    assert_eq!(strip_ansi(output.as_bytes()), b"198.51.100.7 down\n");
    assert!(output.contains("\x1b[38;2;255;0;0mdown"));
    assert!(
        output.ends_with("\x1b[39m") || output.ends_with("\x1b[0m"),
        "interactive stream should not exit with active style: {output:?}"
    );
}

#[test]
fn interactive_streaming_highlighter_finish_is_empty_when_no_style_is_active() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    // Prompt echo plus typed text that matches no rule never activates an
    // interactive overlay style, so there is nothing for finish() to reset.
    let mut output = streaming.push_str("router# ");
    output.push_str(&streaming.push_str("show"));

    assert!(
        !output.contains('\x1b'),
        "precondition: no interactive style should be emitted: {output:?}"
    );
    assert!(
        streaming.finish().is_empty(),
        "interactive finish() must emit nothing when no style is active"
    );
}

#[test]
fn noninteractive_streaming_highlighter_finish_does_not_append_interactive_reset() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = StreamingHighlighter::new(highlighter);

    // Noninteractive highlighting terminates each styled span with its own
    // reset, so a complete line leaves nothing buffered: the interactive
    // end-of-input cleanup reset must not leak into this mode.
    let output = streaming.push_str("198.51.100.7 down\n");

    assert!(output.contains("\x1b[1;38;2;255;0;0mdown"));
    assert!(
        streaming.finish().is_empty(),
        "noninteractive finish() must not append an interactive cleanup reset"
    );
}

#[test]
fn linux_unix_profile_does_not_highlight_clock_times_as_ports() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["linux-unix"]).expect("linux profile loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");

    let output = highlighter.highlight_str("at Wednesday 2026-05-06 10:07:20 PM, then 12:34\n");

    assert!(output.contains("10:07:20 PM"));
    assert!(output.contains("12:34"));
    assert!(!output.contains("\x1b[38;2;0;255;192m:34"));
}

#[test]
fn linux_unix_profile_still_highlights_real_ports() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["linux-unix"]).expect("linux profile loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");

    let output = highlighter.highlight_str("nginx tcp/443 localhost:8443 192.0.2.1:22\n");

    assert!(output.contains("\x1b[38;2;0;255;192mtcp/443\x1b[0m"));
    assert!(output.contains("localhost\x1b[38;2;0;255;192m:8443\x1b[0m"));
    assert!(output.contains("\x1b[38;2;0;255;192m:22\x1b[0m"));
}

#[test]
fn streaming_highlighter_flushes_complete_prompts_without_waiting_for_newline() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["juniper"]).expect("juniper loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = StreamingHighlighter::new(highlighter);

    let output = streaming.push_str("labuser@LAB-MX-01>");

    assert!(output.contains("LAB-MX-01>"));
}

#[test]
fn invalid_regex_errors_include_rule_description() {
    let yaml = r##"
rules:
  - description: broken user rule
    regex: "(["
    color: f#ff0000
"##;

    let config = PrismConfig::from_chromaterm_yaml(yaml).expect("yaml parses");
    let error = Highlighter::from_config(config).expect_err("regex should fail");

    assert!(error.to_string().contains("broken user rule"));
}

#[test]
fn every_builtin_profile_highlights_a_representative_fixture() {
    let store = ProfileStore::builtin();
    let cases = [
        ("generic", "192.0.2.10 down\n"),
        ("juniper", "admin@mx480> show interfaces ge-0/0/0 up\n"),
        ("cisco", "Router# show interface Gi0/1 down\n"),
        ("versa", "versa branch appliance vni-10 down\n"),
        (
            "arista",
            "leaf1# show interfaces Ethernet1 up mlag active\n",
        ),
        ("arubacx", "LAB-ARUBA-01# show interface 1/1/1 up\n"),
        ("fortinet", "FGT # diagnose vpn tunnel phase1 down\n"),
        ("palo-alto", "admin@PA-VM> show system info vsys1 active\n"),
        (
            "linux-unix",
            "root@server:~# systemctl status sshd failed\n",
        ),
    ];

    for (profile, sample) in cases {
        let config =
            PrismConfig::from_profiles(&store, &[profile]).expect("built-in profile loads");
        let highlighter = Highlighter::from_config(config).expect("profile compiles");
        let output = highlighter.highlight_str(sample);
        assert!(
            output.contains("\x1b["),
            "profile {profile} did not highlight sample: {output:?}"
        );
    }
}

fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || needle.len() > haystack.len() {
        return 0;
    }
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

fn assert_all_token_occurrences_have_foreground(output: &str, token: &str, expected: &str) {
    let (visible, foregrounds) = visible_bytes_and_foregrounds(output.as_bytes());
    let visible_text = String::from_utf8(visible).expect("test output remains UTF-8");
    let mut found = 0;

    for (idx, _) in visible_text.match_indices(token) {
        found += 1;
        assert_eq!(
            foregrounds.get(idx).and_then(Clone::clone).as_deref(),
            Some(expected),
            "token {token:?} at {idx} did not have foreground {expected:?} in {output:?}"
        );
    }

    assert!(found > 0, "token {token:?} was not present in {output:?}");
}

fn assert_all_token_occurrences_have_no_foreground(output: &str, token: &str) {
    let (visible, foregrounds) = visible_bytes_and_foregrounds(output.as_bytes());
    let visible_text = String::from_utf8(visible).expect("test output remains UTF-8");
    let mut found = 0;

    for (idx, _) in visible_text.match_indices(token) {
        found += 1;
        assert_eq!(
            foregrounds.get(idx).and_then(Clone::clone),
            None,
            "token {token:?} at {idx} unexpectedly had a foreground in {output:?}"
        );
    }

    assert!(found > 0, "token {token:?} was not present in {output:?}");
}

fn visible_bytes_and_foregrounds(input: &[u8]) -> (Vec<u8>, Vec<Option<String>>) {
    let mut visible = Vec::new();
    let mut foregrounds = Vec::new();
    let mut foreground = None;
    let mut idx = 0;

    while idx < input.len() {
        if input[idx] == 0x1b {
            let end = ansi_sequence_end(input, idx);
            apply_test_sgr_foreground(&input[idx..end], &mut foreground);
            idx = end;
        } else {
            visible.push(input[idx]);
            foregrounds.push(foreground.clone());
            idx += 1;
        }
    }

    (visible, foregrounds)
}

fn apply_test_sgr_foreground(sequence: &[u8], foreground: &mut Option<String>) {
    if !sequence.starts_with(b"\x1b[") || sequence.last() != Some(&b'm') {
        return;
    }

    let params = String::from_utf8_lossy(&sequence[2..sequence.len() - 1]);
    let codes = if params.is_empty() {
        vec![0]
    } else {
        params
            .split(';')
            .filter_map(|part| part.parse::<u16>().ok())
            .collect::<Vec<_>>()
    };
    let mut idx = 0;
    while idx < codes.len() {
        match codes[idx] {
            0 | 39 => *foreground = None,
            30..=37 | 90..=97 => *foreground = Some(codes[idx].to_string()),
            38 if idx + 2 < codes.len() && codes[idx + 1] == 5 => {
                *foreground = Some(format!("38;5;{}", codes[idx + 2]));
                idx += 2;
            }
            38 if idx + 4 < codes.len() && codes[idx + 1] == 2 => {
                *foreground = Some(format!(
                    "38;2;{};{};{}",
                    codes[idx + 2],
                    codes[idx + 3],
                    codes[idx + 4]
                ));
                idx += 4;
            }
            _ => {}
        }
        idx += 1;
    }
}

fn ansi_sequence_end(input: &[u8], start: usize) -> usize {
    if start + 1 >= input.len() || input[start + 1] != b'[' {
        return (start + 2).min(input.len());
    }

    let mut idx = start + 2;
    while idx < input.len() {
        let byte = input[idx];
        idx += 1;
        if (0x40..=0x7e).contains(&byte) {
            break;
        }
    }
    idx
}

#[test]
fn interactive_streaming_highlighter_preserves_prompt_state_on_profile_rebuild() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = new_interactive_streaming(highlighter);

    // 1. Prime the prompt to enable prompt_echo_passthrough
    assert_eq!(streaming.push_str("router# "), "router# ");

    // 2. Replace the highlighter (simulating rebuild/profile reload)
    let new_config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let new_highlighter = Highlighter::from_config(new_config).expect("rules compile");
    streaming.replace_highlighter(new_highlighter);

    // 3. Type characters one by one. If state was preserved, they are not buffered.
    assert_eq!(streaming.push_str("s"), "s");
    assert_eq!(streaming.push_str("h"), "h");
    assert_eq!(streaming.push_str("o"), "o");
    assert_eq!(streaming.push_str("w"), "w");
    assert!(streaming.finish().is_empty());
}

// AUDIT H1: real syslog tags (preceded by SOL/space/colon) must be highlighted.
#[test]
fn generic_syslog_severity_tags_are_highlighted() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let output = highlighter
        .highlight_str("%LINK-3-UPDOWN: changed\n%SYS-5-CONFIG_I done\n%OSPF-6-ADJCHG seen\n");

    assert!(
        output.contains("\x1b[1;38;2;255;51;51m%LINK-3-UPDOWN\x1b[0m"),
        "syslog severe tag not highlighted: {output:?}"
    );
    assert!(
        output.contains("\x1b[38;2;255;255;0m%SYS-5-CONFIG_I\x1b[0m"),
        "syslog warning tag not highlighted: {output:?}"
    );
    assert!(
        output.contains("\x1b[38;2;101;215;253m%OSPF-6-ADJCHG\x1b[0m"),
        "syslog info tag not highlighted: {output:?}"
    );
}

// AUDIT M9: BGP transient/fault states (Active, Connect) must keep the Versa
// blue and not be repainted green by the inherited generic good-state rule.
#[test]
fn versa_bgp_transient_states_stay_blue_over_generic_good_state() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["versa"]).expect("versa loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let output = highlighter.highlight_str("Peer1 Active\nPeer2 Connect\n");

    assert!(
        output.contains("\x1b[1;38;2;77;166;255mActive\x1b[0m"),
        "versa BGP 'Active' should stay blue, not generic green: {output:?}"
    );
    assert!(
        output.contains("\x1b[1;38;2;77;166;255mConnect\x1b[0m"),
        "versa BGP 'Connect' should stay blue, not generic green: {output:?}"
    );
}

// AUDIT M11: systemd states must be colored by health, not all green.
#[test]
fn linux_systemd_states_are_colored_by_health() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["linux-unix"]).expect("linux-unix loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let output = highlighter.highlight_str("svc-a active\nsvc-b dead\nsvc-c masked\n");

    assert!(
        output.contains("\x1b[38;2;0;255;0mactive\x1b[0m"),
        "healthy 'active' should stay green: {output:?}"
    );
    assert!(
        output.contains("\x1b[1;38;2;255;0;0mdead\x1b[0m"),
        "'dead' should be red, not green: {output:?}"
    );
    assert!(
        output.contains("\x1b[38;2;255;255;0mmasked\x1b[0m"),
        "'masked' should be yellow, not green: {output:?}"
    );
}

// AUDIT M12: log priority levels must be colored by severity; info/notice/debug
// are informational (not warnings) and severe levels are red.
#[test]
fn linux_log_priority_levels_are_colored_by_severity() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["linux-unix"]).expect("linux-unix loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let output = highlighter.highlight_str("level info here\nlevel warn here\nlevel emerg here\n");

    assert!(
        output.contains("\x1b[38;2;101;215;253minfo\x1b[0m"),
        "'info' should be info-cyan, not warning-yellow: {output:?}"
    );
    assert!(
        output.contains("\x1b[38;2;255;255;0mwarn\x1b[0m"),
        "'warn' should stay yellow: {output:?}"
    );
    assert!(
        output.contains("\x1b[1;38;2;255;0;0memerg\x1b[0m"),
        "'emerg' should be severe-red: {output:?}"
    );
}

// AUDIT M4: misspelled/unknown keys in user config must be rejected, not
// silently dropped.
#[test]
fn unknown_top_level_config_key_is_rejected() {
    let yaml = "rule:\n  - regex: foo\n    color: f#ff0000\n";
    assert!(
        PrismConfig::from_chromaterm_yaml(yaml).is_err(),
        "misspelled top-level key 'rule' (vs 'rules') should be rejected"
    );
}

#[test]
fn unknown_rule_field_is_rejected() {
    let yaml = "rules:\n  - regex: foo\n    color: f#ff0000\n    colour: red\n";
    assert!(
        PrismConfig::from_chromaterm_yaml(yaml).is_err(),
        "misspelled rule field 'colour' (vs 'color') should be rejected"
    );
}

// AUDIT L5: Cisco BGP transient states must stay blue (same class as Versa M9).
#[test]
fn cisco_bgp_transient_states_stay_blue_over_generic_good_state() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let output = highlighter.highlight_str("Neighbor1 Active\nNeighbor2 Connect\n");

    assert!(
        output.contains("\x1b[1;38;2;77;166;255mActive\x1b[0m"),
        "cisco BGP 'Active' should stay blue, not generic green: {output:?}"
    );
    assert!(
        output.contains("\x1b[1;38;2;77;166;255mConnect\x1b[0m"),
        "cisco BGP 'Connect' should stay blue, not generic green: {output:?}"
    );
}

// AUDIT M1: a byte-mode match boundary inside a multibyte char must not make
// highlight_str panic; style spans snap to UTF-8 char boundaries.
#[test]
fn highlight_str_does_not_panic_on_match_boundary_inside_multibyte_char() {
    let config =
        PrismConfig::from_chromaterm_yaml("rules:\n  - regex: 'CPU: .'\n    color: f#ff0000\n")
            .expect("config loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let output = highlighter.highlight_str("CPU: \u{2501} load\n");
    assert_eq!(
        strip_ansi(output.as_bytes()).as_slice(),
        "CPU: \u{2501} load\n".as_bytes(),
        "visible text must round-trip"
    );
}

// AUDIT M2: the CLI byte path must keep valid UTF-8 even when a match boundary
// lands mid-codepoint.
#[test]
fn highlight_bytes_keeps_valid_utf8_on_mid_codepoint_match_boundary() {
    let config =
        PrismConfig::from_chromaterm_yaml("rules:\n  - regex: 'load: .'\n    color: f#0099ff\n")
            .expect("config loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let output = highlighter.highlight_bytes("load: \u{2501} ok\n".as_bytes());
    assert!(
        std::str::from_utf8(&output).is_ok(),
        "highlighted output must remain valid UTF-8: {output:?}"
    );
}

// AUDIT G2: engine robustness sweep — feeding diverse multibyte/combining
// inputs through the byte highlighter (with rules whose matches can end
// mid-codepoint) must never produce invalid UTF-8 and must round-trip under
// strip_ansi, regardless of where byte-mode match boundaries fall.
#[test]
fn highlight_bytes_never_corrupts_valid_utf8_across_generated_inputs() {
    // 'x.' ends a match one byte into whatever follows 'x'; '.$' matches the
    // single last byte before a newline. Both land mid-codepoint when that byte
    // belongs to a multibyte glyph, and nothing else re-colors the whole glyph.
    let config = PrismConfig::from_chromaterm_yaml(concat!(
        "rules:\n",
        "  - regex: 'x.'\n    color: f#ff0000\n",
        "  - regex: '.$'\n    color: f#00ff00\n",
    ))
    .expect("config loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");

    let glyphs = ["é", "\u{2501}", "🌐", "中", "a\u{0301}"];
    for glyph in glyphs {
        let inputs = [
            format!("x{glyph} ok\n"),
            format!("a x{glyph}b\n"),
            format!("value {glyph}\n"),
            format!("{glyph}\n"),
            format!("{glyph}{glyph}\n"),
        ];
        for input in inputs {
            let output = highlighter.highlight_bytes(input.as_bytes());
            assert!(
                std::str::from_utf8(&output).is_ok(),
                "non-UTF8 output for input {input:?}: {output:?}"
            );
            assert_eq!(
                strip_ansi(&output).as_slice(),
                input.as_bytes(),
                "strip_ansi(output) != input for {input:?}"
            );
        }
    }
}

// AUDIT M8: the Cisco interface rule must require a number, so bare
// abbreviations and English words are not painted as interfaces.
#[test]
fn cisco_interface_rule_requires_a_number() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let h = Highlighter::from_config(config).expect("rules compile");

    let real = h.highlight_str("Gi1/0/1 and Po10\n");
    assert!(
        real.contains("\x1b[38;2;0;153;255mGi1/0/1\x1b[0m"),
        "Gi1/0/1 should still highlight: {real:?}"
    );
    assert!(
        real.contains("\x1b[38;2;0;153;255mPo10\x1b[0m"),
        "Po10 should still highlight: {real:?}"
    );

    let prose = h.highlight_str("serial loopback Te done\n");
    for word in ["serial", "loopback", "Te"] {
        assert!(
            !prose.contains(&format!("\x1b[38;2;0;153;255m{word}")),
            "{word} wrongly highlighted as an interface: {prose:?}"
        );
    }
}

// AUDIT L1: the Juniper interface rule must require a unit/number, so plain
// words are not painted as interfaces.
#[test]
fn juniper_interface_rule_does_not_match_plain_words() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["juniper"]).expect("juniper loads");
    let h = Highlighter::from_config(config).expect("rules compile");

    let real = h.highlight_str("xe-7/0/2 reth1.808 vlan.100 ae0\n");
    for iface in ["xe-7/0/2", "reth1.808", "vlan.100", "ae0"] {
        assert!(
            real.contains(&format!("\x1b[38;2;0;153;255m{iface}\x1b[0m")),
            "{iface} should still highlight: {real:?}"
        );
    }

    let prose = h.highlight_str("set a tap on the gre vlan irb and ae reth\n");
    for word in ["tap", "gre", "vlan", "irb", "ae", "reth"] {
        assert!(
            !prose.contains(&format!("\x1b[38;2;0;153;255m{word}\x1b[0m")),
            "{word} wrongly highlighted as an interface: {prose:?}"
        );
    }
}

// AUDIT L11: ArubaCX platform terms must not match the bare English word
// 'event'.
#[test]
fn arubacx_platform_terms_skip_bare_event_word() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["arubacx"]).expect("arubacx loads");
    let h = Highlighter::from_config(config).expect("rules compile");

    assert!(
        h.highlight_str("status vsx\n")
            .contains("\x1b[38;2;255;0;255mvsx\x1b[0m"),
        "vsx should stay highlighted"
    );
    assert!(
        !h.highlight_str("an event happened\n")
            .contains("\x1b[38;2;255;0;255mevent"),
        "bare 'event' must not be colored as a platform term"
    );
}

// AUDIT L7: the Versa prompt must highlight realistic prompts whose hostname is
// not literally 'versa'/'voss'.
#[test]
fn versa_prompt_highlights_arbitrary_hostnames() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["versa"]).expect("versa loads");
    let h = Highlighter::from_config(config).expect("rules compile");

    let out = h.highlight_str("admin@edge-01> show interfaces\n");
    assert!(
        out.contains("\x1b[38;2;0;191;255madmin@edge-01>"),
        "versa prompt with an arbitrary hostname was not highlighted: {out:?}"
    );
}

// AUDIT L4: the Cisco prompt rule must not paint prose like `issue#42`, while
// still matching real prompts (with or without an immediately typed command).
#[test]
fn cisco_prompt_rule_ignores_prose_with_numeric_suffix() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["cisco"]).expect("cisco loads");
    let h = Highlighter::from_config(config).expect("rules compile");

    for line in ["Router#\n", "Router#show ip\n", "Switch(config)#\n"] {
        let out = h.highlight_str(line);
        assert!(
            out.contains("\x1b[38;2;255;255;255m"),
            "real prompt not highlighted for {line:?}: {out:?}"
        );
    }
    for line in ["issue#42\n", "item#1\n"] {
        let out = h.highlight_str(line);
        assert!(
            !out.contains("\x1b[38;2;255;255;255m"),
            "prose wrongly highlighted as a prompt for {line:?}: {out:?}"
        );
    }
}

// An empty profile name must be rejected, not registered under "".
#[test]
fn empty_profile_name_is_rejected() {
    let empty = "profile:\n  name: \"\"\n  inherits: [generic]\nrules: []\n";
    assert!(
        prismtty::config::parse_profile_yaml(empty).is_err(),
        "empty profile name should be rejected"
    );
    let valid = "profile:\n  name: edge\n  inherits: [generic]\nrules: []\n";
    assert!(
        prismtty::config::parse_profile_yaml(valid).is_ok(),
        "a valid profile should still parse"
    );
}

// A multibyte UTF-8 codepoint split across two reads must be buffered, not
// flushed mid-codepoint (which would splice a reset escape into it and emit
// invalid UTF-8).
#[test]
fn streaming_buffers_multibyte_codepoint_split_across_reads() {
    let config =
        PrismConfig::from_chromaterm_yaml("rules:\n  - regex: '.+'\n    color: f#ff0000\n")
            .expect("config loads");
    let highlighter = Highlighter::from_config(config).expect("rules compile");
    let mut streaming = StreamingHighlighter::new(highlighter);

    let mut out = Vec::new();
    out.extend(streaming.push(b"a\xe2")); // chunk ends on the lead byte of '━'
    out.extend(streaming.push(b"\x94\x81b\n")); // its continuation bytes
    out.extend(streaming.finish());

    assert!(
        std::str::from_utf8(&out).is_ok(),
        "streamed output must stay valid UTF-8: {out:?}"
    );
    assert_eq!(strip_ansi(&out).as_slice(), "a\u{2501}b\n".as_bytes());
}

// AUDIT L12: IPv6 addresses (full, compressed, and with a prefix) must be
// highlighted, like IPv4.
#[test]
fn generic_highlights_ipv6_addresses() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic"]).expect("generic loads");
    let h = Highlighter::from_config(config).expect("rules compile");

    let out = h.highlight_str("link-local fe80::1 site 2001:db8::1 loop ::1/128\n");
    for addr in ["fe80::1", "2001:db8::1", "::1/128"] {
        assert!(
            out.contains(&format!("\x1b[38;2;0;255;255m{addr}\x1b[0m")),
            "IPv6 {addr} not highlighted: {out:?}"
        );
    }
}

// AUDIT L8: FortiGate interface names (port1, wan2, mgmt, …) must be
// highlighted; the profile previously had no interface rule.
#[test]
fn fortinet_highlights_interface_names() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["fortinet"]).expect("fortinet loads");
    let h = Highlighter::from_config(config).expect("rules compile");

    let out = h.highlight_str("traffic from port1 to wan2 over mgmt\n");
    for iface in ["port1", "wan2", "mgmt"] {
        assert!(
            out.contains(&format!("\x1b[38;2;0;153;255m{iface}\x1b[0m")),
            "fortinet interface {iface} not highlighted: {out:?}"
        );
    }
}

// AUDIT (Versa object over-match, found during L7): distinctive Versa tokens
// still paint, but common English words must not.
#[test]
fn versa_object_rule_does_not_match_plain_english_words() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["versa"]).expect("versa loads");
    let h = Highlighter::from_config(config).expect("rules compile");

    let out = h.highlight_str("vni-5 over sdwan fabric\n");
    for term in ["vni-5", "sdwan"] {
        assert!(
            out.contains(&format!("\x1b[38;2;255;0;255m{term}\x1b[0m")),
            "versa object {term} not highlighted: {out:?}"
        );
    }

    let prose =
        h.highlight_str("the branch office org chart tenant agreement controller appliance\n");
    for word in ["branch", "org", "tenant", "controller", "appliance"] {
        assert!(
            !prose.contains(&format!("\x1b[38;2;255;0;255m{word}")),
            "English word {word} wrongly highlighted as a Versa object: {prose:?}"
        );
    }
}

#[test]
fn fortinet_terms_skip_bare_ha_word() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["fortinet"]).expect("fortinet loads");
    let h = Highlighter::from_config(config).expect("rules compile");

    // A distinctive term still highlights (magenta #ff00ff).
    assert!(
        h.highlight_str("config vdom edit\n")
            .contains("\x1b[38;2;255;0;255mvdom\x1b[0m"),
        "vdom should still highlight"
    );
    // Bare 'ha' must not be colored.
    assert!(
        !h.highlight_str("set the ha mode now\n")
            .contains("\x1b[38;2;255;0;255mha"),
        "bare 'ha' must not be highlighted as a Fortinet term"
    );
}

#[test]
fn example_custom_router_ha_role_requires_context() {
    use prismtty::config::load_profile_file;

    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/profiles/custom-router.example.yml"
    );
    let loaded = load_profile_file(path).expect("example profile loads");
    let mut store = ProfileStore::builtin();
    store.insert_profile(
        loaded.meta.name.clone(),
        loaded.meta.inherits.clone(),
        loaded.meta.detection.clone(),
        loaded.rules.clone(),
    );
    let config = PrismConfig::from_profiles(&store, &[loaded.meta.name.as_str()])
        .expect("example profile compiles");
    let h = Highlighter::from_config(config).expect("rules compile");

    const YELLOW: &str = "\x1b[38;2;255;255;0m";
    // A bare role word in unrelated prose must not light up.
    assert!(
        !h.highlight_str("the primary copy was kept\n")
            .contains(YELLOW),
        "bare 'primary' must not be highlighted as an HA role"
    );
    // The same word in its HA context is highlighted.
    assert!(
        h.highlight_str("HA role: primary\n").contains(YELLOW),
        "an HA-role status line should highlight"
    );
}
