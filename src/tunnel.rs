//! `tunnel <host>`: an SSH `ProxyCommand` that carries the SSH byte stream over
//! agent37's WebSocket endpoint, so hosts need no node or `agent37` npm CLI.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use tungstenite::client::IntoClientRequest;
use tungstenite::{Error as WsError, Message, WebSocket};

use crate::key::{self, ApiKey};

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
pub fn run(
    args: &[String],
    env_key: Option<String>,
    key_file: Option<String>,
    url_override: Option<String>,
    diag: &mut dyn Write,
) -> u8 {
    let result = prepare(args, env_key, key_file, url_override)
        .and_then(|(url, key)| bridge(&url, &key, std::io::stdin(), std::io::stdout().lock()));
    match result {
        Ok(()) => 0,
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

/// Pumps `input` to the WebSocket and WebSocket payloads to `output`.
///
/// `Ok` means the local side hung up (ssh exited); a server close or any
/// network failure is an error so the caller sees the tunnel die.
fn bridge<R, W>(url: &str, key: &ApiKey, input: R, mut output: W) -> Result<(), String>
where
    R: Read + Send + 'static,
    W: Write,
{
    let mut socket = connect(url, key)?;
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
    // The read timeout is the only wakeup, so stdin is polled between reads.
    loop {
        loop {
            match rx.try_recv() {
                Ok(chunk) => socket
                    .send(Message::binary(chunk))
                    .map_err(|e| format!("sending to tunnel: {e}"))?,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    let _ = socket.close(None);
                    return Ok(());
                }
            }
        }
        match socket.read() {
            Ok(Message::Binary(data)) => forward(&mut output, data.as_slice())?,
            Ok(Message::Text(text)) => forward(&mut output, text.as_str().as_bytes())?,
            Ok(Message::Close(_)) => return Err("tunnel closed by server".to_owned()),
            Ok(_) => {}
            Err(WsError::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(format!("tunnel lost: {e}")),
        }
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
