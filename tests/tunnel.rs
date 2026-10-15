//! End-to-end tests of `tunnel` against a fake agent37 WebSocket endpoint.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail loudly on setup errors"
)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::{Message, WebSocket};

/// Key the fake accepts.
const KEY: &str = "sk_test_fake";
/// A host that passes validation.
const HOST: &str = "abc123def0.agent37.app";

/// What the fake does after the upgrade.
#[derive(Clone, Copy)]
enum Behaviour {
    /// Relay bytes to and from `target`.
    Bridge(SocketAddr),
    /// Close the WebSocket right away.
    CloseNow,
}

/// Accepts upgrades carrying the right key header; returns the URL.
fn fake_endpoint(behaviour: Behaviour) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}/", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for tcp in listener.incoming().flatten() {
            std::thread::spawn(move || serve(tcp, behaviour));
        }
    });
    url
}

/// One upgraded connection.
fn serve(tcp: TcpStream, behaviour: Behaviour) {
    #[allow(
        clippy::result_large_err,
        reason = "signature fixed by tungstenite's Callback"
    )]
    fn check(request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        match request.headers().get("x-agent37-key") {
            Some(value) if value == KEY => Ok(response),
            _ => Err(ErrorResponse::new(Some("unauthorized".to_owned()))),
        }
    }
    let Ok(mut ws) = tungstenite::accept_hdr(tcp, check) else {
        return;
    };
    match behaviour {
        Behaviour::CloseNow => {
            let _ = ws.close(None);
            while ws.read().is_ok() {}
        }
        Behaviour::Bridge(target) => relay(&mut ws, TcpStream::connect(target).unwrap()),
    }
}

/// Copies between the WebSocket and `target` until either side ends.
fn relay(ws: &mut WebSocket<TcpStream>, mut target: TcpStream) {
    let poll = Some(Duration::from_millis(5));
    ws.get_ref().set_read_timeout(poll).unwrap();
    target.set_read_timeout(poll).unwrap();
    let mut buf = [0_u8; 8192];
    loop {
        match ws.read() {
            Ok(Message::Binary(data)) => target.write_all(data.as_slice()).unwrap(),
            Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Ok(Message::Close(_)) | Err(_) => return,
            Ok(_) => {}
        }
        match target.read(&mut buf) {
            Ok(0) => {
                let _ = ws.close(None);
            }
            Ok(n) => ws.send(Message::binary(buf[..n].to_vec())).unwrap(),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return,
        }
    }
}

/// A TCP echo server; returns its address.
fn echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut tcp in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut buf = [0_u8; 4096];
                while let Ok(n) = tcp.read(&mut buf) {
                    if n == 0 || tcp.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

/// The binary under test, aimed at `url` with the given key.
fn tunnel(host: &str, url: &str, key: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_crabbox-agent37"));
    command
        .args(["tunnel", host])
        .env("AGENT37_API_KEY", key)
        .env("AGENT37_TUNNEL_URL", url)
        .env_remove("AGENT37_KEY_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Waits for exit, failing the test instead of hanging.
fn wait(child: &mut std::process::Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "tunnel did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Stdin bytes reach the target and its reply returns on stdout; stdin EOF exits 0.
#[test]
fn bridges_bytes_both_ways() {
    let url = fake_endpoint(Behaviour::Bridge(echo_server()));
    let mut child = tunnel(HOST, &url, KEY).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let payload: Vec<u8> = (0..100_000_u32).map(|i| (i % 251) as u8).collect();
    let sent = payload.clone();
    let writer = std::thread::spawn(move || {
        stdin.write_all(&sent).unwrap();
        stdin
    });
    let mut echoed = vec![0_u8; payload.len()];
    stdout.read_exact(&mut echoed).unwrap();
    assert!(echoed == payload, "echoed bytes differ");
    drop(writer.join().unwrap());
    assert!(wait(&mut child).success());
}

/// Hosts that are not `<id>.agent37.app` exit 1 without connecting.
#[test]
fn invalid_host_exits_one() {
    for host in ["abc123def0", "abc123def0.example.com", "x.agent37.app"] {
        let mut child = tunnel(host, "ws://127.0.0.1:1/", KEY).spawn().unwrap();
        drop(child.stdin.take());
        let status = wait(&mut child);
        assert_eq!(status.code(), Some(1), "{host}");
        let mut err = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut err)
            .unwrap();
        assert!(err.contains("not an agent37 instance host"), "{err}");
    }
}

/// A server-side close ends the process nonzero.
#[test]
fn server_close_exits_nonzero() {
    let url = fake_endpoint(Behaviour::CloseNow);
    let mut child = tunnel(HOST, &url, KEY).spawn().unwrap();
    let _stdin = child.stdin.take().unwrap();
    assert_eq!(wait(&mut child).code(), Some(1));
}

/// Losing the connection (nothing listening) ends the process nonzero.
#[test]
fn unreachable_endpoint_exits_nonzero() {
    let dead = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let mut child = tunnel(HOST, &format!("ws://{dead}/"), KEY).spawn().unwrap();
    drop(child.stdin.take());
    assert_eq!(wait(&mut child).code(), Some(1));
}

/// A rejected key is refused, and the key is never echoed to stderr.
#[test]
fn wrong_key_is_refused_without_echo() {
    let url = fake_endpoint(Behaviour::Bridge(echo_server()));
    let mut child = tunnel(HOST, &url, "sk_wrong_secret").spawn().unwrap();
    let _stdin = child.stdin.take().unwrap();
    assert_eq!(wait(&mut child).code(), Some(1));
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert!(err.contains("refused"), "{err}");
    assert!(!err.contains("sk_wrong_secret"), "{err}");
}

/// The key never goes in clear to a non-loopback endpoint.
#[test]
fn plaintext_non_loopback_is_refused() {
    let mut child = tunnel(HOST, "ws://example.com/", KEY).spawn().unwrap();
    drop(child.stdin.take());
    assert_eq!(wait(&mut child).code(), Some(1));
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert!(err.contains("wss"), "{err}");
}

/// First existing path among `candidates`.
fn find_tool(candidates: &[&str]) -> Option<String> {
    candidates
        .iter()
        .find(|path| std::path::Path::new(path).exists())
        .map(|path| (*path).to_owned())
}

/// A free loopback port.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Kills the child on drop so a failed assertion leaves no sshd behind.
struct Guard(std::process::Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts an unprivileged sshd on a free loopback port that accepts `client.pub`.
fn start_sshd(dir: &tempfile::TempDir, sshd: &str, keygen: &str) -> (Guard, u16) {
    let path = |name: &str| dir.path().join(name);
    for name in ["host", "client"] {
        let status = Command::new(keygen)
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(path(name))
            .status()
            .unwrap();
        assert!(status.success());
    }
    std::fs::copy(path("client.pub"), path("authorized_keys")).unwrap();
    let port = free_port();
    let config = format!(
        "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\n\
         StrictModes no\nUsePAM no\nPasswordAuthentication no\nAllowTcpForwarding yes\n\
         GatewayPorts no\n",
        path("host").display(),
        path("pid").display(),
        path("authorized_keys").display(),
    );
    std::fs::write(path("sshd_config"), config).unwrap();
    let guard = Guard(
        Command::new(sshd)
            .arg("-D")
            .arg("-f")
            .arg(path("sshd_config"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "sshd did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    (guard, port)
}

/// ssh with `-R` from loopback `remote` to a local echo server, through the tunnel binary.
fn spawn_ssh(ssh: &str, identity: &std::path::Path, url: &str, remote: u16) -> Guard {
    let echo = echo_server();
    let proxy = format!("{} tunnel {HOST}", env!("CARGO_BIN_EXE_crabbox-agent37"));
    let user = std::env::var("USER").unwrap_or_else(|_| "root".to_owned());
    Guard(
        Command::new(ssh)
            .args([
                "-N",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "StrictHostKeyChecking=no",
            ])
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "UserKnownHostsFile=/dev/null",
            ])
            .arg("-i")
            .arg(identity)
            .arg("-o")
            .arg(format!("ProxyCommand={proxy}"))
            .arg("-R")
            .arg(format!("127.0.0.1:{remote}:{echo}"))
            .arg(format!("{user}@{HOST}"))
            .env("AGENT37_API_KEY", KEY)
            .env("AGENT37_TUNNEL_URL", url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    )
}

/// `ssh -N -o ExitOnForwardFailure=yes -R` through the tunnel to a real sshd
/// holds the forward: a connection to the remote port reaches the local echo.
#[test]
#[allow(
    clippy::print_stderr,
    reason = "visible notice when the host has no sshd"
)]
fn ssh_reverse_forward_holds_through_tunnel() {
    let (Some(sshd), Some(ssh), Some(keygen)) = (
        find_tool(&["/usr/sbin/sshd", "/usr/bin/sshd"]),
        find_tool(&["/usr/bin/ssh", "/bin/ssh"]),
        find_tool(&["/usr/bin/ssh-keygen", "/bin/ssh-keygen"]),
    ) else {
        eprintln!("skipped: sshd, ssh or ssh-keygen not installed");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (_sshd, port) = start_sshd(&dir, &sshd, &keygen);
    let url = fake_endpoint(Behaviour::Bridge(
        format!("127.0.0.1:{port}").parse().unwrap(),
    ));
    let remote = free_port();
    let mut client = spawn_ssh(&ssh, &dir.path().join("client"), &url, remote);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut forwarded = loop {
        if let Ok(tcp) = TcpStream::connect(("127.0.0.1", remote)) {
            break tcp;
        }
        if let Some(status) = client.0.try_wait().unwrap() {
            let mut err = String::new();
            client
                .0
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut err)
                .unwrap();
            panic!("ssh exited early ({status}): {err}");
        }
        assert!(Instant::now() < deadline, "forward never came up");
        std::thread::sleep(Duration::from_millis(100));
    };
    forwarded
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    forwarded.write_all(b"through the forward").unwrap();
    let mut reply = [0_u8; 19];
    forwarded.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"through the forward");
    assert!(
        client.0.try_wait().unwrap().is_none(),
        "ssh must still hold the forward"
    );
}
