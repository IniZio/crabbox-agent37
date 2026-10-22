//! Mirrors the tunnel's diagnostic lines into a file named by an env var.
//!
//! ssh discards the stderr of a backgrounded `ProxyCommand`, so the file is the
//! only place a dropped tunnel's cause survives.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{SystemTime, UNIX_EPOCH};

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
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
    timestamp(millis)
}

/// Formats milliseconds since the Unix epoch as UTC, with the civil-from-days
/// conversion of Howard Hinnant's date algorithms.
fn timestamp(millis: u64) -> String {
    let secs = millis / 1000;
    let (days, rest) = (secs / 86_400, secs % 86_400);
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60,
        millis % 1000,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known instants format to their UTC calendar time.
    #[test]
    fn formats_utc_milliseconds() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(timestamp(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(timestamp(1_709_251_199_999), "2024-02-29T23:59:59.999Z");
    }
}
