//! The cache scope keys agent37 seeds; seeds that a failed claim leaves running are stopped.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail loudly on setup errors"
)]

#[path = "support/fake_agent37.rs"]
mod fake_agent37;

#[allow(dead_code, reason = "each test binary uses a different subset")]
#[path = "support/cache_client.rs"]
mod cache_client;

use std::time::{SystemTime, UNIX_EPOCH};

use cache_client::{
    acquire, acquire_extra, all, config, instance, outcomes, release, run, seal_docker, store,
    volumes, DOCKER, KEY, NPM, OTHER_SCOPE, SCOPE,
};
use fake_agent37::{FakeAgent37, StartFault};
use serde_json::{json, Value};

/// Acquires lease 1 with data in both volumes and releases it into a seed of [`SCOPE`].
fn seeded(fake: &FakeAgent37) -> Value {
    let first = acquire(fake, 1, "standard");
    let root = fake.box_root(first["cloudId"].as_str().unwrap());
    std::fs::write(store(&root, NPM).join("pkg.tgz"), "npm data").unwrap();
    std::fs::write(store(&root, DOCKER).join("image"), "layers").unwrap();
    seal_docker(&root);
    assert_eq!(release(fake, &first), 0);
    assert_eq!(
        instance(fake, &first["cloudId"])["metadata"]["crabbox_role"],
        "seed"
    );
    first
}

/// The seed of `first` is still a stopped seed.
fn untouched(fake: &FakeAgent37, first: &Value) {
    let seed = instance(fake, &first["cloudId"]);
    assert_eq!(seed["status"], "stopped");
    assert_eq!(seed["metadata"]["crabbox_role"], "seed");
}

#[test]
fn two_scopes_with_identical_groups_never_share_a_seed() {
    let fake = FakeAgent37::start_with_key(KEY);
    let first = seeded(&fake);
    let (other, _) = acquire_extra(&fake, 2, &json!({ "cacheScope": OTHER_SCOPE }));
    assert_ne!(other["cloudId"], first["cloudId"]);
    assert_eq!(outcomes(&other), all("empty"));
    untouched(&fake, &first);
}

#[test]
fn a_seed_of_the_scope_serves_a_new_group_as_empty() {
    let fake = FakeAgent37::start_with_key(KEY);
    let first = seeded(&fake);
    let mut more = volumes();
    more.as_array_mut()
        .unwrap()
        .push(json!({"group": "g2", "name": "pip", "key": "g2-pip", "paths": ["~/.cache/pip"]}));
    let extra = json!({ "cacheScope": SCOPE, "cacheVolumes": more });
    let (second, _) = acquire_extra(&fake, 2, &extra);
    assert_eq!(second["cloudId"], first["cloudId"], "the seed is reused");
    let mut want = all("restored");
    want.push(("pip".into(), "empty".into()));
    assert_eq!(outcomes(&second), want);
}

#[test]
fn without_a_scope_acquire_is_cold_and_release_deletes() {
    let fake = FakeAgent37::start_with_key(KEY);
    let first = seeded(&fake);
    let (cold, _) = acquire_extra(&fake, 2, &json!({}));
    assert_ne!(cold["cloudId"], first["cloudId"]);
    assert_eq!(outcomes(&cold), all("empty"));
    let root = fake.box_root(cold["cloudId"].as_str().unwrap());
    assert_eq!(
        std::fs::read_link(root.join("root/.npm")).unwrap(),
        store(&root, NPM),
        "the cold Box still gets its links"
    );
    assert_eq!(release(&fake, &cold), 0);
    assert_eq!(instance(&fake, &cold["cloudId"]), Value::Null, "no seed");
    untouched(&fake, &first);
}

#[test]
fn an_invalid_scope_is_cold_and_diagnosed() {
    let fake = FakeAgent37::start_with_key(KEY);
    let first = seeded(&fake);
    for (n, bad) in [(2, json!("0123456789ABCDEF")), (3, json!(42))] {
        let (cold, diag) = acquire_extra(&fake, n, &json!({ "cacheScope": bad }));
        assert_ne!(cold["cloudId"], first["cloudId"]);
        assert_eq!(outcomes(&cold), all("empty"));
        assert!(diag.contains("cacheScope"), "{diag}");
        assert_eq!(release(&fake, &cold), 0);
        assert_eq!(instance(&fake, &cold["cloudId"]), Value::Null);
    }
    untouched(&fake, &first);
}

#[test]
fn a_seed_left_running_by_a_failed_claim_is_stopped() {
    let fake = FakeAgent37::start_with_key(KEY);
    let first = seeded(&fake);
    fake.fail_next_start(StartFault::ClaimFails);
    let second = acquire(&fake, 2, "standard");
    assert_ne!(second["cloudId"], first["cloudId"]);
    let now = instance(&fake, &first["cloudId"]);
    assert_eq!(now["metadata"]["crabbox_role"], "seed");
    assert_eq!(now["status"], "stopped", "the reverted seed is stopped");
}

#[test]
fn a_seed_left_running_that_will_not_stop_is_deleted() {
    let fake = FakeAgent37::start_with_key(KEY);
    let first = seeded(&fake);
    fake.fail_next_start(StartFault::ClaimFailsThenStopFails);
    let second = acquire(&fake, 2, "standard");
    assert_ne!(second["cloudId"], first["cloudId"]);
    assert_eq!(instance(&fake, &first["cloudId"]), Value::Null);
}

#[test]
fn cleanup_deletes_a_seed_stuck_running_for_over_an_hour() {
    let fake = FakeAgent37::start_with_key(KEY);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    for (id, since) in [("stuck", now - 2 * 3600), ("fresh", now - 60)] {
        let since = since.to_string();
        fake.add_instance_with_metadata(
            id,
            &format!("crabbox-me--seed.{since}"),
            "vm",
            "running",
            &[
                ("crabbox_owner", "me"),
                ("crabbox_role", "seed"),
                ("crabbox_cache_key", "k"),
                ("crabbox_seed_gen", "1"),
                ("crabbox_seed_since", &since),
            ],
        );
    }
    let config = config(&fake, &json!({}));
    let (code, out) = run(&json!({"protocolVersion": 1, "operation": "cleanup", "config": config}));
    assert_eq!(code, 0, "{out}");
    let ids: Vec<Value> = fake.instances().iter().map(|i| i["id"].clone()).collect();
    assert_eq!(ids, ["fresh"], "{out}");
}
