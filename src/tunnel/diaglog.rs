//! Mirrors the tunnel's diagnostic lines into a file named by an env var.
//!
//! ssh discards the stderr of a backgrounded `ProxyCommand`, so the file is the
//! only place a dropped tunnel's cause survives.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

use time::macros::format_description;
use time::OffsetDateTime;

/// Writes to `stderr` as given, and each complete line to the log file with a
/// UTC timestamp, the process id and the target host in front.
pub(super) struct DiagLog<'a> {
    /// The unchanged diagnostic stream.
    stderr: &'a mut dyn Write,
    /// The append-only log, or `None` when not requested or after a write failed.
    file: Option<File>,
    /// `pid=<n> host=<host> `, put after the timestamp of every line.
    context: String,
    /// Bytes of the line being written, until its newline arrives.
    pending: Vec<u8>,
}

impl<'a> DiagLog<'a> {
    /// Opens `path` for appending (created 0600) when given. A failure to open
    /// writes one warning to `stderr` and leaves the log off.
    pub(super) fn open(stderr: &'a mut dyn Write, path: Option<&str>, host: &str) -> Self {
        let file = path.and_then(|path| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(path)
                .map_err(|e| {
                    let _ = writeln!(
                        stderr,
                        "crabbox-agent37 tunnel: tunnel log {path} not written: {e}"
                    );
                })
                .ok()
        });
        let host: String = host
            .chars()
            .map(|c| if c.is_control() || c == ' ' { '?' } else { c })
            .collect();
        Self {
            stderr,
            file,
            context: format!("pid={} host={host} ", std::process::id()),
            pending: Vec::new(),
        }
    }

    /// Appends one finished line with its prefix in a single write, so
    /// concurrent tunnels interleave whole lines.
    fn append(&mut self, line: &[u8]) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let mut entry = format!("{} {}", timestamp_now(), self.context).into_bytes();
        entry.extend_from_slice(line);
        if file.write_all(&entry).is_err() {
            self.file = None;
        }
    }
}

impl Write for DiagLog<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.stderr.write_all(buf);
        if self.file.is_some() {
            self.pending.extend_from_slice(buf);
            while let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=end).collect();
                self.append(&line);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stderr.flush()
    }
}

/// The current UTC time as `2026-10-22T09:15:30.123Z`.
fn timestamp_now() -> String {
    OffsetDateTime::now_utc()
        .format(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        ))
        .unwrap_or_default()
}
