//! `acquire` template and disk settings against a fake agent37 API.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "tests fail loudly on setup errors"
)]

#[path = "support/fake_agent37.rs"]
mod fake_agent37;

use std::io::Write;
use std::process::{Command, Stdio};

use fake_agent37::FakeAgent37;
use serde_json::{json, Value};

/// Key the fake accepts.
const KEY: &str = "sk_test_fake";

/// Runs `acquire` for `class` with `extra` merged into the config; returns exit code, stdout and the fake.
fn acquire(class: &str, extra: &Value) -> (i32, Value, FakeAgent37) {
    let fake = FakeAgent37::start_with_key(KEY);
    let mut config = json!({ "apiUrl": fake.url(), "owner": "alice" });
    for (k, v) in extra.as_object().unwrap() {
        config[k] = v.clone();
    }
    let request = json!({
        "protocolVersion": 1,
        "operation": "acquire",
        "config": config,
        "desired": {
            "leaseId": "cbx_000000000001",
            "slug": "blue-crab",
            "name": "crabbox-blue-crab-000000000001",
            "class": class,
        },
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_crabbox-agent37"))
        .env("AGENT37_API_KEY", KEY)
        .env_remove("AGENT37_KEY_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = serde_json::from_slice(&output.stdout).unwrap();
    (output.status.code().unwrap(), stdout, fake)
}

/// The body of the create request, or `None` when none was sent.
fn create_body(fake: &FakeAgent37) -> Option<Value> {
    fake.requests()
        .into_iter()
        .find(|r| r.method == "POST" && r.path == "/v1/instances")
        .map(|r| serde_json::from_str(&r.body).unwrap())
}

#[test]
fn unset_keeps_default_template_and_sends_no_disk() {
    let (code, out, fake) = acquire("standard", &json!({}));
    assert_eq!(code, 0, "{out}");
    let body = create_body(&fake).unwrap();
    assert_eq!(body["template"], "agent37-ubuntu");
    assert_eq!(body["resources"], json!({"cpu": 2, "memory": 4}));
}

#[test]
fn template_and_disk_reach_the_create_body() {
    let (code, out, fake) = acquire("fast", &json!({"template": "agent37-node", "diskGb": 20}));
    assert_eq!(code, 0, "{out}");
    let body = create_body(&fake).unwrap();
    assert_eq!(body["template"], "agent37-node");
    assert_eq!(
        body["resources"],
        json!({"cpu": 4, "memory": 8, "disk": 20})
    );
}

#[test]
fn disk_is_checked_against_the_shape_range() {
    for (class, ok, bad) in [
        ("standard", 12, 13),
        ("fast", 20, 21),
        ("large", 40, 41),
        ("beast", 80, 81),
    ] {
        let (code, out, _) = acquire(class, &json!({"diskGb": ok}));
        assert_eq!(code, 0, "{class} {ok}: {out}");
        let (code, out, fake) = acquire(class, &json!({"diskGb": bad}));
        assert_eq!(code, 1, "{class} {bad}: {out}");
        assert!(create_body(&fake).is_none());
        let text = out.to_string();
        assert!(
            text.contains("diskGb") && text.contains(&bad.to_string()),
            "{text}"
        );
    }
    let (code, _, _) = acquire("fast", &json!({"diskGb": 1}));
    assert_eq!(code, 1);
}

#[test]
fn bad_template_or_disk_type_is_rejected_without_creating() {
    for extra in [
        json!({"template": ""}),
        json!({"template": 3}),
        json!({"diskGb": "20"}),
        json!({"diskGb": 2.5}),
    ] {
        let (code, out, fake) = acquire("fast", &extra);
        assert_eq!(code, 1, "{extra}: {out}");
        assert!(create_body(&fake).is_none(), "{extra}");
    }
}
