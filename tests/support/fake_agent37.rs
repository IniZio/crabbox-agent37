//! In-process fake of the agent37 HTTP API for end-to-end tests.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test support fails loudly on malformed traffic"
)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use serde_json::{json, Value};

/// One request as the fake saw it.
#[derive(Clone, Debug)]
#[allow(dead_code, reason = "each test binary reads a different subset")]
pub(crate) struct RecordedRequest {
    /// HTTP method.
    pub(crate) method: String,
    /// Request target including any query string.
    pub(crate) path: String,
    /// Header names (lowercased) and values in arrival order.
    pub(crate) headers: Vec<(String, String)>,
    /// Raw body text.
    pub(crate) body: String,
}

impl RecordedRequest {
    /// Value of the first header called `name`, case-insensitive.
    #[allow(dead_code, reason = "each test binary uses a different subset")]
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Mutable server state shared with the accept thread.
struct State {
    /// Bearer token the fake accepts.
    key: String,
    /// Instances as API objects.
    instances: Vec<Value>,
    /// SSH keys as API objects.
    ssh_keys: Vec<Value>,
    /// Whether VM creation answers 403 `tier_limit`.
    vm_tier_limited: bool,
    /// Every request received, in order.
    requests: Vec<RecordedRequest>,
    /// Counter for generated ids.
    next_id: u32,
    /// Status given to freshly created instances.
    create_status: String,
    /// GETs of an instance before provisioning|starting turns running; `None` = never.
    polls_until_running: Option<u32>,
    /// GET counts per instance id.
    polls: HashMap<String, u32>,
    /// Whether exec answers 502 `provisioning_failed`.
    exec_fails: bool,
}

/// Handle to a running fake; the server thread lives until the test process exits.
pub(crate) struct FakeAgent37 {
    /// Base URL without trailing slash.
    url: String,
    /// State shared with the server thread.
    state: Arc<Mutex<State>>,
}

impl FakeAgent37 {
    /// Starts a fake on a free local port that accepts only `key`.
    pub(crate) fn start_with_key(key: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State {
            key: key.to_owned(),
            instances: Vec::new(),
            ssh_keys: Vec::new(),
            vm_tier_limited: false,
            requests: Vec::new(),
            next_id: 1,
            create_status: "running".to_owned(),
            polls_until_running: None,
            polls: HashMap::new(),
            exec_fails: false,
        }));
        let shared = Arc::clone(&state);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let shared = Arc::clone(&shared);
                thread::spawn(move || serve(stream, &shared));
            }
        });
        Self { url, state }
    }

    /// Base URL, e.g. `http://127.0.0.1:41234`.
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// Locks the shared state.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Seeds an instance of the given `kind` (`vm`, `container`, ...).
    #[allow(dead_code, reason = "each test binary uses a different subset")]
    pub(crate) fn add_instance(&self, id: &str, name: &str, kind: &str) {
        self.lock().instances.push(instance_json(id, name, kind));
    }

    /// Seeds an instance with an explicit `status`.
    #[allow(dead_code, reason = "each test binary uses a different subset")]
    pub(crate) fn add_instance_with_status(&self, id: &str, name: &str, kind: &str, status: &str) {
        let mut instance = instance_json(id, name, kind);
        instance["status"] = json!(status);
        self.lock().instances.push(instance);
    }

    /// Sets the status new instances get (default `running`).
    #[allow(dead_code, reason = "each test binary uses a different subset")]
    pub(crate) fn set_create_status(&self, status: &str) {
        status.clone_into(&mut self.lock().create_status);
    }

    /// Sets how many GETs of an instance pass before provisioning|starting turns running.
    #[allow(dead_code, reason = "each test binary uses a different subset")]
    pub(crate) fn set_polls_until_running(&self, polls: Option<u32>) {
        self.lock().polls_until_running = polls;
    }

    /// Makes exec answer 502 `provisioning_failed`.
    #[allow(dead_code, reason = "each test binary uses a different subset")]
    pub(crate) fn set_exec_fails(&self, fails: bool) {
        self.lock().exec_fails = fails;
    }

    /// The instance object with `id`, if present.
    #[allow(dead_code, reason = "each test binary uses a different subset")]
    pub(crate) fn instance(&self, id: &str) -> Option<Value> {
        self.lock()
            .instances
            .iter()
            .find(|i| i["id"] == id)
            .cloned()
    }

    /// All instance objects.
    #[allow(dead_code, reason = "each test binary uses a different subset")]
    pub(crate) fn instances(&self) -> Vec<Value> {
        self.lock().instances.clone()
    }

    /// Registers an OpenSSH public key line with the workspace.
    #[allow(dead_code, reason = "each test binary uses a different subset")]
    pub(crate) fn add_ssh_key(&self, public_key: &str) {
        let mut state = self.lock();
        let id = format!("key_{}", state.ssh_keys.len() + 1);
        state.ssh_keys.push(json!({
            "id": id,
            "public_key": public_key,
            "name": "test key",
        }));
        drop(state);
    }

    /// Makes `POST /v1/instances` with `type: vm` fail like an unfunded workspace.
    #[allow(dead_code, reason = "each test binary uses a different subset")]
    pub(crate) fn set_vm_tier_limited(&self, limited: bool) {
        self.lock().vm_tier_limited = limited;
    }

    /// Snapshot of every request received so far.
    pub(crate) fn requests(&self) -> Vec<RecordedRequest> {
        self.lock().requests.clone()
    }
}

/// The host key line the fake serves for instance `id`.
#[allow(dead_code, reason = "each test binary uses a different subset")]
pub(crate) fn host_key_line(id: &str) -> String {
    format!("{} root@{id}\n", expected_host_key(id))
}

/// The key material (no comment, no newline) crabbox should pin for `id`.
#[allow(dead_code, reason = "each test binary uses a different subset")]
pub(crate) fn expected_host_key(id: &str) -> String {
    format!("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI{id}")
}

/// An instance API object.
fn instance_json(id: &str, name: &str, kind: &str) -> Value {
    json!({ "id": id, "name": name, "type": kind, "status": "running" })
}

/// Reads one request from `stream`, answers it and closes the connection.
fn serve(stream: TcpStream, state: &Mutex<State>) {
    let Some(request) = read_request(&stream) else {
        return;
    };
    let (status, body) = route(&request, state);
    let reason = match status {
        200 => "OK",
        201 => "Created",
        401 => "Unauthorized",
        400 => "Bad Request",
        403 => "Forbidden",
        502 => "Bad Gateway",
        _ => "Not Found",
    };
    let payload = body.to_string();
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let mut stream = stream;
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Parses an HTTP/1.1 request with an optional `Content-Length` body.
fn read_request(stream: &TcpStream) -> Option<RecordedRequest> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line).ok()?;
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        let (name, value) = trimmed.split_once(':')?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
    let length = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(RecordedRequest {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// Error body in the nested shape used by most endpoints.
fn nested_error(code: &str, message: &str) -> Value {
    json!({ "error": { "code": code, "message": message } })
}

/// Records `request`, then dispatches it; returns status and JSON body.
fn route(request: &RecordedRequest, state: &Mutex<State>) -> (u16, Value) {
    let mut state = state.lock().unwrap();
    state.requests.push(request.clone());
    let expected = format!("Bearer {}", state.key);
    if request.header("authorization") != Some(expected.as_str()) {
        return (
            401,
            json!({ "error": "invalid_api_key", "message": "Invalid API key." }),
        );
    }
    let path = request.path.split('?').next().unwrap_or_default();
    match (request.method.as_str(), path) {
        ("GET", "/v1/instances") => (200, json!({ "data": state.instances })),
        ("GET", "/v1/ssh-keys") => (200, json!({ "data": state.ssh_keys })),
        ("POST", "/v1/instances") => create_instance(&mut state, &request.body),
        ("GET", _) if path.starts_with("/v1/instances/") => {
            let id = path.trim_start_matches("/v1/instances/");
            get_instance(&mut state, id)
        }
        ("POST", _) if path.starts_with("/v1/instances/") && path.ends_with("/exec") => {
            let id = path
                .trim_start_matches("/v1/instances/")
                .trim_end_matches("/exec");
            exec(&state, id, &request.body)
        }
        ("DELETE", _) if path.starts_with("/v1/instances/") => {
            let id = path.trim_start_matches("/v1/instances/");
            let before = state.instances.len();
            state.instances.retain(|i| i["id"] != id);
            if state.instances.len() == before {
                (404, nested_error("not_found", "Instance not found."))
            } else {
                (200, json!({ "id": id, "deleted": true }))
            }
        }
        _ => (404, nested_error("not_found", "No such route.")),
    }
}

/// Handles `POST /v1/instances`.
fn create_instance(state: &mut State, body: &str) -> (u16, Value) {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let kind = parsed["type"].as_str().unwrap_or("container").to_owned();
    if kind == "vm" && state.vm_tier_limited {
        return (
            403,
            nested_error(
                "tier_limit",
                "VM instances are not available on your current plan. Add funds to your workspace to unlock them.",
            ),
        );
    }
    let id = format!("vm{:08}", state.next_id);
    state.next_id += 1;
    let name = parsed["name"].as_str().unwrap_or("unnamed");
    let mut created = instance_json(&id, name, &kind);
    created["status"] = json!(state.create_status);
    for field in ["region", "resources", "metadata", "auto_sleep"] {
        if !parsed[field].is_null() {
            created[field] = parsed[field].clone();
        }
    }
    state.instances.push(created.clone());
    (201, created)
}

/// Handles `GET /v1/instances/{id}`, advancing provisioning instances.
fn get_instance(state: &mut State, id: &str) -> (u16, Value) {
    let count = state.polls.entry(id.to_owned()).or_insert(0);
    let seen = *count;
    *count += 1;
    let promote = state.polls_until_running.is_some_and(|n| seen >= n);
    let Some(instance) = state.instances.iter_mut().find(|i| i["id"] == id) else {
        return (404, nested_error("not_found", "Instance not found."));
    };
    if promote
        && matches!(
            instance["status"].as_str(),
            Some("provisioning" | "starting")
        )
    {
        instance["status"] = json!("running");
    }
    (200, instance.clone())
}

/// Handles `POST /v1/instances/{id}/exec`.
fn exec(state: &State, id: &str, body: &str) -> (u16, Value) {
    let Some(instance) = state.instances.iter().find(|i| i["id"] == id) else {
        return (404, nested_error("not_found", "Instance not found."));
    };
    if state.exec_fails {
        return (
            502,
            nested_error("provisioning_failed", "VM provisioning failed."),
        );
    }
    if instance["status"] != "running" {
        return (
            400,
            nested_error("invalid_request", "Instance is not running."),
        );
    }
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    if parsed["command"] == "cat /etc/ssh/ssh_host_ed25519_key.pub" && parsed["user"] == "root" {
        (
            200,
            json!({ "exit_code": 0, "stdout": host_key_line(id), "stderr": "" }),
        )
    } else {
        (
            200,
            json!({ "exit_code": 127, "stdout": "", "stderr": "command not found" }),
        )
    }
}
