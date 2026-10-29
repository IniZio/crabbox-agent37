//! End-to-end tests of the `resolve` operation against a fake agent37 API.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail loudly on setup errors"
)]

#[path = "support/fake_agent37.rs"]
mod fake_agent37;

use std::process::{Command, Stdio};

use fake_agent37::{expected_host_key, FakeAgent37};
use serde_json::{json, Value};

/// Key the fake accepts.
const KEY: &str = "sk_test_fake";
/// Id of the instance seeded by `seeded`.
const ID: &str = "vmabc12345";
/// Suffix of the seeded instance name after the owner prefix.
const SHORT_NAME: &str = "crabbox-blue-crab-000000000001";
/// Full name of the seeded instance on agent37.
const FULL_NAME: &str = "crabbox-alice--crabbox-blue-crab-000000000001";

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

    /// The returned lease; fails the test when the call did not succeed.
    fn lease(&self) -> &Value {
        assert_eq!(self.code, 0, "{}", self.text());
        assert_eq!(self.stdout["protocolVersion"], 1);
        &self.stdout["lease"]
    }
}

/// Runs the binary with `request` on stdin.
fn run(request: &Value) -> Outcome {
    let mut child = Command::new(env!("CARGO_BIN_EXE_crabbox-agent37"))
        .env("AGENT37_API_KEY", KEY)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(
        &mut child.stdin.take().unwrap(),
        request.to_string().as_bytes(),
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    Outcome {
        code: output.status.code().unwrap(),
        stdout: serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("bad stdout {stdout:?}: {e}")),
    }
}

/// A fake server holding one running instance named `FULL_NAME`.
fn seeded() -> FakeAgent37 {
    let fake = FakeAgent37::start_with_key(KEY);
    fake.add_instance_with_status(ID, FULL_NAME, "vm", "running");
    fake
}

/// A `resolve` request against `fake`.
fn resolve(fake: &FakeAgent37, desired: Option<&Value>, expected: &Value) -> Value {
    let mut request = json!({
        "protocolVersion": 1,
        "operation": "resolve",
        "config": { "apiUrl": fake.url(), "owner": "alice" },
        "expected": expected,
    });
    if let Some(desired) = desired {
        request["desired"] = desired.clone();
    }
    request
}

/// The `desired` object for the seeded instance.
fn desired() -> Value {
    json!({ "leaseId": "cbx_000000000001", "slug": "blue-crab", "name": SHORT_NAME })
}

/// Methods and paths the fake saw, as `"METHOD /path"`.
fn seen(fake: &FakeAgent37) -> Vec<String> {
    fake.requests()
        .iter()
        .map(|r| format!("{} {}", r.method, r.path))
        .collect()
}

/// Resolve must never create, delete or exec anything on agent37 except host key reads.
fn assert_sent_no_creates_or_deletes(fake: &FakeAgent37) {
    let writes: Vec<String> = seen(fake)
        .into_iter()
        .filter(|r| r == "POST /v1/instances" || r.starts_with("DELETE "))
        .collect();
    assert!(writes.is_empty(), "resolve sent writes: {writes:?}");
}

#[test]
fn expected_fields_are_repeated_exactly_with_name_host_key_and_host() {
    let fake = seeded();
    let expected = json!({
        "leaseId": "cbx_000000000001",
        "slug": "blue-crab",
        "cloudId": ID,
    });
    let out = run(&resolve(&fake, Some(&desired()), &expected));
    let lease = out.lease();
    assert_eq!(lease["leaseId"], "cbx_000000000001");
    assert_eq!(lease["slug"], "blue-crab");
    assert_eq!(lease["cloudId"], ID);
    assert_eq!(lease["name"], SHORT_NAME);
    assert_eq!(lease["status"], "ready");
    assert_eq!(lease["ssh"]["user"], "root");
    assert_eq!(lease["ssh"]["host"], format!("{ID}.agent37.app"));
    assert_eq!(lease["ssh"]["hostKey"], expected_host_key(ID));
    assert_sent_no_creates_or_deletes(&fake);
}

#[test]
fn running_instance_resolves_with_gets_and_one_host_key_exec() {
    let fake = seeded();
    let out = run(&resolve(&fake, Some(&desired()), &json!({ "cloudId": ID })));
    assert_eq!(out.lease()["status"], "ready");
    let non_get: Vec<String> = seen(&fake)
        .into_iter()
        .filter(|r| !r.starts_with("GET "))
        .collect();
    assert_eq!(non_get, [format!("POST /v1/instances/{ID}/exec")]);
}

#[test]
fn running_instance_whose_host_key_cannot_be_read_fails_to_resolve() {
    let fake = seeded();
    fake.set_exec_fails(true);
    let out = run(&resolve(&fake, Some(&desired()), &json!({ "cloudId": ID })));
    assert_ne!(out.code, 0, "resolved without a host key: {}", out.stdout);
    assert!(out.text().contains("host key"), "{}", out.text());
}

#[test]
fn release_only_resolve_of_a_running_instance_skips_the_host_key() {
    let fake = seeded();
    let mut request = resolve(&fake, Some(&desired()), &json!({ "cloudId": ID }));
    request["releaseOnly"] = json!(true);
    let out = run(&request);
    let lease = out.lease();
    assert_eq!(lease["status"], "ready");
    assert!(lease["ssh"].get("hostKey").is_none(), "{}", lease["ssh"]);
    let non_get: Vec<String> = seen(&fake)
        .into_iter()
        .filter(|r| !r.starts_with("GET "))
        .collect();
    assert!(non_get.is_empty(), "non-GET requests: {non_get:?}");
}

#[test]
fn only_expected_derives_name_by_stripping_the_owner_prefix() {
    let fake = seeded();
    let expected = json!({
        "leaseId": "cbx_000000000001",
        "slug": "blue-crab",
        "cloudId": ID,
    });
    let out = run(&resolve(&fake, None, &expected));
    let lease = out.lease();
    assert_eq!(lease["leaseId"], "cbx_000000000001");
    assert_eq!(lease["slug"], "blue-crab");
    assert_eq!(lease["name"], SHORT_NAME);
    assert_eq!(lease["cloudId"], ID);
    assert_eq!(lease["ssh"]["hostKey"], expected_host_key(ID));
    assert_sent_no_creates_or_deletes(&fake);
}

#[test]
fn desired_without_cloud_id_finds_the_instance_by_name() {
    let fake = seeded();
    let out = run(&resolve(&fake, Some(&desired()), &json!({})));
    let lease = out.lease();
    assert_eq!(lease["cloudId"], ID);
    assert_eq!(lease["leaseId"], "cbx_000000000001");
    assert_eq!(lease["slug"], "blue-crab");
    assert_eq!(lease["name"], SHORT_NAME);
    assert_eq!(lease["status"], "ready");
    assert_eq!(lease["ssh"]["host"], format!("{ID}.agent37.app"));
    assert_eq!(lease["ssh"]["hostKey"], expected_host_key(ID));
    assert_sent_no_creates_or_deletes(&fake);
}

#[test]
fn cloud_id_differing_from_the_instance_named_by_desired_is_rejected() {
    let fake = seeded();
    fake.add_instance_with_status(
        "vmother999",
        "crabbox-alice--crabbox-other-000000000002",
        "vm",
        "running",
    );
    let expected = json!({ "cloudId": "vmother999" });
    let out = run(&resolve(&fake, Some(&desired()), &expected));
    assert_eq!(out.code, 1, "{}", out.stdout);
    let text = out.text();
    assert!(text.contains(ID), "{text}");
    assert!(text.contains("vmother999"), "{text}");
    assert_sent_no_creates_or_deletes(&fake);
}

#[test]
fn cloud_id_of_a_teammates_instance_is_rejected() {
    let fake = seeded();
    fake.add_instance_with_status(
        "vmbob00001",
        "crabbox-bob--crabbox-blue-crab-000000000001",
        "vm",
        "running",
    );
    let expected = json!({ "cloudId": "vmbob00001" });
    let out = run(&resolve(&fake, None, &expected));
    assert_eq!(out.code, 1, "{}", out.stdout);
    assert!(out.stdout.get("lease").is_none(), "{}", out.stdout);
    assert_sent_no_creates_or_deletes(&fake);
}

#[test]
fn unknown_cloud_id_reports_not_found() {
    let fake = seeded();
    let expected = json!({ "cloudId": "vmmissing1" });
    let out = run(&resolve(&fake, None, &expected));
    assert_eq!(out.code, 1, "{}", out.stdout);
    assert!(out.text().contains("not found"), "{}", out.text());
    assert_sent_no_creates_or_deletes(&fake);
}

#[test]
fn stopped_instance_is_reported_stopped_without_host_key_or_exec() {
    let fake = FakeAgent37::start_with_key(KEY);
    fake.add_instance_with_status(ID, FULL_NAME, "vm", "stopped");
    let expected = json!({ "cloudId": ID });
    let out = run(&resolve(&fake, Some(&desired()), &expected));
    let lease = out.lease();
    assert_eq!(lease["status"], "stopped");
    assert_eq!(lease["cloudId"], ID);
    assert!(
        lease["ssh"].get("hostKey").is_none(),
        "unexpected hostKey in {}",
        lease["ssh"]
    );
    let execs: Vec<String> = seen(&fake)
        .into_iter()
        .filter(|r| r.ends_with("/exec"))
        .collect();
    assert!(
        execs.is_empty(),
        "exec sent for stopped instance: {execs:?}"
    );
    assert_sent_no_creates_or_deletes(&fake);
}

#[test]
fn expected_lease_id_disagreeing_with_desired_is_rejected() {
    let fake = seeded();
    let expected = json!({ "leaseId": "cbx_999999999999", "cloudId": ID });
    let out = run(&resolve(&fake, Some(&desired()), &expected));
    assert_eq!(out.code, 1, "{}", out.stdout);
    assert!(out.stdout.get("lease").is_none(), "{}", out.stdout);
    assert_sent_no_creates_or_deletes(&fake);
}

#[test]
fn bare_id_without_cloud_id_or_desired_reports_not_found_without_api_calls() {
    let fake = seeded();
    let out = run(&resolve(&fake, None, &json!({})));
    assert_eq!(out.code, 1, "{}", out.stdout);
    assert!(out.text().ends_with("not found"), "{}", out.text());
    assert!(seen(&fake).is_empty(), "{:?}", seen(&fake));
}

#[test]
fn desired_without_cloud_id_and_no_such_instance_reports_not_found() {
    let fake = FakeAgent37::start_with_key(KEY);
    let out = run(&resolve(&fake, Some(&desired()), &json!({})));
    assert_eq!(out.code, 1, "{}", out.stdout);
    assert!(out.text().ends_with("not found"), "{}", out.text());
    assert_sent_no_creates_or_deletes(&fake);
}

#[test]
fn rejected_api_key_is_not_reported_as_not_found() {
    let fake = FakeAgent37::start_with_key("sk_test_other");
    fake.add_instance_with_status(ID, FULL_NAME, "vm", "running");
    let out = run(&resolve(&fake, Some(&desired()), &json!({})));
    assert_eq!(out.code, 1, "{}", out.stdout);
    assert!(!out.text().contains("not found"), "{}", out.text());
}
