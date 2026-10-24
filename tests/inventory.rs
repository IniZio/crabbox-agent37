//! End-to-end tests of list, release, touch and cleanup against a fake agent37 API.
//!
//! Teammates share one agent37 workspace, so every operation must stay
//! scoped to the configured owner.

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

use fake_agent37::FakeAgent37;
use serde_json::{json, Value};

/// Key the fake accepts.
const KEY: &str = "sk_test_fake";

/// What the binary printed and returned.
struct Outcome {
    /// Process exit code.
    code: i32,
    /// Parsed stdout.
    stdout: Value,
    /// Diagnostics.
    stderr: String,
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
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// A request for `operation` as owner alice, merged with `extra` fields.
fn request(fake: &FakeAgent37, operation: &str, extra: &Value) -> Value {
    let mut req = json!({
        "protocolVersion": 1,
        "operation": operation,
        "config": { "apiUrl": fake.url(), "owner": "alice" },
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

/// Seeds an instance owned by `owner` with no lease id or slug.
fn add_orphan(fake: &FakeAgent37, owner: &str, id: &str) {
    fake.add_instance_with_metadata(
        id,
        &format!("crabbox-{owner}--orphan"),
        "vm",
        "running",
        &[("crabbox_owner", owner)],
    );
}

/// The `lease` object a release request carries for the instance seeded by `add_lease`.
fn lease_ref(id: &str, n: u32) -> Value {
    json!({
        "leaseId": format!("cbx_{n:012}"),
        "slug": format!("slug-{n}"),
        "name": format!("name-{n}"),
        "cloudId": id,
    })
}

/// Paths of every DELETE the fake received.
fn deletes(fake: &FakeAgent37) -> Vec<String> {
    fake.requests()
        .into_iter()
        .filter(|r| r.method == "DELETE")
        .map(|r| r.path)
        .collect()
}

/// Whether the fake still holds instance `id`.
fn exists(fake: &FakeAgent37, id: &str) -> bool {
    fake.instance(id).is_some()
}

/// Seeds alice and bob orphans plus a complete lease each.
fn cleanup_fixture() -> FakeAgent37 {
    let fake = FakeAgent37::start_with_key(KEY);
    add_orphan(&fake, "alice", "vmaliceorphan");
    add_lease(&fake, "alice", "vmalicelease", "running", 1);
    add_orphan(&fake, "bob", "vmboborphan");
    add_lease(&fake, "bob", "vmboblease", "running", 2);
    fake
}

/// Asserts `lease` is the one `add_lease` seeds for `n`.
fn assert_lease(lease: &Value, n: u32, id: &str, status: &str) {
    assert_eq!(lease["leaseId"], format!("cbx_{n:012}"));
    assert_eq!(lease["slug"], format!("slug-{n}"));
    assert_eq!(lease["name"], format!("name-{n}"));
    assert_eq!(lease["cloudId"], id);
    assert_eq!(lease["status"], status);
}

#[test]
fn list_of_an_empty_workspace_returns_an_empty_array() {
    let fake = FakeAgent37::start_with_key(KEY);
    let out = run(&request(&fake, "list", &json!({})));
    assert_eq!(out.code, 0);
    assert_eq!(out.stdout, json!({ "protocolVersion": 1, "leases": [] }));
    assert!(out.stdout["leases"].is_array());
}

#[test]
fn list_returns_only_the_owners_complete_leases() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, "alice", "vmarun", "running", 1);
    add_lease(&fake, "alice", "vmastop", "stopped", 2);
    add_lease(&fake, "bob", "vmbrun", "running", 3);
    fake.add_instance_with_metadata(
        "vmwrongowner",
        "crabbox-alice--x",
        "vm",
        "running",
        &[
            ("crabbox_owner", "bob"),
            ("crabbox_lease_id", "cbx_000000000009"),
            ("crabbox_slug", "slug-9"),
        ],
    );
    fake.add_instance_with_status("vmnometa", "crabbox-alice--bare", "vm", "running");

    let out = run(&request(&fake, "list", &json!({})));
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.stdout["protocolVersion"], 1);
    let mut leases = out.stdout["leases"].as_array().unwrap().clone();
    leases.sort_by_key(|l| l["cloudId"].as_str().unwrap().to_owned());
    assert_eq!(leases.len(), 2, "{leases:?}");
    assert_lease(&leases[0], 1, "vmarun", "ready");
    assert_lease(&leases[1], 2, "vmastop", "stopped");
}

#[test]
fn release_deletes_exactly_the_named_instance() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, "alice", "vminsta", "running", 1);
    add_lease(&fake, "alice", "vminstb", "running", 2);
    let out = run(&request(
        &fake,
        "release",
        &json!({ "lease": lease_ref("vminsta", 1) }),
    ));
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.stdout, json!({ "protocolVersion": 1 }));
    assert_eq!(deletes(&fake), vec!["/v1/instances/vminsta".to_owned()]);
    assert!(!exists(&fake, "vminsta"));
    assert!(exists(&fake, "vminstb"));
}

#[test]
fn release_of_an_already_deleted_instance_succeeds() {
    let fake = FakeAgent37::start_with_key(KEY);
    let out = run(&request(
        &fake,
        "release",
        &json!({ "lease": lease_ref("gone", 1) }),
    ));
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.stdout, json!({ "protocolVersion": 1 }));
}

#[test]
fn release_refuses_a_teammates_instance() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, "bob", "vmbobinst", "running", 1);
    let out = run(&request(
        &fake,
        "release",
        &json!({ "lease": lease_ref("vmbobinst", 1) }),
    ));
    assert_ne!(out.code, 0);
    assert!(out.stdout.get("error").is_some(), "{}", out.stdout);
    assert!(deletes(&fake).is_empty(), "{:?}", deletes(&fake));
    assert!(exists(&fake, "vmbobinst"));
}

#[test]
fn release_with_mismatched_expected_cloud_id_sends_no_delete() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, "alice", "vminsta", "running", 1);
    add_lease(&fake, "alice", "vminstb", "running", 2);
    let out = run(&request(
        &fake,
        "release",
        &json!({
            "lease": lease_ref("vminsta", 1),
            "expected": { "cloudId": "vminstb" },
        }),
    ));
    assert_ne!(out.code, 0);
    assert!(out.stdout.get("error").is_some(), "{}", out.stdout);
    assert!(deletes(&fake).is_empty(), "{:?}", deletes(&fake));
    assert!(exists(&fake, "vminsta"));
    assert!(exists(&fake, "vminstb"));
}

#[test]
fn touch_succeeds_without_calling_agent37() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, "alice", "vminsta", "running", 1);
    let out = run(&request(
        &fake,
        "touch",
        &json!({ "lease": lease_ref("vminsta", 1) }),
    ));
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.stdout, json!({ "protocolVersion": 1 }));
    assert!(fake.requests().is_empty(), "{:?}", fake.requests());
}

#[test]
fn cleanup_never_deletes_a_teammates_instance() {
    let fake = cleanup_fixture();
    let out = run(&request(&fake, "cleanup", &json!({})));
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.stdout["protocolVersion"], 1);
    assert!(out.stdout["message"].is_string(), "{}", out.stdout);
    assert!(!exists(&fake, "vmaliceorphan"));
    assert!(exists(&fake, "vmalicelease"));
    assert!(exists(&fake, "vmboborphan"));
    assert!(exists(&fake, "vmboblease"));
    assert_eq!(
        deletes(&fake),
        vec!["/v1/instances/vmaliceorphan".to_owned()]
    );
}

#[test]
fn cleanup_dry_run_deletes_nothing() {
    let fake = cleanup_fixture();
    let out = run(&request(&fake, "cleanup", &json!({ "dryRun": true })));
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.stdout["protocolVersion"], 1);
    assert!(deletes(&fake).is_empty(), "{:?}", deletes(&fake));
    assert_eq!(fake.instances().len(), 4);
}

#[test]
fn resolve_release_only_skips_exec_for_a_stopped_instance() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, "alice", "vminsta", "stopped", 1);
    let out = run(&request(
        &fake,
        "resolve",
        &json!({
            "releaseOnly": true,
            "expected": {
                "cloudId": "vminsta",
                "leaseId": "cbx_000000000001",
                "slug": "slug-1",
            },
            "desired": {
                "leaseId": "cbx_000000000001",
                "slug": "slug-1",
                "name": "name-1",
            },
        }),
    ));
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.stdout["lease"]["status"], "stopped");
    let execs: Vec<String> = fake
        .requests()
        .into_iter()
        .map(|r| r.path)
        .filter(|p| p.ends_with("/exec"))
        .collect();
    assert!(execs.is_empty(), "exec sent: {execs:?}");
}

#[test]
fn list_and_resolve_use_the_running_lease_box_of_two_with_one_name() {
    let fake = FakeAgent37::start_with_key(KEY);
    for (id, role, status) in [
        ("vmstale", "lease", "stopped"),
        ("vmlive", "lease", "running"),
        ("vmnorole", "", "running"),
    ] {
        fake.add_instance_with_metadata(
            id,
            "crabbox-alice--name-1",
            "vm",
            status,
            &[
                ("crabbox_owner", "alice"),
                ("crabbox_role", role),
                ("crabbox_lease_id", "cbx_000000000001"),
                ("crabbox_slug", "slug-1"),
            ],
        );
    }
    let listed = run(&request(&fake, "list", &json!({})));
    assert_eq!(listed.code, 0, "{}", listed.stdout);
    let ids: Vec<&Value> = listed.stdout["leases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| &l["cloudId"])
        .collect();
    assert_eq!(ids, ["vmlive"]);
    let desired = json!({ "leaseId": "cbx_000000000001", "slug": "slug-1", "name": "name-1" });
    let resolved = run(&request(&fake, "resolve", &json!({ "desired": desired })));
    assert_eq!(resolved.code, 0, "{}", resolved.stdout);
    assert_eq!(resolved.stdout["lease"]["cloudId"], "vmlive");
    for out in [&listed, &resolved] {
        assert!(
            out.stderr.contains("vmlive") && out.stderr.contains("vmstale"),
            "warning names both: {}",
            out.stderr
        );
    }
}
