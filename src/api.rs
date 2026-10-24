//! HTTP implementation of [`Agent37`] over ureq.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::agent37::{Agent37, ApiError, CreateSpec, ExecOutput, Instance, SshKey};
use crate::key::ApiKey;

/// Image crabbox VMs are created from when the config names no `template`.
const TEMPLATE: &str = "agent37-ubuntu";

/// Production API root, used when the config carries no `apiUrl`.
const DEFAULT_BASE_URL: &str = "https://api.agent37.com";

/// Upper bound for one whole request when the config sets no `requestTimeoutSeconds`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-request timeout from the provider config: `requestTimeoutSeconds` if a positive integer, else the default.
#[must_use]
pub fn request_timeout(config: &Value) -> Duration {
    config
        .get("requestTimeoutSeconds")
        .and_then(Value::as_u64)
        .filter(|secs| *secs > 0)
        .map_or(REQUEST_TIMEOUT, Duration::from_secs)
}

/// API root from the provider config: `apiUrl` without trailing slashes, else the default.
#[must_use]
pub fn base_url(config: &Value) -> String {
    config
        .get("apiUrl")
        .and_then(Value::as_str)
        .map_or(DEFAULT_BASE_URL, |url| url.trim_end_matches('/'))
        .to_owned()
}

/// agent37 REST client; the key is only ever sent as a bearer header.
pub struct HttpAgent37 {
    /// Root URL without a trailing slash.
    base: String,
    /// Workspace API key.
    key: ApiKey,
    /// Shared ureq agent carrying the timeout and status handling.
    agent: ureq::Agent,
}

impl HttpAgent37 {
    /// Client for `base_url`, authenticating with `key`; each request is bounded by `timeout`.
    #[must_use]
    pub fn new(mut base_url: String, key: ApiKey, timeout: Duration) -> Self {
        base_url.truncate(base_url.trim_end_matches('/').len());
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            // Non-2xx responses are mapped by `into_body`, which reads agent37's error body.
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            base: base_url,
            key,
            agent,
        }
    }

    /// Bearer header value.
    fn bearer(&self) -> String {
        format!("Bearer {}", self.key.expose())
    }

    /// Absolute URL for `path`.
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Body text of a 2xx response; any other status becomes [`ApiError::Http`].
    fn into_body(
        result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<String, ApiError> {
        let mut response = result.map_err(|e| ApiError::Unreachable(e.to_string()))?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| ApiError::Unreachable(e.to_string()))?;
        if (200..300).contains(&status) {
            Ok(text)
        } else {
            Err(http_error(status, &text))
        }
    }

    /// GET `path` and decode the JSON body.
    fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, ApiError> {
        let result = self
            .agent
            .get(self.url(path))
            .header("Authorization", self.bearer())
            .call();
        decode(&Self::into_body(result)?)
    }

    /// POST `body` as JSON to `path` and decode the JSON response.
    fn post<T: for<'de> Deserialize<'de>>(&self, path: &str, body: &Value) -> Result<T, ApiError> {
        let result = self
            .agent
            .post(self.url(path))
            .header("Authorization", self.bearer())
            .send_json(body);
        decode(&Self::into_body(result)?)
    }
}

/// Request body for creating an instance.
///
/// Deliberately carries no `env` (no secrets in the instance) and no `budget` (managed budget stays at zero).
fn create_body(spec: &CreateSpec) -> Value {
    let metadata: Map<String, Value> = spec
        .metadata
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    let mut resources = Map::new();
    resources.insert("cpu".into(), json!(spec.cpu));
    resources.insert("memory".into(), json!(spec.memory_gb));
    resources.insert("disk".into(), json!(spec.disk_gb));
    json!({
        "template": spec.template.as_deref().unwrap_or(TEMPLATE),
        "type": "vm",
        "region": spec.region,
        "name": spec.name,
        "resources": resources,
        "metadata": metadata,
        "auto_sleep": false,
    })
}

/// Exec result as agent37 serialises it.
#[derive(Deserialize)]
struct WireExec {
    /// Process exit status.
    exit_code: i64,
    /// Captured standard output.
    #[serde(default)]
    stdout: String,
    /// Captured standard error.
    #[serde(default)]
    stderr: String,
}

/// Parse a JSON body.
fn decode<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T, ApiError> {
    serde_json::from_str(text).map_err(|e| ApiError::Decode(e.to_string()))
}

/// `{"data": [...]}` list envelope.
#[derive(Deserialize)]
struct Listing<T> {
    /// The listed items.
    data: Vec<T>,
}

/// Instance as agent37 serialises it.
#[derive(Deserialize)]
struct WireInstance {
    /// Instance id.
    id: String,
    /// Instance name.
    name: Option<String>,
    /// `vm` or `default`.
    #[serde(rename = "type")]
    kind: Option<String>,
    /// Lifecycle status.
    status: Option<String>,
    /// Metadata set at create; absent or `null` on instances created elsewhere.
    #[serde(default)]
    metadata: Option<BTreeMap<String, Value>>,
}

impl From<WireInstance> for Instance {
    fn from(w: WireInstance) -> Self {
        Self {
            id: w.id,
            name: w.name,
            kind: w.kind,
            status: w.status,
            metadata: w
                .metadata
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(k, v)| match v {
                    Value::String(s) => Some((k, s)),
                    _ => None,
                })
                .collect(),
        }
    }
}

/// SSH key as agent37 serialises it.
#[derive(Deserialize)]
struct WireKey {
    /// Key id.
    id: String,
    /// OpenSSH SHA256 fingerprint; other fields of the item are ignored.
    fingerprint: String,
}

/// Build the error for a non-2xx response.
///
/// Bodies are either `{"error":{"code","message"}}` or `{"error":"code","message"}`.
fn http_error(status: u16, body: &str) -> ApiError {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let text = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_owned);
    let error = parsed.get("error");
    let (code, message) = match error {
        Some(Value::String(code)) => (Some(code.clone()), text(parsed.get("message"))),
        Some(envelope @ Value::Object(_)) => (
            text(envelope.get("code")),
            text(envelope.get("message")).or_else(|| text(parsed.get("message"))),
        ),
        _ => (None, text(parsed.get("message"))),
    };
    ApiError::Http {
        status,
        code,
        message: message
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| format!("HTTP {status}")),
    }
}

/// Percent-encode everything outside the URL-unreserved set so an id cannot alter the path.
fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

impl Agent37 for HttpAgent37 {
    fn list_instances(&self) -> Result<Vec<Instance>, ApiError> {
        let listing: Listing<WireInstance> = self.get("/v1/instances")?;
        Ok(listing.data.into_iter().map(Instance::from).collect())
    }

    fn list_ssh_keys(&self) -> Result<Vec<SshKey>, ApiError> {
        let listing: Listing<WireKey> = self.get("/v1/ssh-keys")?;
        Ok(listing
            .data
            .into_iter()
            .map(|k| SshKey {
                id: k.id,
                fingerprint: k.fingerprint,
            })
            .collect())
    }

    fn delete_instance(&self, id: &str) -> Result<(), ApiError> {
        let result = self
            .agent
            .delete(self.url(&format!("/v1/instances/{}", encode_segment(id))))
            .header("Authorization", self.bearer())
            .call();
        Self::into_body(result).map(|_| ())
    }

    fn get_instance(&self, id: &str) -> Result<Instance, ApiError> {
        let wire: WireInstance = self.get(&format!("/v1/instances/{}", encode_segment(id)))?;
        Ok(wire.into())
    }

    fn create_instance(&self, spec: &CreateSpec) -> Result<Instance, ApiError> {
        let wire: WireInstance = self.post("/v1/instances", &create_body(spec))?;
        Ok(wire.into())
    }

    fn stop_instance(&self, id: &str) -> Result<(), ApiError> {
        let path = format!("/v1/instances/{}/stop", encode_segment(id));
        self.post::<Value>(&path, &json!({})).map(|_| ())
    }

    fn start_instance(&self, id: &str) -> Result<(), ApiError> {
        let path = format!("/v1/instances/{}/start", encode_segment(id));
        self.post::<Value>(&path, &json!({})).map(|_| ())
    }

    fn patch_instance(
        &self,
        id: &str,
        name: &str,
        metadata: &[(String, String)],
    ) -> Result<Instance, ApiError> {
        let metadata: Map<String, Value> = metadata
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let result = self
            .agent
            .patch(self.url(&format!("/v1/instances/{}", encode_segment(id))))
            .header("Authorization", self.bearer())
            .send_json(json!({ "name": name, "metadata": metadata }));
        let wire: WireInstance = decode(&Self::into_body(result)?)?;
        Ok(wire.into())
    }

    fn exec_as_root(&self, id: &str, command: &str) -> Result<ExecOutput, ApiError> {
        let path = format!("/v1/instances/{}/exec", encode_segment(id));
        let wire: WireExec = self.post(&path, &json!({"command": command, "user": "root"}))?;
        Ok(ExecOutput {
            exit_code: wire.exit_code,
            stdout: wire.stdout,
            stderr: wire.stderr,
        })
    }
}

#[cfg(test)]
mod tests;
