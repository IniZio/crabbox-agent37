//! `CRABBOX_AGENT37_TUNNEL_LOG` mirrors the tunnel's diagnostics into a file.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail loudly on setup errors"
)]

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::CloseFrame;

/// Key the fake accepts and the log must never hold.
const KEY: &str = "sk_test_log_key_9f3a";
/// A host that passes validation.
const HOST: &str = "abc123def0.agent37.app";

/// What the fake does after the upgrade.
#[derive(Clone, Copy)]
enum Behaviour {
    /// Close with code 1011 and reason `upstream sshd gone`.
    Close,
    /// Drop the TCP connection with no closing handshake.
    Drop,
}

/// Accepts every upgrade and applies `behaviour`; returns the URL.
fn fake_endpoint(behaviour: Behaviour) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "ws://{}/?token=query-secret",
        listener.local_addr().unwrap()
    );
    std::thread::spawn(move || {
        for tcp in listener.incoming().flatten() {
            std::thread::spawn(move || serve(tcp, behaviour));
        }
    });
    url
}

/// One upgraded connection.
fn serve(tcp: TcpStream, behaviour: Behaviour) {
    let Ok(mut ws) = tungstenite::accept(tcp) else {
        return;
    };
    match behaviour {
        Behaviour::Close => {
            let frame = CloseFrame {
                code: CloseCode::Error,
                reason: "upstream sshd gone".into(),
            };
            let _ = ws.close(Some(frame));
            while ws.read().is_ok() {}
        }
        Behaviour::Drop => drop(ws),
    }
}

/// Runs the tunnel against `url` with `log` as its log path (if any) until it exits.
fn run_tunnel(url: &str, log: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_crabbox-agent37"));
    command
        .args(["tunnel", HOST])
        .env("AGENT37_API_KEY", KEY)
        .env("AGENT37_TUNNEL_URL", url)
        .env_remove("AGENT37_KEY_FILE")
        .env_remove("CRABBOX_AGENT37_TUNNEL_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(log) = log {
        command.env("CRABBOX_AGENT37_TUNNEL_LOG", log);
    }
    let mut child = command.spawn().unwrap();
    let stdin = child.stdin.take();
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "tunnel did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(stdin);
    child.wait_with_output().unwrap()
}

/// The log lines, each split into timestamp, pid field, host field and message.
fn log_lines(path: &Path) -> Vec<String> {
    let mut text = String::new();
    std::fs::File::open(path)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    text.lines().map(str::to_owned).collect()
}

/// A UTC millisecond timestamp such as `2026-10-22T09:15:30.123Z`.
const fn is_timestamp(word: &str) -> bool {
    let b = word.as_bytes();
    b.len() == 24
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && b[19] == b'.'
        && b[23] == b'Z'
}

/// A server close lands in the file with timestamp, pid, host, code and reason, and on stderr.
#[test]
fn server_close_is_logged_with_code_and_reason() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("tunnel.log");
    let out = run_tunnel(&fake_endpoint(Behaviour::Close), Some(&log));
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).unwrap();
    let lines = log_lines(&log);
    let closed = lines
        .iter()
        .find(|l| l.contains("closed by server"))
        .unwrap_or_else(|| panic!("no close line in {lines:?}"));
    let mut words = closed.splitn(4, ' ');
    assert!(is_timestamp(words.next().unwrap()), "{closed}");
    let pid = words.next().unwrap().strip_prefix("pid=").unwrap();
    assert!(pid.parse::<u32>().is_ok(), "{closed}");
    assert_eq!(words.next(), Some(format!("host={HOST}").as_str()));
    let message = words.next().unwrap();
    assert!(message.contains("code=1011"), "{closed}");
    assert!(message.contains("upstream sshd gone"), "{closed}");
    assert!(stderr.contains(message), "{stderr}");
    assert!(
        lines.iter().any(|l| l.contains("tunnel: exit ")),
        "{lines:?}"
    );
    let mode = std::fs::metadata(&log).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// A dropped TCP connection reads as a reset or EOF, not as a server close.
#[test]
fn abrupt_drop_is_logged_as_reset_not_close() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("tunnel.log");
    let out = run_tunnel(&fake_endpoint(Behaviour::Drop), Some(&log));
    assert_eq!(out.status.code(), Some(1));
    let lines = log_lines(&log);
    let lost = lines
        .iter()
        .find(|l| l.contains("tunnel lost"))
        .unwrap_or_else(|| panic!("no lost line in {lines:?}"));
    assert!(
        lost.contains("reset") || lost.contains("eof") || lost.contains("EOF"),
        "{lost}"
    );
    assert!(!lost.contains("closed by server"), "{lost}");
    assert!(
        !lines.iter().any(|l| l.contains("closed by server")),
        "{lines:?}"
    );
}

/// An unwritable log path warns once on stderr and the tunnel still works.
#[test]
fn unwritable_log_path_warns_once_and_tunnel_works() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("missing-dir").join("tunnel.log");
    let out = run_tunnel(&fake_endpoint(Behaviour::Close), Some(&log));
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("closed by server"), "{stderr}");
    let warnings = stderr.lines().filter(|l| l.contains("tunnel log")).count();
    assert_eq!(warnings, 1, "{stderr}");
    assert!(!log.exists());
}

/// Neither the API key nor the tunnel URL's query string reaches the log.
#[test]
fn log_never_holds_key_or_query() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("tunnel.log");
    for behaviour in [Behaviour::Close, Behaviour::Drop] {
        let out = run_tunnel(&fake_endpoint(behaviour), Some(&log));
        assert!(out.status.code().is_some());
    }
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("tunnel: exit "), "{text}");
    assert!(!text.contains(KEY), "{text}");
    assert!(
        !text.contains("query-secret") && !text.contains("token="),
        "{text}"
    );
}
