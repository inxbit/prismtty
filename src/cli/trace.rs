use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(super) struct IoTrace {
    file: Option<Arc<Mutex<File>>>,
}

impl IoTrace {
    pub(super) fn open(path: Option<&Path>) -> io::Result<Self> {
        let file = match path {
            Some(path) => Some(Arc::new(Mutex::new(
                OpenOptions::new().create(true).append(true).open(path)?,
            ))),
            None => None,
        };
        Ok(Self { file })
    }

    pub(super) fn log(&self, direction: &str, bytes: &[u8]) {
        let Some(file) = &self.file else {
            return;
        };
        let Ok(mut file) = file.lock() else {
            return;
        };
        let _ = writeln!(file, "{direction} {}", trace_hex(bytes));
    }
}

fn trace_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().saturating_mul(3).saturating_sub(1));
    for (idx, byte) in bytes.iter().enumerate() {
        if idx > 0 {
            output.push(' ');
        }
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

#[cfg(test)]
mod tests {
    #[test]
    fn trace_hex_encodes_bytes_for_diagnostics() {
        assert_eq!(super::trace_hex(b"echo\r\n"), "65 63 68 6f 0d 0a");
    }
}
