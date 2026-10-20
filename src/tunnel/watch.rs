//! Stall and round-trip diagnostics for a running tunnel, written to stderr.
//!
//! Every line starts with `crabbox-agent37 tunnel:` and carries `key=value`
//! fields: counts and durations only, never payload bytes or the key.

use std::io::Write;
use std::time::{Duration, Instant};

/// Silence from the gateway longer than this is reported as a stall.
const STALL: Duration = Duration::from_secs(5);
/// Interval between WebSocket pings.
const PING_EVERY: Duration = Duration::from_secs(5);
/// A ping unanswered this long is reported.
const PING_UNANSWERED: Duration = Duration::from_secs(5);
/// A pong slower than this is reported.
const SLOW_RTT: Duration = Duration::from_secs(1);

/// A ping awaiting its pong.
struct Outstanding {
    /// Sequence number carried as the ping payload.
    seq: u64,
    /// When it was sent.
    at: Instant,
    /// Whether the unanswered line was already written.
    reported: bool,
}

/// Byte counts, gateway silence and ping state of one tunnel.
pub(super) struct Watch {
    /// When the tunnel opened.
    started: Instant,
    /// Bytes sent to the gateway.
    sent: u64,
    /// Bytes received from the gateway.
    received: u64,
    /// When gateway data last arrived, or the tunnel opened.
    last_data: Instant,
    /// Longest silence between gateway data so far.
    longest_wait: Duration,
    /// Whether a stall line was written and its resume line is pending.
    stalled: bool,
    /// When the next ping is due.
    next_ping: Instant,
    /// Sequence number of the last ping sent.
    seq: u64,
    /// The latest ping, until its pong arrives.
    ping: Option<Outstanding>,
}

impl Watch {
    /// State for a tunnel opened at `now`.
    pub(super) fn new(now: Instant) -> Self {
        Self {
            started: now,
            sent: 0,
            received: 0,
            last_data: now,
            longest_wait: Duration::ZERO,
            stalled: false,
            next_ping: now + PING_EVERY,
            seq: 0,
            ping: None,
        }
    }

    /// Counts bytes sent to the gateway.
    pub(super) const fn sent(&mut self, n: usize) {
        self.sent += n as u64;
    }

    /// Counts gateway data and reports the end of a stall.
    pub(super) fn data(&mut self, now: Instant, n: usize, diag: &mut dyn Write) {
        let waited = now - self.last_data;
        self.longest_wait = self.longest_wait.max(waited);
        if self.stalled {
            log(diag, &format!("resume waited={}", secs(waited)));
            self.stalled = false;
        }
        self.received += n as u64;
        self.last_data = now;
    }

    /// Reports a stall or an unanswered ping; returns a ping payload when one is due.
    pub(super) fn tick(&mut self, now: Instant, diag: &mut dyn Write) -> Option<Vec<u8>> {
        let waited = now - self.last_data;
        if !self.stalled && waited > STALL {
            log(diag, &format!("stall waited={}", secs(waited)));
            self.stalled = true;
        }
        if let Some(ping) = self.ping.as_mut().filter(|p| !p.reported) {
            let age = now - ping.at;
            if age >= PING_UNANSWERED {
                log(
                    diag,
                    &format!("ping unanswered seq={} after={}", ping.seq, secs(age)),
                );
                ping.reported = true;
            }
        }
        if now < self.next_ping {
            return None;
        }
        self.next_ping = now + PING_EVERY;
        self.seq += 1;
        self.ping = Some(Outstanding {
            seq: self.seq,
            at: now,
            reported: false,
        });
        Some(self.seq.to_be_bytes().to_vec())
    }

    /// Matches a pong to the latest ping and reports a slow round trip.
    pub(super) fn pong(&mut self, now: Instant, payload: &[u8], diag: &mut dyn Write) {
        let Some(ping) = self.ping.take_if(|p| p.seq.to_be_bytes() == payload) else {
            return;
        };
        let rtt = now - ping.at;
        if rtt > SLOW_RTT {
            log(diag, &format!("ping rtt={} seq={}", secs(rtt), ping.seq));
        }
    }

    /// Writes the exit summary.
    pub(super) fn summary(&self, now: Instant, diag: &mut dyn Write) {
        let longest = self.longest_wait.max(now - self.last_data);
        log(
            diag,
            &format!(
                "exit sent={} received={} duration={} longest_wait={}",
                self.sent,
                self.received,
                secs(now - self.started),
                secs(longest),
            ),
        );
    }
}

/// Seconds with two decimals and a unit, as in `5.01s`.
fn secs(d: Duration) -> String {
    format!("{:.2}s", d.as_secs_f64())
}

/// Writes one prefixed diagnostic line; a failed stderr write is ignored.
fn log(diag: &mut dyn Write, line: &str) {
    let _ = writeln!(diag, "crabbox-agent37 tunnel: {line}");
}
