//! End-to-end tests of the `acquire` operation against a fake agent37 API.

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

/// What the binary printed and returned.
struct Outcome {
    /// Process exit code.
    code: i32,
    /// Parsed stdout.
    stdout: Value,
    /// Raw stdout, for failure messages.
    raw: String,
}

impl Outcome {
    /// The `message` or `error` text.
    fn text(&self) -> &str {
        self.stdout["message"]
            .as_str()
            .or_else(|| self.stdout["error"].as_str())
            .unwrap_or(&self.raw)
    }

    /// The `lease` object of a successful response.
    fn lease(&self) -> &Value {
        assert_eq!(self.code, 0, "acquire failed: {}", self.raw);
        &self.stdout["lease"]
    }
}

/// Runs the binary with `request` on stdin; the API key must not reach stdout or stderr.
fn run(request: &Value, env_key: Option<&str>) -> Outcome {
    let mut command = Command::new(env!("CARGO_BIN_EXE_crabbox-agent37"));
    command
        .env_remove("AGENT37_API_KEY")
        .env_remove("AGENT37_KEY_FILE")
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
    let raw = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!raw.contains(KEY), "key leaked to stdout: {raw}");
    assert!(!stderr.contains(KEY), "key leaked to stderr: {stderr}");
    Outcome {
        code: output.status.code().unwrap(),
        stdout: serde_json::from_str(&raw).unwrap_or_else(|e| panic!("bad stdout {raw:?}: {e}")),
        raw,
    }
}

/// Runs `acquire` authenticated through `AGENT37_API_KEY`.
fn run_env(request: &Value) -> Outcome {
    run(request, Some(KEY))
}

/// The binary under test, resolved the way the implementation resolves it.
fn exe() -> String {
    std::fs::canonicalize(env!("CARGO_BIN_EXE_crabbox-agent37"))
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned()
}

/// Config pointing at `fake`, owned by alice.
fn config(fake: &FakeAgent37) -> Value {
    json!({ "apiUrl": fake.url(), "owner": "alice" })
}

/// An `acquire` request with the standard desired lease and `config`.
fn acquire_with(config: &Value) -> Value {
    json!({
        "protocolVersion": 1,
        "operation": "acquire",
        "config": config,
        "desired": {
            "leaseId": "cbx_000000000001",
            "slug": "blue-crab",
            "name": "crabbox-blue-crab-000000000001",
        },
    })
}

/// An `acquire` request with the default config.
fn acquire(fake: &FakeAgent37) -> Value {
    acquire_with(&config(fake))
}

/// Writes a 0600 file holding the key inside `dir`.
fn key_file_in(dir: &TempDir) -> PathBuf {
    let path = dir.path().join("api_key");
    std::fs::write(&path, format!("{KEY}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

/// Methods and paths the fake saw, as `"METHOD /path"`.
fn seen(fake: &FakeAgent37) -> Vec<String> {
    fake.requests()
        .iter()
        .map(|r| format!("{} {}", r.method, r.path))
        .collect()
}

/// Number of recorded requests starting with `prefix`.
fn count(fake: &FakeAgent37, prefix: &str) -> usize {
    seen(fake).iter().filter(|r| r.starts_with(prefix)).count()
}

/// The parsed body of the first `POST /v1/instances`.
fn create_body(fake: &FakeAgent37) -> Value {
    let request = fake
        .requests()
        .into_iter()
        .find(|r| r.method == "POST" && r.path == "/v1/instances")
        .expect("no create request");
    serde_json::from_str(&request.body).unwrap()
}

#[test]
fn creates_vm_with_default_template_region_and_resources() {
    let fake = FakeAgent37::start_with_key(KEY);
    let out = run_env(&acquire(&fake));
    out.lease();
    let body = create_body(&fake);
    assert_eq!(body["type"], "vm");
    assert_eq!(body["template"], "agent37-ubuntu");
    assert_eq!(body["region"], "us");
    assert_eq!(body["resources"]["cpu"], 2);
    assert_eq!(body["resources"]["memory"], 4);
    assert_eq!(body["auto_sleep"], false);
    assert_eq!(
        body["name"],
        "crabbox-alice--crabbox-blue-crab-000000000001"
    );
    assert_eq!(body["metadata"]["crabbox_owner"], "alice");
}

#[test]
fn resources_follow_the_requested_class() {
    for (class, cpu, memory) in [
        ("standard", 2, 4),
        ("fast", 4, 8),
        ("large", 8, 16),
        ("beast", 16, 32),
    ] {
        let fake = FakeAgent37::start_with_key(KEY);
        let mut request = acquire(&fake);
        request["desired"]["class"] = json!(class);
        run_env(&request).lease();
        let body = create_body(&fake);
        assert_eq!(body["resources"]["cpu"], cpu, "class {class}");
        assert_eq!(body["resources"]["memory"], memory, "class {class}");
    }
}

#[test]
fn configured_region_is_honoured() {
    let fake = FakeAgent37::start_with_key(KEY);
    let mut cfg = config(&fake);
    cfg["region"] = json!("eu");
    run_env(&acquire_with(&cfg)).lease();
    assert_eq!(create_body(&fake)["region"], "eu");
}

#[test]
fn unknown_class_fails_without_creating() {
    let fake = FakeAgent37::start_with_key(KEY);
    let mut request = acquire(&fake);
    request["desired"]["class"] = json!("gigantic");
    let out = run_env(&request);
    assert_eq!(out.code, 1, "{}", out.raw);
    assert_eq!(count(&fake, "POST /v1/instances"), 0);
    assert!(fake.instances().is_empty());
}

#[test]
fn create_body_never_carries_env_and_no_managed_or_budget_call() {
    let fake = FakeAgent37::start_with_key(KEY);
    run_env(&acquire(&fake)).lease();
    let body = create_body(&fake);
    assert!(body.get("env").is_none(), "{body}");
    assert!(body.get("budget").is_none(), "{body}");
    for request in seen(&fake) {
        assert!(!request.contains("managed"), "{request}");
        assert!(!request.contains("budget"), "{request}");
    }
}

#[test]
fn repeated_acquire_returns_the_same_instance_with_one_create() {
    let fake = FakeAgent37::start_with_key(KEY);
    let request = acquire(&fake);
    let first = run_env(&request);
    let second = run_env(&request);
    assert_eq!(first.lease()["cloudId"], second.lease()["cloudId"]);
    let creates = seen(&fake)
        .iter()
        .filter(|r| *r == "POST /v1/instances")
        .count();
    assert_eq!(creates, 1);
    assert_eq!(fake.instances().len(), 1);
}

#[test]
fn response_describes_the_lease_and_ssh_endpoint() {
    let fake = FakeAgent37::start_with_key(KEY);
    let out = run_env(&acquire(&fake));
    let lease = out.lease();
    let created = fake.instances();
    let id = created[0]["id"].as_str().unwrap();
    assert_eq!(lease["leaseId"], "cbx_000000000001");
    assert_eq!(lease["slug"], "blue-crab");
    assert_eq!(lease["name"], "crabbox-blue-crab-000000000001");
    assert_eq!(lease["cloudId"], id);
    let ssh = &lease["ssh"];
    assert_eq!(ssh["user"], "root");
    assert_eq!(ssh["host"], format!("{id}.agent37.app"));
    assert_eq!(ssh["port"], "22");
    let ready = ssh["readyCheck"].as_str().unwrap();
    assert!(["bash", "git", "rsync", "tar"]
        .iter()
        .all(|t| ready.contains(t)));
    assert_eq!(ssh["hostKey"], fake_agent37::expected_host_key(id));
}

#[test]
fn proxy_command_without_key_file_runs_tunnel_directly() {
    let fake = FakeAgent37::start_with_key(KEY);
    let out = run_env(&acquire(&fake));
    assert_eq!(
        out.lease()["ssh"]["proxyCommand"],
        format!("{} tunnel %h", exe())
    );
}

#[test]
fn proxy_command_with_key_file_passes_its_path_through_env() {
    let fake = FakeAgent37::start_with_key(KEY);
    let dir = TempDir::new().unwrap();
    let key_file = key_file_in(&dir);
    let mut cfg = config(&fake);
    cfg["keyFile"] = json!(key_file.to_str().unwrap());
    let out = run(&acquire_with(&cfg), None);
    assert_eq!(
        out.lease()["ssh"]["proxyCommand"],
        format!(
            "env AGENT37_KEY_FILE={} {} tunnel %h",
            key_file.to_str().unwrap(),
            exe()
        )
    );
}

#[test]
fn proxy_command_single_quotes_a_key_file_path_with_spaces() {
    let fake = FakeAgent37::start_with_key(KEY);
    let dir = tempfile::Builder::new()
        .prefix("key dir ")
        .tempdir()
        .unwrap();
    let key_file = key_file_in(&dir);
    let mut cfg = config(&fake);
    cfg["keyFile"] = json!(key_file.to_str().unwrap());
    let out = run(&acquire_with(&cfg), None);
    assert_eq!(
        out.lease()["ssh"]["proxyCommand"],
        format!(
            "env AGENT37_KEY_FILE='{}' {} tunnel %h",
            key_file.to_str().unwrap(),
            exe()
        )
    );
}

#[test]
fn waits_for_a_provisioning_instance_to_become_running() {
    let fake = FakeAgent37::start_with_key(KEY);
    fake.set_create_status("provisioning");
    fake.set_polls_until_running(Some(2));
    let out = run_env(&acquire(&fake));
    out.lease();
    assert!(count(&fake, "GET /v1/instances/") >= 2, "{:?}", seen(&fake));
}

#[test]
fn deadline_deletes_an_instance_that_never_becomes_running() {
    let fake = FakeAgent37::start_with_key(KEY);
    fake.set_create_status("provisioning");
    fake.set_polls_until_running(None);
    let mut cfg = config(&fake);
    cfg["readyTimeoutSeconds"] = json!(1);
    let out = run_env(&acquire_with(&cfg));
    assert_eq!(out.code, 1, "{}", out.raw);
    assert_eq!(
        count(&fake, "DELETE /v1/instances/"),
        1,
        "{:?}",
        seen(&fake)
    );
    assert!(fake.instances().is_empty());
}

#[test]
fn failed_acquire_deletes_a_created_instance_even_with_keep() {
    let fake = FakeAgent37::start_with_key(KEY);
    fake.set_create_status("provisioning");
    fake.set_polls_until_running(None);
    let mut cfg = config(&fake);
    cfg["readyTimeoutSeconds"] = json!(1);
    let mut request = acquire_with(&cfg);
    request["keep"] = json!(true);
    let out = run_env(&request);
    assert_eq!(out.code, 1, "{}", out.raw);
    assert!(out.raw.contains("deleted"), "{}", out.raw);
    assert_eq!(
        count(&fake, "DELETE /v1/instances/"),
        1,
        "{:?}",
        seen(&fake)
    );
    assert!(fake.instances().is_empty());
}

#[test]
fn failed_acquire_never_deletes_a_pre_existing_instance() {
    let fake = FakeAgent37::start_with_key(KEY);
    let mut request = acquire(&fake);
    request["keep"] = json!(true);
    assert_eq!(run_env(&request).code, 0);
    fake.set_exec_fails(true);
    let mut cfg = config(&fake);
    cfg["readyTimeoutSeconds"] = json!(1);
    let mut request = acquire_with(&cfg);
    request["keep"] = json!(true);
    let out = run_env(&request);
    assert_eq!(out.code, 1, "{}", out.raw);
    assert_eq!(count(&fake, "DELETE "), 0, "{:?}", seen(&fake));
    assert_eq!(fake.instances().len(), 1);
}

#[test]
fn exec_failure_deletes_the_instance() {
    let fake = FakeAgent37::start_with_key(KEY);
    fake.set_exec_fails(true);
    let mut cfg = config(&fake);
    cfg["readyTimeoutSeconds"] = json!(1);
    let out = run_env(&acquire_with(&cfg));
    assert_eq!(out.code, 1, "{}", out.raw);
    assert_eq!(
        count(&fake, "DELETE /v1/instances/"),
        1,
        "{:?}",
        seen(&fake)
    );
    assert!(fake.instances().is_empty());
}

#[test]
fn missing_owner_fails_without_creating() {
    let fake = FakeAgent37::start_with_key(KEY);
    let out = run_env(&acquire_with(&json!({ "apiUrl": fake.url() })));
    assert_eq!(out.code, 1, "{}", out.raw);
    assert!(out.text().contains("owner"), "{}", out.text());
    assert_eq!(count(&fake, "POST /v1/instances"), 0);
}

#[test]
fn unfunded_workspace_error_names_the_cause() {
    let fake = FakeAgent37::start_with_key(KEY);
    fake.set_vm_tier_limited(true);
    let out = run_env(&acquire(&fake));
    assert_eq!(out.code, 1, "{}", out.raw);
    assert!(
        out.text().contains("workspace not funded for VM instances"),
        "{}",
        out.text()
    );
}

/// Exec requests whose command starts with `prefix`.
fn exec_count(fake: &FakeAgent37, prefix: &str) -> usize {
    fake.requests()
        .iter()
        .filter(|r| r.path.ends_with("/exec"))
        .filter(|r| {
            let body: Value = serde_json::from_str(&r.body).unwrap();
            body["command"]
                .as_str()
                .unwrap_or_default()
                .starts_with(prefix)
        })
        .count()
}

#[test]
fn acquire_with_all_tools_present_probes_once() {
    let fake = FakeAgent37::start_with_key(KEY);
    run_env(&acquire(&fake)).lease();
    assert_eq!(count(&fake, "POST /v1/instances/vm00000001/exec"), 2);
    assert_eq!(exec_count(&fake, ": crabbox-tools-probe; "), 1);
    assert_eq!(exec_count(&fake, ": crabbox-tools-install; "), 0);
}

#[test]
fn acquire_installs_a_missing_tool() {
    let fake = FakeAgent37::start_with_key(KEY);
    fake.set_missing_tools(&["rsync"]);
    run_env(&acquire(&fake)).lease();
    assert_eq!(exec_count(&fake, ": crabbox-tools-install; "), 1);
    assert!(fake.missing_tools().is_empty());
}

#[test]
fn failed_tool_install_fails_acquire_and_deletes_the_instance() {
    let fake = FakeAgent37::start_with_key(KEY);
    fake.set_missing_tools(&["rsync"]);
    fake.set_tool_install_fails(true);
    let out = run_env(&acquire(&fake));
    assert_eq!(out.code, 1, "{}", out.raw);
    assert!(out.text().contains("rsync"), "{}", out.text());
    assert_eq!(
        count(&fake, "DELETE /v1/instances/"),
        1,
        "{:?}",
        seen(&fake)
    );
    assert!(fake.instances().is_empty());
}

#[test]
fn missing_apt_get_fails_acquire_naming_the_tool() {
    let fake = FakeAgent37::start_with_key(KEY);
    fake.set_missing_tools(&["rsync"]);
    fake.set_no_apt(true);
    let out = run_env(&acquire(&fake));
    assert_eq!(out.code, 1, "{}", out.raw);
    assert!(out.text().contains("rsync"), "{}", out.text());
}
