//! `tunnel <host>`: an SSH `ProxyCommand` that carries the SSH byte stream over
//! agent37's WebSocket endpoint, so hosts need no node or `agent37` npm CLI.

use std::ffi::c_int;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use tungstenite::client::IntoClientRequest;
use tungstenite::error::ProtocolError;
use tungstenite::protocol::CloseFrame;
use tungstenite::{Error as WsError, Message, WebSocket};

use crate::key::{self, ApiKey};

mod diaglog;
mod watch;

use diaglog::DiagLog;
use watch::Watch;

/// DNS zone of instance hosts, as in `<id>.agent37.app`.
const HOST_DOMAIN: &str = "agent37.app";
/// Port of the in-instance sshd behind the WebSocket proxy.
const SSH_PORT: u16 = 22022;
/// Header carrying the API key; the endpoint rejects the upgrade without it.
const KEY_HEADER: &str = "X-Agent37-Key";
/// Connect and upgrade deadline.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long one WebSocket read waits before pending stdin bytes are forwarded.
const POLL: Duration = Duration::from_millis(10);
/// Bytes read from stdin per WebSocket frame.
const CHUNK: usize = 16 * 1024;
/// Signals that end the tunnel with its exit summary, and their names in it.
/// ssh sends `TERM` or `HUP` to its `ProxyCommand` when the session dies.
const SIGNALS: [(c_int, &str); 3] = [(SIGTERM, "TERM"), (SIGHUP, "HUP"), (SIGINT, "INT")];

/// Instance id from an `<id>.agent37.app` host; anything else is refused.
pub fn instance_id(host: &str) -> Result<&str, String> {
    let id = host
        .strip_suffix(HOST_DOMAIN)
        .and_then(|rest| rest.strip_suffix('.'))
        .filter(|id| {
            id.len() == 10
                && id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
        });
    id.ok_or_else(|| format!("not an agent37 instance host: {host} (want <id>.{HOST_DOMAIN})"))
}

/// WebSocket URL of the instance's SSH endpoint.
#[must_use]
pub fn endpoint(id: &str) -> String {
    format!("wss://{id}-{SSH_PORT}.{HOST_DOMAIN}/")
}

/// Runs the subcommand with the arguments after `tunnel`; returns the exit code.
///
/// `url_override` replaces the endpoint for tests and is honoured only for
/// loopback `ws://` URLs, so the key can never be sent in clear to a real host.
/// `log_path` names a file that also receives every diagnostic line.
pub fn run(
    args: &[String],
    env_key: Option<String>,
    key_file: Option<String>,
    url_override: Option<String>,
    log_path: Option<&str>,
    stderr: &mut dyn Write,
) -> u8 {
    let mut diag = DiagLog::open(
        stderr,
        log_path.filter(|path| !path.is_empty()),
        args.first().map_or("-", String::as_str),
    );
    let diag = &mut diag;
    let result = prepare(args, env_key, key_file, url_override).and_then(|(url, key)| {
        bridge(
            &url,
            &key,
            std::io::stdin(),
            std::io::stdout().lock(),
            &mut *diag,
        )
    });
    match result {
        Ok(None) => 0,
        Ok(Some(signo)) => u8::try_from(128 + signo).unwrap_or(u8::MAX),
        Err(error) => {
            let _ = writeln!(diag, "crabbox-agent37 tunnel: {error}");
            1
        }
    }
}

/// Validates the host and loads the key.
fn prepare(
    args: &[String],
    env_key: Option<String>,
    key_file: Option<String>,
    url_override: Option<String>,
) -> Result<(String, ApiKey), String> {
    let [host] = args else {
        return Err("usage: crabbox-agent37 tunnel <id>.agent37.app".to_owned());
    };
    let id = instance_id(host)?;
    let config = key_file.map_or_else(
        || serde_json::json!({}),
        |path| serde_json::json!({ "keyFile": path }),
    );
    let key = key::resolve(&config, env_key)?;
    let url = url_override.unwrap_or_else(|| endpoint(id));
    Ok((url, key))
}

/// Transport under the WebSocket: TLS for `wss`, plain TCP for loopback `ws`.
enum Transport {
    /// Loopback test endpoint.
    Plain(TcpStream),
    /// Production endpoint.
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Transport {
    /// The underlying socket, for timeouts.
    fn tcp(&self) -> &TcpStream {
        match self {
            Self::Plain(tcp) => tcp,
            Self::Tls(tls) => &tls.sock,
        }
    }
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(tcp) => tcp.read(buf),
            Self::Tls(tls) => tls.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(tcp) => tcp.write(buf),
            Self::Tls(tls) => tls.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(tcp) => tcp.flush(),
            Self::Tls(tls) => tls.flush(),
        }
    }
}

/// Opens the WebSocket with the key header.
fn connect(url: &str, key: &ApiKey) -> Result<WebSocket<Transport>, String> {
    let mut request = url.into_client_request().map_err(|e| e.to_string())?;
    let uri = request.uri().clone();
    let host = uri.host().ok_or("endpoint has no host")?.to_owned();
    let secure = match uri.scheme_str() {
        Some("wss") => true,
        Some("ws") if matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]") => false,
        _ => return Err("endpoint must be wss (ws only on loopback)".to_owned()),
    };
    let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
    let value = key
        .expose()
        .parse()
        .map_err(|_| "API key is not a valid header value".to_owned())?;
    request.headers_mut().insert(KEY_HEADER, value);

    let tcp = TcpStream::connect((host.trim_matches(['[', ']']), port))
        .map_err(|e| format!("could not reach {host}: {e}"))?;
    tcp.set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|e| e.to_string())?;
    tcp.set_nodelay(true).map_err(|e| e.to_string())?;
    let transport = if secure {
        let roots = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(|e| e.to_string())?
                .with_root_certificates(roots)
                .with_no_client_auth();
        let name = ServerName::try_from(host.clone()).map_err(|e| e.to_string())?;
        let conn = ClientConnection::new(Arc::new(config), name).map_err(|e| e.to_string())?;
        Transport::Tls(Box::new(StreamOwned::new(conn, tcp)))
    } else {
        Transport::Plain(tcp)
    };
    let (socket, _) = tungstenite::client(request, transport)
        .map_err(|e| format!("tunnel to {host} refused: {e}"))?;
    socket
        .get_ref()
        .tcp()
        .set_read_timeout(Some(POLL))
        .map_err(|e| e.to_string())?;
    Ok(socket)
}

/// Pumps `input` to the WebSocket and WebSocket payloads to `output`, then
/// writes the exit summary to `diag`.
///
/// `Ok(None)` means the local side hung up (ssh exited) and `Ok(Some(signo))`
/// that one of [`SIGNALS`] arrived; a server close or any network failure is
/// an error so the caller sees the tunnel die.
fn bridge<R, W>(
    url: &str,
    key: &ApiKey,
    input: R,
    mut output: W,
    diag: &mut dyn Write,
) -> Result<Option<c_int>, String>
where
    R: Read + Send + 'static,
    W: Write,
{
    let mut socket = connect(url, key)?;
    // Holds the index into SIGNALS plus one of the last signal caught, 0 for none.
    let caught = Arc::new(AtomicUsize::new(0));
    for (index, (signo, _)) in SIGNALS.iter().enumerate() {
        signal_hook::flag::register_usize(*signo, Arc::clone(&caught), index + 1)
            .map_err(|e| format!("installing signal handler: {e}"))?;
    }
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(16);
    std::thread::spawn(move || {
        let mut input = input;
        let mut buf = vec![0_u8; CHUNK];
        while let Ok(n) = input.read(&mut buf) {
            let Some(chunk) = buf.get(..n).filter(|c| !c.is_empty()) else {
                break;
            };
            if tx.send(chunk.to_vec()).is_err() {
                break;
            }
        }
    });
    let mut watch = Watch::new(Instant::now());
    let result = pump(&mut socket, &rx, &mut output, &mut watch, &caught, diag);
    let signal = result.as_ref().ok().copied().flatten();
    watch.summary(Instant::now(), signal.map(|(_, name)| name), diag);
    result.map(|signal| signal.map(|(signo, _)| signo))
}

/// The transfer loop of [`bridge`].
fn pump<W: Write>(
    socket: &mut WebSocket<Transport>,
    rx: &mpsc::Receiver<Vec<u8>>,
    output: &mut W,
    watch: &mut Watch,
    caught: &AtomicUsize,
    diag: &mut dyn Write,
) -> Result<Option<(c_int, &'static str)>, String> {
    // The read timeout is the only wakeup, so stdin and signals are polled between reads.
    loop {
        if let Some(signal) = caught
            .load(Ordering::Relaxed)
            .checked_sub(1)
            .and_then(|index| SIGNALS.get(index))
        {
            return Ok(Some(*signal));
        }
        loop {
            match rx.try_recv() {
                Ok(chunk) => {
                    let n = chunk.len();
                    socket
                        .send(Message::binary(chunk))
                        .map_err(|e| format!("sending to tunnel: {e}"))?;
                    watch.sent(Instant::now(), n);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    let _ = socket.close(None);
                    return Ok(None);
                }
            }
        }
        if let Some(payload) = watch.tick(Instant::now(), diag) {
            socket
                .send(Message::Ping(payload.into()))
                .map_err(|e| format!("sending to tunnel: {e}"))?;
        }
        match socket.read() {
            Ok(Message::Binary(data)) => {
                watch.data(Instant::now(), data.len(), diag);
                forward(output, data.as_slice())?;
            }
            Ok(Message::Text(text)) => {
                watch.data(Instant::now(), text.len(), diag);
                forward(output, text.as_str().as_bytes())?;
            }
            Ok(Message::Pong(payload)) => watch.pong(Instant::now(), payload.as_slice(), diag),
            Ok(Message::Close(frame)) => return Err(describe_close(frame.as_ref())),
            Ok(_) => {}
            Err(WsError::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(describe_loss(&e)),
        }
    }
}

/// What the server's close frame said, as `code=<n> reason="<text>"`.
fn describe_close(frame: Option<&CloseFrame>) -> String {
    frame.map_or_else(
        || "tunnel closed by server with no close frame".to_owned(),
        |f| {
            format!(
                "tunnel closed by server code={} reason={:?}",
                u16::from(f.code),
                &*f.reason
            )
        },
    )
}

/// Why the connection failed without a close frame: an I/O error with its
/// kind, or the peer ending the stream with no closing handshake.
fn describe_loss(error: &WsError) -> String {
    match error {
        WsError::Io(e) => format!("tunnel lost: io error kind={:?}: {e}", e.kind()),
        WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake) => {
            "tunnel lost: connection reset or EOF without a closing handshake".to_owned()
        }
        other => format!("tunnel lost: {other}"),
    }
}

/// Writes one payload to the local side and flushes it.
fn forward<W: Write>(output: &mut W, data: &[u8]) -> Result<(), String> {
    output
        .write_all(data)
        .and_then(|()| output.flush())
        .map_err(|e| format!("writing to local side: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formed instance host yields its id.
    #[test]
    fn accepts_instance_host() {
        assert_eq!(instance_id("abc123def0.agent37.app"), Ok("abc123def0"));
    }

    /// Anything but `<id>.agent37.app` is refused.
    #[test]
    fn refuses_other_hosts() {
        for host in [
            "abc123def0",
            "abc123def0.example.com",
            "abc123def0.agent37.app.evil.com",
            "evilagent37.app",
            "abc123def0-22022.agent37.app",
            "ABC123DEF0.agent37.app",
            "short.agent37.app",
            ".agent37.app",
            "",
        ] {
            assert!(instance_id(host).is_err(), "{host}");
        }
    }

    /// The endpoint is the SSH port label under the proxy domain.
    #[test]
    fn endpoint_shape() {
        assert_eq!(
            endpoint("abc123def0"),
            "wss://abc123def0-22022.agent37.app/"
        );
    }
}
