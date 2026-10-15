//! End-to-end tests of the `doctor` operation against a fake agent37 API.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail loudly on setup errors"
)]

#[path = "support/fake_agent37.rs"]
mod fake_agent37;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use fake_agent37::FakeAgent37;
use serde_json::{json, Value};
use tempfile::TempDir;

/// Key the fake accepts.
const KEY: &str = "sk_test_fake";
/// Public key line registered in the happy-path setups.
const PUBLIC_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFakeBlobForTestsOnly12345678 test@host";

/// A fake server plus key files on disk.
struct Setup {
    /// Keeps the key files alive.
    dir: TempDir,
    /// The fake API.
    fake: FakeAgent37,
}

impl Setup {
    /// Fake with `KEY`, an SSH keypair on disk and its public key registered.
    fn new() -> Self {
        let setup = Self::unregistered();
        setup.fake.add_ssh_key(PUBLIC_KEY);
        setup
    }

    /// Like `new` but the public key is not registered with the workspace.
    fn unregistered() -> Self {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("id_ed25519"), "private\n").unwrap();
        std::fs::write(dir.path().join("id_ed25519.pub"), format!("{PUBLIC_KEY}\n")).unwrap();
        Self {
            dir,
            fake: FakeAgent37::start_with_key(KEY),
        }
    }

    /// Writes an API key file with `mode`.
    fn key_file(&self, contents: &str, mode: u32) -> PathBuf {
        let path = self.dir.path().join("api_key");
        std::fs::write(&path, format!("{contents}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    /// Config without a `keyFile`.
    fn config(&self) -> Value {
        json!({
            "apiUrl": self.fake.url(),
            "sshKey": self.dir.path().join("id_ed25519").to_str().unwrap(),
        })
    }

    /// Config pointing at a key file holding `contents` with `mode`.
    fn config_with_key_file(&self, contents: &str, mode: u32) -> Value {
        let mut config = self.config();
        config["keyFile"] = json!(self.key_file(contents, mode).to_str().unwrap());
        config
    }
}

/// What the binary printed and returned.
struct Outcome {
    /// Process exit code.
    code: i32,
    /// Parsed stdout.
    stdout: Value,
}

impl Outcome {
    /// The `message` or `error` text.
    fn text(&self) -> &str {
        self.stdout["message"]
            .as_str()
            .or_else(|| self.stdout["error"].as_str())
            .unwrap()
    }
}

/// Runs the binary with `request` on stdin; no secret in `secrets` may reach stdout or stderr.
fn run(request: &Value, env_key: Option<&str>, secrets: &[&str]) -> Outcome {
    let mut command = Command::new(env!("CARGO_BIN_EXE_crabbox-agent37"));
    command
        .env_remove("AGENT37_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(key) = env_key {
        command.env("AGENT37_API_KEY", key);
    }
    let mut child = command.spawn().unwrap();
    std::io::Write::write_all(
        &mut child.stdin.take().unwrap(),
        request.to_string().as_bytes(),
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    for secret in secrets {
        assert!(!stdout.contains(secret), "key leaked to stdout: {stdout}");
        assert!(!stderr.contains(secret), "key leaked to stderr: {stderr}");
    }
    Outcome {
        code: output.status.code().unwrap(),
        stdout: serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("bad stdout {stdout:?}: {e}")),
    }
}

/// A `doctor` request carrying `config`.
fn doctor(config: &Value) -> Value {
    json!({ "protocolVersion": 1, "operation": "doctor", "config": config })
}

/// Methods and paths the fake saw, as `"METHOD /path"`.
fn seen(fake: &FakeAgent37) -> Vec<String> {
    fake.requests()
        .iter()
        .map(|r| format!("{} {}", r.method, r.path))
        .collect()
}

#[test]
fn healthy_workspace_reports_every_check_without_creating_anything() {
    let setup = Setup::new();
    setup.fake.add_instance("inst_a", "dev", "vm");
    let out = run(
        &doctor(&setup.config_with_key_file(KEY, 0o600)),
        None,
        &[KEY],
    );
    assert_eq!(out.code, 0, "{}", out.text());
    assert_eq!(out.stdout["protocolVersion"], 1);
    let message = out.text();
    assert!(message.starts_with("agent37 ready"), "{message}");
    for part in [
        "workspace reachable",
        "VM instances available",
        "SSH key registered",
    ] {
        assert!(message.contains(part), "missing {part:?} in {message}");
    }
    assert!(!seen(&setup.fake).iter().any(|r| r.starts_with("POST")));
}

#[test]
fn missing_vm_is_probed_with_create_then_delete() {
    let setup = Setup::new();
    setup.fake.add_instance("inst_a", "box", "container");
    let out = run(
        &doctor(&setup.config_with_key_file(KEY, 0o600)),
        None,
        &[KEY],
    );
    assert_eq!(out.code, 0, "{}", out.text());
    assert!(out.text().contains("VM instances available"));
    let requests = setup.fake.requests();
    let post = requests.iter().find(|r| r.method == "POST").unwrap();
    assert_eq!(post.path, "/v1/instances");
    let body: Value = serde_json::from_str(&post.body).unwrap();
    assert_eq!(body["type"], "vm");
    assert_eq!(body["name"], "crabbox-agent37-doctor-probe");
    assert!(
        body.get("env").is_none() && body.get("budget").is_none(),
        "{body}"
    );
    let delete = requests.iter().find(|r| r.method == "DELETE").unwrap();
    assert_eq!(delete.path, "/v1/instances/inst_1");
    let post_at = requests.iter().position(|r| r.method == "POST").unwrap();
    let delete_at = requests.iter().position(|r| r.method == "DELETE").unwrap();
    assert!(post_at < delete_at);
}

#[test]
fn tier_limited_workspace_fails_with_funding_hint() {
    let setup = Setup::new();
    setup.fake.set_vm_tier_limited(true);
    let out = run(
        &doctor(&setup.config_with_key_file(KEY, 0o600)),
        None,
        &[KEY],
    );
    assert_eq!(out.code, 1);
    assert!(
        out.text().contains("workspace not funded for VM instances"),
        "{}",
        out.text()
    );
}

#[test]
fn unregistered_ssh_key_fails() {
    let setup = Setup::unregistered();
    setup.fake.add_instance("inst_a", "dev", "vm");
    let out = run(
        &doctor(&setup.config_with_key_file(KEY, 0o600)),
        None,
        &[KEY],
    );
    assert_eq!(out.code, 1);
    assert!(
        out.text().contains("is not registered with the workspace"),
        "{}",
        out.text()
    );
}

#[test]
fn wrong_api_key_is_rejected() {
    let setup = Setup::new();
    let wrong = "sk_test_wrong";
    let out = run(
        &doctor(&setup.config_with_key_file(wrong, 0o600)),
        None,
        &[KEY, wrong],
    );
    assert_eq!(out.code, 1);
    assert!(out.text().contains("API key rejected"), "{}", out.text());
}

#[test]
fn key_file_with_wide_mode_is_refused_before_any_request() {
    let setup = Setup::new();
    let out = run(
        &doctor(&setup.config_with_key_file(KEY, 0o644)),
        None,
        &[KEY],
    );
    assert_eq!(out.code, 1);
    assert!(out.text().contains("0600"), "{}", out.text());
    assert!(setup.fake.requests().is_empty());
}

#[test]
fn key_from_environment_works() {
    let setup = Setup::new();
    setup.fake.add_instance("inst_a", "dev", "vm");
    let out = run(&doctor(&setup.config()), Some(KEY), &[KEY]);
    assert_eq!(out.code, 0, "{}", out.text());
}

#[test]
fn unknown_operation_and_future_protocol_fail() {
    let setup = Setup::new();
    let config = setup.config_with_key_file(KEY, 0o600);
    let unknown = json!({ "protocolVersion": 1, "operation": "frobnicate", "config": config });
    let out = run(&unknown, None, &[KEY]);
    assert_eq!(out.code, 1);
    assert!(out.stdout["error"].is_string());
    let future = json!({ "protocolVersion": 2, "operation": "doctor", "config": config });
    let out = run(&future, None, &[KEY]);
    assert_eq!(out.code, 1);
    assert!(out.stdout["error"].is_string());
}

#[test]
fn bearer_header_carries_key_and_paths_never_do() {
    let setup = Setup::new();
    setup.fake.add_instance("inst_a", "dev", "vm");
    let out = run(
        &doctor(&setup.config_with_key_file(KEY, 0o600)),
        None,
        &[KEY],
    );
    assert_eq!(out.code, 0, "{}", out.text());
    let requests = setup.fake.requests();
    assert!(!requests.is_empty());
    for request in &requests {
        assert_eq!(
            request.header("authorization"),
            Some(format!("Bearer {KEY}").as_str())
        );
        assert!(!request.path.contains(KEY), "key in path {}", request.path);
    }
}
