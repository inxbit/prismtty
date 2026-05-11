use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use nix::libc;

pub(super) struct RuntimeRegistration {
    pid: u32,
    path: PathBuf,
}

impl RuntimeRegistration {
    pub(super) fn register() -> io::Result<Self> {
        let dir = runtime_dir();
        fs::create_dir_all(&dir)?;
        let path = pid_registry_path();
        let pid = std::process::id();
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(file, "{pid}")?;
        Ok(Self { pid, path })
    }
}

impl Drop for RuntimeRegistration {
    fn drop(&mut self) {
        let Ok(input) = fs::read_to_string(&self.path) else {
            return;
        };
        let retained = input
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .filter(|pid| *pid != self.pid && process_is_alive(*pid))
            .map(|pid| pid.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let output = if retained.is_empty() {
            String::new()
        } else {
            format!("{retained}\n")
        };
        let _ = fs::write(&self.path, output);
    }
}

pub(super) struct ReloadWatcher {
    marker: PathBuf,
    last_seen: Option<SystemTime>,
}

impl ReloadWatcher {
    pub(super) fn new() -> Self {
        let marker = reload_marker_path();
        let last_seen = reload_marker_time(&marker);
        Self { marker, last_seen }
    }

    pub(super) fn reload_requested(&mut self) -> bool {
        let current = reload_marker_time(&self.marker);
        if current.is_some() && current != self.last_seen {
            self.last_seen = current;
            return true;
        }
        false
    }
}

pub(super) fn request_reload() -> io::Result<usize> {
    let dir = runtime_dir();
    fs::create_dir_all(&dir)?;
    fs::write(reload_marker_path(), format!("{:?}\n", SystemTime::now()))?;

    let path = pid_registry_path();
    let input = fs::read_to_string(&path).unwrap_or_default();
    let mut count = 0usize;
    let mut retained = Vec::new();
    for pid in input
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
    {
        if process_is_alive(pid) {
            count += 1;
            retained.push(pid.to_string());
        }
    }

    let output = if retained.is_empty() {
        String::new()
    } else {
        format!("{}\n", retained.join("\n"))
    };
    fs::write(path, output)?;
    Ok(count)
}

fn runtime_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("PRISMTTY_RUNTIME_DIR") {
        return PathBuf::from(path);
    }
    std::env::temp_dir().join(format!("prismtty-{}", current_uid()))
}

fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

fn pid_registry_path() -> PathBuf {
    runtime_dir().join("pids")
}

fn reload_marker_path() -> PathBuf {
    runtime_dir().join("reload")
}

fn reload_marker_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}
