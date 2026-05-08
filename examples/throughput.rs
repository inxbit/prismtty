use prismtty::{Highlighter, PrismConfig, ProfileStore};
use std::time::Instant;

fn main() {
    let store = ProfileStore::builtin();
    let config = PrismConfig::from_profiles(&store, &["generic", "cisco", "juniper", "linux-unix"])
        .expect("built-in profiles load");
    let highlighter = Highlighter::from_config(config).expect("built-in profiles compile");

    for (name, sample) in [
        ("router dump", router_dump_sample()),
        ("syslog", syslog_sample()),
        ("mixed ansi", mixed_ansi_sample()),
    ] {
        let started = Instant::now();
        let highlighted = highlighter.highlight_str(&sample);
        let elapsed = started.elapsed();

        let mib = sample.len() as f64 / 1024.0 / 1024.0;
        let mib_per_sec = mib / elapsed.as_secs_f64();
        println!(
            "{name}: highlighted {:.2} MiB into {:.2} MiB in {:.3}s ({:.2} MiB/s)",
            mib,
            highlighted.len() as f64 / 1024.0 / 1024.0,
            elapsed.as_secs_f64(),
            mib_per_sec
        );
    }
}

fn router_dump_sample() -> String {
    let line =
        "Gi0/1 is up, line protocol is down 192.0.2.1 ge-0/0/0 FULL root@server:~# error failed\n";
    line.repeat(100_000)
}

fn syslog_sample() -> String {
    let line = "2026-05-06T10:29:58Z router %LINK-3-UPDOWN: Interface GigabitEthernet0/1 changed state to down from 198.51.100.7\n";
    line.repeat(100_000)
}

fn mixed_ansi_sample() -> String {
    let line = "\x1b[32madmin@mx480>\x1b[0m show route 203.0.113.0/24 active accepted ge-0/0/0.0\n";
    line.repeat(100_000)
}
