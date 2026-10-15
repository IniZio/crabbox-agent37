//! End-to-end tests of pause and resume against a fake agent37 API.
//!
//! Both operations change billable state, so they must stay scoped to the
//! configured owner and the exact lease named in the request.

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
use std::time::{Duration, Instant};

use fake_agent37::{expected_host_key, FakeAgent37};
use serde_json::{json, Value};

/// Key the fake accepts.
const KEY: &str = "sk_test_fake";

/// What the binary printed and returned.
struct Outcome {
    /// Process exit code.
    code: i32,
    /// Parsed stdout.
    stdout: Value,
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

/// A request for `operation` as owner alice, merged with `extra` fields.
fn request(fake: &FakeAgent37, operation: &str, extra: &Value) -> Value {
    let mut req = json!({
        "protocolVersion": 1,
        "operation": operation,
        "config": { "apiUrl": fake.url(), "owner": "alice", "readyTimeoutSeconds": 20 },
    });
    for (k, v) in extra.as_object().unwrap() {
        req[k] = v.clone();
    }
    req
}

/// Seeds a complete lease owned by `owner`.
fn add_lease(fake: &FakeAgent37, owner: &str, id: &str, status: &str, n: u32) {
    let lease_id = format!("cbx_{n:012}");
    let slug = format!("slug-{n}");
    fake.add_instance_with_metadata(
        id,
        &format!("crabbox-{owner}--name-{n}"),
        "vm",
        status,
        &[
            ("crabbox_owner", owner),
            ("crabbox_lease_id", &lease_id),
            ("crabbox_slug", &slug),
        ],
    );
}

/// The `lease` object a request carries for the instance seeded by `add_lease`.
fn lease_ref(id: &str, n: u32) -> Value {
    json!({
        "leaseId": format!("cbx_{n:012}"),
        "slug": format!("slug-{n}"),
        "name": format!("name-{n}"),
        "cloudId": id,
    })
}

/// Number of POSTs to `/v1/instances/{id}/{action}` the fake received.
fn posts(fake: &FakeAgent37, id: &str, action: &str) -> usize {
    let path = format!("/v1/instances/{id}/{action}");
    fake.requests()
        .iter()
        .filter(|r| r.method == "POST" && r.path == path)
        .count()
}

/// Number of stop and start POSTs of any instance.
fn state_changes(fake: &FakeAgent37) -> usize {
    fake.requests()
        .iter()
        .filter(|r| r.method == "POST" && (r.path.ends_with("/stop") || r.path.ends_with("/start")))
        .count()
}

/// Status string of instance `id`.
fn status(fake: &FakeAgent37, id: &str) -> String {
    fake.instance(id).unwrap()["status"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Error text of a failed outcome.
fn error_text(out: &Outcome) -> String {
    assert_eq!(out.code, 1, "{}", out.stdout);
    out.stdout["error"].to_string()
}

#[test]
fn pause_of_a_running_instance_stops_it_exactly_once() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, "alice", "vmrun", "running", 1);
    let out = run(&request(
        &fake,
        "pause",
        &json!({ "lease": lease_ref("vmrun", 1) }),
    ));
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.stdout["protocolVersion"], 1);
    assert!(out.stdout["message"].is_string(), "{}", out.stdout);
    assert_eq!(posts(&fake, "vmrun", "stop"), 1);
    assert_eq!(status(&fake, "vmrun"), "stopped");
}

#[test]
fn pause_of_a_stopped_instance_succeeds_without_state_changes() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, "alice", "vmstop", "stopped", 1);
    let out = run(&request(
        &fake,
        "pause",
        &json!({ "lease": lease_ref("vmstop", 1) }),
    ));
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(state_changes(&fake), 0);
}

#[test]
fn resume_of_a_stopped_instance_starts_it_and_returns_a_fresh_host_key() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, "alice", "vmstop", "stopped", 1);
    let out = run(&request(
        &fake,
        "resume",
        &json!({ "lease": lease_ref("vmstop", 1) }),
    ));
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(posts(&fake, "vmstop", "start"), 1);
    let lease = &out.stdout["lease"];
    assert_eq!(lease["status"], "ready");
    assert_eq!(lease["cloudId"], "vmstop");
    let key = lease["ssh"]["hostKey"].as_str().unwrap();
    assert_eq!(key, fake.current_host_key("vmstop"));
    assert_ne!(key, expected_host_key("vmstop"));
}

#[test]
fn resume_installs_a_missing_required_tool() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, "alice", "vmstop", "stopped", 1);
    fake.set_missing_tools(&["rsync"]);
    let out = run(&request(
        &fake,
        "resume",
        &json!({ "lease": lease_ref("vmstop", 1) }),
    ));
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.stdout["lease"]["status"], "ready");
    assert!(
        fake.missing_tools().is_empty(),
        "{:?}",
        fake.missing_tools()
    );
}

#[test]
fn resume_fails_when_a_required_tool_cannot_be_installed() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, "alice", "vmstop", "stopped", 1);
    fake.set_missing_tools(&["rsync"]);
    fake.set_tool_install_fails(true);
    let out = run(&request(
        &fake,
        "resume",
        &json!({ "lease": lease_ref("vmstop", 1) }),
    ));
    assert_ne!(out.code, 0);
    let error = error_text(&out);
    assert!(error.contains("lacks required tools: rsync"), "{error}");
}

#[test]
fn resume_with_nothing_missing_succeeds() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, "alice", "vmstop", "stopped", 1);
    let out = run(&request(
        &fake,
        "resume",
        &json!({ "lease": lease_ref("vmstop", 1) }),
    ));
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert!(fake.missing_tools().is_empty());
}

#[test]
fn resume_waits_out_a_cold_boot_where_ssh_is_not_ready() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, "alice", "vmstop", "stopped", 1);
    fake.set_exec_fails_after_start(3);
    let out = run(&request(
        &fake,
        "resume",
        &json!({ "lease": lease_ref("vmstop", 1) }),
    ));
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.stdout["lease"]["status"], "ready");
    assert_eq!(
        out.stdout["lease"]["ssh"]["hostKey"],
        fake.current_host_key("vmstop")
    );
    assert_eq!(posts(&fake, "vmstop", "start"), 1);
}

#[test]
fn resume_times_out_when_the_host_key_never_becomes_readable() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, "alice", "vmstop", "stopped", 1);
    fake.set_exec_fails(true);
    let mut req = request(&fake, "resume", &json!({ "lease": lease_ref("vmstop", 1) }));
    req["config"]["readyTimeoutSeconds"] = json!(2);
    let began = Instant::now();
    let out = run(&req);
    let error = error_text(&out).to_lowercase();
    assert!(
        error.contains("host key") || error.contains("not ready"),
        "{error}"
    );
    assert!(began.elapsed() < Duration::from_secs(15));
}

#[test]
fn resume_of_a_running_instance_does_not_start_it_again() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, "alice", "vmrun", "running", 1);
    let out = run(&request(
        &fake,
        "resume",
        &json!({ "lease": lease_ref("vmrun", 1) }),
    ));
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(posts(&fake, "vmrun", "start"), 0);
    assert_eq!(out.stdout["lease"]["status"], "ready");
    assert!(out.stdout["lease"]["ssh"]["hostKey"].is_string());
}

#[test]
fn both_operations_refuse_another_owners_instance() {
    // The second seed carries alice's name prefix but bob's owner metadata, so
    // only the owner check can reject it.
    for (name, owner_meta) in [
        ("crabbox-bob--name-1", "bob"),
        ("crabbox-alice--name-1", "bob"),
    ] {
        for (operation, status) in [("pause", "running"), ("resume", "stopped")] {
            let fake = FakeAgent37::start_with_key(KEY);
            fake.add_instance_with_metadata(
                "vmbob",
                name,
                "vm",
                status,
                &[
                    ("crabbox_owner", owner_meta),
                    ("crabbox_lease_id", "cbx_000000000001"),
                    ("crabbox_slug", "slug-1"),
                ],
            );
            let out = run(&request(
                &fake,
                operation,
                &json!({ "lease": lease_ref("vmbob", 1) }),
            ));
            let text = error_text(&out);
            if name.starts_with("crabbox-alice--") {
                assert!(
                    text.contains("belongs to another owner"),
                    "{operation}: {text}"
                );
            }
            assert_eq!(state_changes(&fake), 0, "{name} {operation}");
            assert_eq!(self::status(&fake, "vmbob"), status, "{name} {operation}");
        }
    }
}

#[test]
fn both_operations_refuse_a_cloud_id_that_differs_from_the_expected_one() {
    for (operation, status) in [("pause", "running"), ("resume", "stopped")] {
        let fake = FakeAgent37::start_with_key(KEY);
        add_lease(&fake, "alice", "vmalice", status, 1);
        let extra = json!({
            "lease": lease_ref("vmalice", 1),
            "expected": { "cloudId": "vmother" },
        });
        let out = run(&request(&fake, operation, &extra));
        error_text(&out);
        assert_eq!(state_changes(&fake), 0, "{operation}");
        assert_eq!(self::status(&fake, "vmalice"), status, "{operation}");
    }
}

#[test]
fn both_operations_refuse_a_lease_id_that_does_not_match_the_instance() {
    for (operation, status) in [("pause", "running"), ("resume", "stopped")] {
        let fake = FakeAgent37::start_with_key(KEY);
        add_lease(&fake, "alice", "vmalice", status, 1);
        let mut lease = lease_ref("vmalice", 1);
        lease["leaseId"] = json!("cbx_000000000099");
        let out = run(&request(&fake, operation, &json!({ "lease": lease })));
        error_text(&out);
        assert_eq!(state_changes(&fake), 0, "{operation}");
        assert_eq!(self::status(&fake, "vmalice"), status, "{operation}");
    }
}

#[test]
fn acquire_of_a_stopped_instance_fails_and_names_the_state() {
    let fake = FakeAgent37::start_with_key(KEY);
    fake.add_instance_with_metadata(
        "vmstop",
        "crabbox-alice--crabbox-blue-crab-000000000001",
        "vm",
        "stopped",
        &[
            ("crabbox_owner", "alice"),
            ("crabbox_lease_id", "cbx_000000000001"),
            ("crabbox_slug", "blue-crab"),
        ],
    );
    let acquire = request(
        &fake,
        "acquire",
        &json!({ "desired": {
            "leaseId": "cbx_000000000001",
            "slug": "blue-crab",
            "name": "crabbox-blue-crab-000000000001",
        } }),
    );
    let out = run(&acquire);
    assert!(error_text(&out).contains("stopped"), "{}", out.stdout);
    assert_eq!(state_changes(&fake), 0);
}
