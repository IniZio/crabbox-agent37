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

use fake_agent37::{Dockerd, FakeAgent37};
use serde_json::{json, Value};

/// Key the fake accepts.
const KEY: &str = "sk_test_fake";

/// Runs `acquire` for `class` with `extra` merged into the config; returns exit code, stdout and the fake.
fn acquire(class: &str, extra: &Value) -> (i32, Value, FakeAgent37) {
    acquire_env(class, extra, &[])
}

/// Like [`acquire`], with `env` set on the adapter process.
fn acquire_env(class: &str, extra: &Value, env: &[(&str, &str)]) -> (i32, Value, FakeAgent37) {
    acquire_on(FakeAgent37::start_with_key(KEY), class, extra, env)
}

/// Like [`acquire_env`], against a fake the caller already seeded.
fn acquire_on(
    fake: FakeAgent37,
    class: &str,
    extra: &Value,
    env: &[(&str, &str)],
) -> (i32, Value, FakeAgent37) {
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
        .env_remove("CRABBOX_AGENT37_TEMPLATE")
        .env_remove("CRABBOX_AGENT37_DISK_GB")
        .envs(env.iter().copied())
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
fn unset_disk_asks_for_the_class_maximum() {
    for (class, shape) in [
        ("standard", json!({"cpu": 2, "memory": 4, "disk": 12})),
        ("fast", json!({"cpu": 4, "memory": 8, "disk": 20})),
        ("large", json!({"cpu": 8, "memory": 16, "disk": 40})),
        ("beast", json!({"cpu": 16, "memory": 32, "disk": 80})),
    ] {
        let (code, out, fake) = acquire(class, &json!({}));
        assert_eq!(code, 0, "{class}: {out}");
        let body = create_body(&fake).unwrap();
        assert_eq!(body["template"], "agent37-ubuntu");
        assert_eq!(body["resources"], shape, "{class}");
    }
}

/// Requests whose method and path end match, such as `("POST", "/stop")`.
fn calls(fake: &FakeAgent37, method: &str, suffix: &str) -> usize {
    fake.requests()
        .iter()
        .filter(|r| r.method == method && r.path.ends_with(suffix))
        .filter(|r| suffix != "/exec" || r.body.contains("crabbox-docker-root"))
        .count()
}

/// Acquires a new standard VM against a fake whose dockerd behaves as `mode`.
fn acquire_with_dockerd(mode: Dockerd) -> (i32, Value, FakeAgent37) {
    let fake = FakeAgent37::start_with_key(KEY);
    fake.set_dockerd(mode);
    acquire_on(fake, "standard", &json!({}), &[])
}

#[test]
fn a_created_vm_gets_docker_data_on_the_billed_disk() {
    let (code, out, fake) = acquire_with_dockerd(Dockerd::OnBilledDisk);
    assert_eq!(code, 0, "{out}");
    assert!(fake.docker_root_set(out["lease"]["cloudId"].as_str().unwrap()));
    assert_eq!(calls(&fake, "POST", "/exec"), 1);
    assert_eq!(calls(&fake, "POST", "/stop"), 0);
}

#[test]
fn an_existing_vm_keeps_its_docker_config() {
    let fake = FakeAgent37::start_with_key(KEY);
    fake.add_instance("i1", "crabbox-alice--crabbox-blue-crab-000000000001", "vm");
    let (code, out, fake) = acquire_on(fake, "standard", &json!({}), &[]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(calls(&fake, "POST", "/exec"), 0);
}

#[test]
fn a_running_dockerd_elsewhere_is_moved_by_a_reboot() {
    let (code, out, fake) = acquire_with_dockerd(Dockerd::ElsewhereUntilBoot);
    assert_eq!(code, 0, "{out}");
    assert_eq!(calls(&fake, "POST", "/stop"), 1);
    assert_eq!(calls(&fake, "POST", "/start"), 1);
    assert_eq!(calls(&fake, "POST", "/exec"), 2);
    let id = out["lease"]["cloudId"].as_str().unwrap();
    let key = out["lease"]["ssh"]["hostKey"].as_str().unwrap();
    assert_eq!(key, fake.current_host_key(id));
}

#[test]
fn docker_off_the_billed_disk_fails_acquire_and_deletes_the_vm() {
    for (mode, says) in [
        (
            Dockerd::AlwaysElsewhere,
            "dockerd is still not on /root/docker after a reboot",
        ),
        (Dockerd::ScriptFails, "JSONDecodeError"),
    ] {
        let (code, out, fake) = acquire_with_dockerd(mode);
        assert_eq!(code, 1, "{out}");
        assert!(out.to_string().contains(says), "{out}");
        assert!(fake.instances().is_empty(), "{:?}", fake.instances());
    }
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

#[test]
fn env_template_and_disk_override_config() {
    let env = [
        ("CRABBOX_AGENT37_TEMPLATE", "agent37-python"),
        ("CRABBOX_AGENT37_DISK_GB", "16"),
    ];
    let (code, out, fake) = acquire_env(
        "fast",
        &json!({"template": "agent37-node", "diskGb": 8}),
        &env,
    );
    assert_eq!(code, 0, "{out}");
    let body = create_body(&fake).unwrap();
    assert_eq!(body["template"], "agent37-python");
    assert_eq!(body["resources"]["disk"], 16);
}

#[test]
fn empty_env_counts_as_unset() {
    let env = [
        ("CRABBOX_AGENT37_TEMPLATE", ""),
        ("CRABBOX_AGENT37_DISK_GB", ""),
    ];
    let (code, out, fake) = acquire_env(
        "fast",
        &json!({"template": "agent37-node", "diskGb": 8}),
        &env,
    );
    assert_eq!(code, 0, "{out}");
    let body = create_body(&fake).unwrap();
    assert_eq!(body["template"], "agent37-node");
    assert_eq!(body["resources"]["disk"], 8);
}

#[test]
fn env_disk_errors_name_the_variable() {
    let (code, out, fake) = acquire_env("fast", &json!({}), &[("CRABBOX_AGENT37_DISK_GB", "21")]);
    assert_eq!(code, 1, "{out}");
    assert!(create_body(&fake).is_none());
    assert!(
        out.to_string()
            .contains("CRABBOX_AGENT37_DISK_GB 21 is outside 2-20 GB for a 4 vCPU / 8 GB VM"),
        "{out}"
    );
    for bad in ["abc", "2.5", "-3"] {
        let (code, out, fake) =
            acquire_env("fast", &json!({}), &[("CRABBOX_AGENT37_DISK_GB", bad)]);
        assert_eq!(code, 1, "{bad}: {out}");
        assert!(create_body(&fake).is_none());
        assert!(
            out.to_string()
                .contains("CRABBOX_AGENT37_DISK_GB must be a whole number of GB"),
            "{out}"
        );
    }
}

#[test]
fn template_is_sent_trimmed() {
    let (code, out, fake) = acquire("fast", &json!({"template": "  agent37-node "}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(create_body(&fake).unwrap()["template"], "agent37-node");
    let env = [("CRABBOX_AGENT37_TEMPLATE", " agent37-go ")];
    let (code, out, fake) = acquire_env("fast", &json!({}), &env);
    assert_eq!(code, 0, "{out}");
    assert_eq!(create_body(&fake).unwrap()["template"], "agent37-go");
}

#[test]
fn bad_disk_fails_even_when_the_instance_exists() {
    let fake = FakeAgent37::start_with_key(KEY);
    fake.add_instance("i1", "crabbox-alice--crabbox-blue-crab-000000000001", "vm");
    let env = [("CRABBOX_AGENT37_DISK_GB", "21")];
    let (code, out, fake) = acquire_on(fake, "fast", &json!({}), &env);
    assert_eq!(code, 1, "{out}");
    assert!(create_body(&fake).is_none());
    assert!(
        out.to_string().contains("CRABBOX_AGENT37_DISK_GB 21"),
        "{out}"
    );
}
