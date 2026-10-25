//! A pool of stopped seeds per seed key: release keeps three, acquire claims the newest free
//! one, one sweep trims seeds and stranded leases, and a read-only listing shows the seeds.

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

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use cache_client::{
    acquire, acquire_as, acquire_extra, all, calls, config, instance, outcomes, release, run, KEY,
    SCOPE,
};
use fake_agent37::FakeAgent37;
use serde_json::{json, Value};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Acquires leases `1..=n` cold at once, so none claims a seed, and returns them.
fn cold_leases(fake: &FakeAgent37, n: u32) -> Vec<Value> {
    (1..=n).map(|i| acquire(fake, i, "standard")).collect()
}

/// Releases each lease in order.
fn release_all(fake: &FakeAgent37, leases: &[Value]) {
    for lease in leases {
        assert_eq!(release(fake, lease), 0);
    }
}

/// Acquires leases `first..` at once, one per temp dir in `hosts`; a shared dir is one host.
#[allow(
    clippy::needless_collect,
    reason = "every racer starts before any is joined"
)]
fn together(fake: &FakeAgent37, first: u32, hosts: &[&Path]) -> Vec<Value> {
    std::thread::scope(|s| {
        let racers: Vec<_> = (first..)
            .zip(hosts)
            .map(|(n, tmp)| s.spawn(move || acquire_as(fake, "me", n, "standard", tmp)))
            .collect();
        racers.into_iter().map(|r| r.join().unwrap()).collect()
    })
}

/// Ids of the instances the fake holds, sorted.
fn ids(fake: &FakeAgent37) -> Vec<String> {
    let mut ids: Vec<String> = fake
        .instances()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    ids
}

/// Gives seed `id` the generation of `ago` seconds ago.
fn born(fake: &FakeAgent37, id: &Value, ago: u64) {
    fake.set_metadata(
        id.as_str().unwrap(),
        "crabbox_seed_gen",
        &(now() - ago).to_string(),
    );
}

/// Runs `operation` for owner `me` with `cache` merged into the config.
fn operate(fake: &FakeAgent37, operation: &str, cache: &Value) -> (i32, Value) {
    let config = config(fake, &json!({ "cache": cache }));
    run(&json!({"protocolVersion": 1, "operation": operation, "config": config}))
}

/// Adds a stopped seed of `key`, born `ago` seconds ago.
fn add_seed(fake: &FakeAgent37, (id, key): (&str, &str), ago: u64) {
    let gen = (now() - ago).to_string();
    fake.add_instance_with_metadata(
        id,
        &format!("crabbox-seed-{key}-{gen}"),
        "vm",
        "stopped",
        &[
            ("crabbox_owner", "me"),
            ("crabbox_state", "seed"),
            ("crabbox_cache_key", key),
            ("crabbox_seed_gen", &gen),
        ],
    );
}

/// Adds a running lease Box of `owner` in `state`, claimed `ago` seconds ago when given.
fn add_lease(fake: &FakeAgent37, (id, owner, state): (&str, &str, &str), ago: Option<u64>) {
    add_box(fake, (id, owner, state, "running"), ago);
}

/// Adds a lease Box of `owner` in `state` and `status`, claimed `ago` seconds ago when given.
fn add_box(
    fake: &FakeAgent37,
    (id, owner, state, status): (&str, &str, &str, &str),
    ago: Option<u64>,
) {
    let claimed = ago.map(|a| (now() - a).to_string());
    let mut meta = vec![
        ("crabbox_owner", owner),
        ("crabbox_state", state),
        ("crabbox_lease_id", id),
        ("crabbox_slug", id),
    ];
    if let Some(c) = &claimed {
        meta.push(("crabbox_claimed_at", c));
    }
    fake.add_instance_with_metadata(id, &format!("crabbox-{owner}--{id}"), "vm", status, &meta);
}

#[test]
fn release_keeps_three_seeds_and_deletes_the_oldest_beyond_them() {
    let fake = FakeAgent37::start_with_key(KEY);
    let leases = cold_leases(&fake, 4);
    release_all(&fake, &leases[..3]);
    assert_eq!(
        ids(&fake),
        ["vm00000001", "vm00000002", "vm00000003", "vm00000004"]
    );
    release_all(&fake, &leases[3..]);
    assert_eq!(
        ids(&fake),
        ["vm00000002", "vm00000003", "vm00000004"],
        "the fourth release keeps three seeds and drops the oldest"
    );
    for id in ["vm00000002", "vm00000003", "vm00000004"] {
        let seed = fake.instance(id).unwrap();
        assert_eq!(seed["metadata"]["crabbox_state"], "seed");
        assert_eq!(seed["status"], "stopped");
    }
}

#[test]
fn seeds_per_key_sets_how_many_a_release_keeps() {
    let fake = FakeAgent37::start_with_key(KEY);
    let leases = cold_leases(&fake, 2);
    let cache = json!({"seedsPerKey": 1});
    for lease in &leases {
        let request = json!({
            "protocolVersion": 1, "operation": "release",
            "config": config(&fake, &json!({ "cache": cache })),
            "lease": {"leaseId": lease["leaseId"], "slug": lease["slug"],
                      "name": lease["name"], "cloudId": lease["cloudId"]},
        });
        assert_eq!(run(&request).0, 0);
    }
    assert_eq!(ids(&fake), ["vm00000002"]);
}

#[test]
fn acquire_claims_the_newest_seed_first() {
    let fake = FakeAgent37::start_with_key(KEY);
    let leases = cold_leases(&fake, 3);
    release_all(&fake, &leases);
    for (lease, ago) in leases.iter().zip([10, 300, 200]) {
        born(&fake, &lease["cloudId"], ago);
    }
    let (claimed, diag) = acquire_extra(&fake, 4, &json!({"cacheScope": SCOPE}));
    assert_eq!(
        claimed["cloudId"], leases[0]["cloudId"],
        "newest generation"
    );
    assert_eq!(outcomes(&claimed), all("restored"));
    let gen = fake.instance("vm00000001").unwrap()["metadata"]["crabbox_seed_gen"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        diag.contains(&format!("restored from seed gen {gen}")),
        "{diag}"
    );
    let (second, _) = acquire_extra(&fake, 5, &json!({"cacheScope": SCOPE}));
    assert_eq!(
        second["cloudId"], leases[2]["cloudId"],
        "then the next newest"
    );
    let lease = instance(&fake, &claimed["cloudId"]);
    assert_eq!(lease["metadata"]["crabbox_state"], "lease");
    assert!(lease["metadata"]["crabbox_claimed_at"].as_str().is_some());
}

#[test]
fn an_acquire_with_no_seed_logs_cold() {
    let fake = FakeAgent37::start_with_key(KEY);
    let (_, diag) = acquire_extra(&fake, 1, &json!({"cacheScope": SCOPE}));
    assert!(diag.contains("cold"), "{diag}");
    assert!(!diag.contains("restored from seed"), "{diag}");
}

#[test]
fn two_acquires_on_one_host_claim_two_different_seeds() {
    let fake = FakeAgent37::start_with_key(KEY);
    let leases = cold_leases(&fake, 3);
    release_all(&fake, &leases);
    let tmp = std::env::temp_dir();
    let got = together(&fake, 4, &[&tmp, &tmp]);
    assert_ne!(got[0]["cloudId"], got[1]["cloudId"]);
    for lease in &got {
        assert_eq!(outcomes(lease), all("restored"));
        assert!(leases.iter().any(|l| l["cloudId"] == lease["cloudId"]));
    }
}

#[test]
fn two_hosts_racing_with_two_seeds_end_on_two_different_seeds() {
    let fake = FakeAgent37::start_with_key(KEY);
    let leases = cold_leases(&fake, 2);
    release_all(&fake, &leases);
    born(&fake, &leases[0]["cloudId"], 100);
    born(&fake, &leases[1]["cloudId"], 50);
    fake.start_race();
    let hosts = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let got = together(&fake, 3, &[hosts[0].path(), hosts[1].path()]);
    assert_ne!(got[0]["cloudId"], got[1]["cloudId"], "{got:?}");
    for lease in &got {
        assert_eq!(outcomes(lease), all("restored"), "{got:?}");
        let held = instance(&fake, &lease["cloudId"]);
        assert_eq!(held["metadata"]["crabbox_lease_id"], lease["leaseId"]);
    }
}

#[test]
fn a_seed_taken_after_the_list_leaves_the_next_seed_to_claim() {
    let fake = FakeAgent37::start_with_key(KEY);
    let leases = cold_leases(&fake, 2);
    release_all(&fake, &leases);
    born(&fake, &leases[0]["cloudId"], 100);
    born(&fake, &leases[1]["cloudId"], 50);
    let newest = leases[1]["cloudId"].clone();
    let from = fake.requests().len();
    fake.take_after_list(newest.as_str().unwrap(), 2);
    let (got, diag) = acquire_extra(&fake, 3, &json!({"cacheScope": SCOPE}));
    assert_eq!(
        got["cloudId"], leases[0]["cloudId"],
        "the older seed: {diag}"
    );
    assert_eq!(outcomes(&got), all("restored"));
    assert_eq!(
        calls(&fake, from, "PATCH", &newest, ""),
        0,
        "no write to it"
    );
    assert_eq!(instance(&fake, &newest)["name"], "crabbox-me--rival");
}

#[test]
fn a_landed_claim_patch_is_given_back_and_spares_the_other_seeds() {
    let fake = FakeAgent37::start_with_key(KEY);
    let leases = cold_leases(&fake, 2);
    release_all(&fake, &leases);
    born(&fake, &leases[0]["cloudId"], 100);
    born(&fake, &leases[1]["cloudId"], 50);
    let (newest, other) = (leases[1]["cloudId"].clone(), leases[0]["cloudId"].clone());
    let from = fake.requests().len();
    fake.lose_next_patch(newest.as_str().unwrap(), false);
    let got = acquire(&fake, 3, "standard");
    assert_ne!(got["cloudId"], newest);
    let back = instance(&fake, &newest);
    assert_eq!(back["metadata"]["crabbox_state"], "seed");
    assert_eq!(back["status"], "stopped");
    assert_eq!(calls(&fake, from, "PATCH", &other, ""), 0);
    assert_eq!(instance(&fake, &other)["metadata"]["crabbox_state"], "seed");
}

#[test]
fn cleanup_trims_each_key_to_seeds_per_key_oldest_first() {
    let fake = FakeAgent37::start_with_key(KEY);
    for (n, ago) in [(1, 50), (2, 40), (3, 30), (4, 20), (5, 10)] {
        add_seed(&fake, (&format!("a{n}"), "ka"), ago);
    }
    add_seed(&fake, ("b1", "kb"), 60);
    let (code, out) = operate(&fake, "cleanup", &json!({}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        ids(&fake),
        ["a3", "a4", "a5", "b1"],
        "three newest of ka: {out}"
    );
    let (code, out) = operate(&fake, "cleanup", &json!({"seedsPerKey": 1}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(ids(&fake), ["a5", "b1"], "one per key: {out}");
}

#[test]
fn cleanup_deletes_seeds_past_the_ttl() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_seed(&fake, ("old", "k1"), 8 * 86_400);
    add_seed(&fake, ("mid", "k2"), 2 * 86_400);
    add_seed(&fake, ("new", "k3"), 60);
    let (code, out) = operate(&fake, "cleanup", &json!({}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(ids(&fake), ["mid", "new"], "{out}");
    let (code, out) = operate(&fake, "cleanup", &json!({"ttlDays": 1}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(ids(&fake), ["new"], "{out}");
}

#[test]
fn cleanup_keeps_the_newest_max_seeds_per_owner() {
    let fake = FakeAgent37::start_with_key(KEY);
    for n in 1..=6 {
        add_seed(&fake, (&format!("s{n}"), &format!("k{n}")), 70 - n * 10);
    }
    let (code, out) = operate(&fake, "cleanup", &json!({"maxSeeds": 4}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(ids(&fake), ["s3", "s4", "s5", "s6"], "{out}");
}

#[test]
fn cleanup_never_sweeps_a_lease_by_age() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, ("month-old", "me", "lease"), Some(30 * 86_400));
    add_box(
        &fake,
        ("asleep", "me", "lease", "stopped"),
        Some(30 * 86_400),
    );
    let (code, out) = operate(&fake, "cleanup", &json!({}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(ids(&fake), ["asleep", "month-old"], "{out}");
}

#[test]
fn cleanup_sweeps_a_claim_left_over_an_hour() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, ("stuck", "me", "claiming"), Some(2 * 3600));
    add_lease(&fake, ("fresh", "me", "claiming"), Some(600));
    add_lease(&fake, ("unstamped", "me", "claiming"), None);
    add_lease(&fake, ("theirs", "you", "claiming"), Some(2 * 3600));
    let (code, out) = operate(&fake, "cleanup", &json!({}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(ids(&fake), ["fresh", "theirs", "unstamped"], "{out}");
}

#[test]
fn a_long_ready_timeout_lengthens_the_claim_bound() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, ("slow", "me", "claiming"), Some(2 * 3600));
    let config = config(&fake, &json!({ "readyTimeoutSeconds": 3 * 3600 }));
    let (code, out) = run(&json!({"protocolVersion": 1, "operation": "cleanup", "config": config}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        ids(&fake),
        ["slow"],
        "a claim may still be starting it: {out}"
    );
}

#[test]
fn cleanup_spares_a_stale_claim_given_back_after_it_listed() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, ("stale", "me", "claiming"), Some(2 * 3600));
    fake.release_after_list("stale");
    let (code, out) = operate(&fake, "cleanup", &json!({}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(ids(&fake), ["stale"], "it is a seed again: {out}");
}

/// Adds a stopped seed of `owner` as the previous release named and marked it: a lease name,
/// `crabbox_role=seed`, no `crabbox_state`, and the lease id and slug its lease left behind.
fn add_old_seed(fake: &FakeAgent37, (id, owner): (&str, &str)) {
    add_old_format(fake, (id, owner, owner), "seed");
}

/// Adds a stopped old-format Box named for `named`, whose `crabbox_owner` metadata is `owner`
/// and whose `crabbox_role` is `role`.
fn add_old_format(fake: &FakeAgent37, (id, named, owner): (&str, &str, &str), role: &str) {
    let gen = (now() - 3600).to_string();
    fake.add_instance_with_metadata(
        id,
        &format!("crabbox-{named}--seed.{gen}"),
        "vm",
        "stopped",
        &[
            ("crabbox_owner", owner),
            ("crabbox_role", role),
            ("crabbox_cache_key", "k"),
            ("crabbox_seed_gen", &gen),
            ("crabbox_seed_since", &gen),
            ("crabbox_lease_id", id),
            ("crabbox_slug", id),
        ],
    );
}

#[test]
fn old_format_seeds_stay_out_of_list_and_cleanup_deletes_only_this_owners() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_old_seed(&fake, ("old", "me"));
    add_old_seed(&fake, ("theirs", "you"));
    add_lease(&fake, ("live", "me", "lease"), Some(60));
    let (code, listed) = operate(&fake, "list", &json!({}));
    assert_eq!(code, 0, "{listed}");
    let names: Vec<&Value> = listed["leases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| &l["name"])
        .collect();
    assert_eq!(names, ["live"], "{listed}");
    let (code, out) = operate(&fake, "cleanup", &json!({}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(ids(&fake), ["live", "theirs"], "{out}");
}

#[test]
fn cleanup_keeps_an_old_format_lease_named_like_a_seed() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_old_format(&fake, ("lease", "me", "me"), "lease");
    add_old_seed(&fake, ("old", "me"));
    let (code, out) = operate(&fake, "cleanup", &json!({}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(ids(&fake), ["lease"], "{out}");
}

#[test]
fn cleanup_keeps_an_old_format_seed_whose_metadata_names_another_owner() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_old_format(&fake, ("theirs", "me", "you"), "seed");
    add_old_seed(&fake, ("old", "me"));
    let (code, out) = operate(&fake, "cleanup", &json!({}));
    assert_eq!(code, 0, "{out}");
    assert_eq!(ids(&fake), ["theirs"], "{out}");
}

#[test]
fn list_hides_a_seed_by_its_state_whatever_its_name() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_lease(&fake, ("kept", "me", "seed"), None);
    add_lease(&fake, ("live", "me", "lease"), Some(60));
    let (code, listed) = operate(&fake, "list", &json!({}));
    assert_eq!(code, 0, "{listed}");
    let names: Vec<&Value> = listed["leases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| &l["name"])
        .collect();
    assert_eq!(names, ["live"], "{listed}");
}

/// Asserts each listed seed names its scope, a generation and the disk the fake created it with.
fn check_rows(fake: &FakeAgent37, rows: &[Value]) {
    for row in rows {
        assert_eq!(row["scope"], SCOPE);
        let disk =
            fake.instance(row["cloudId"].as_str().unwrap()).unwrap()["resources"]["disk"].clone();
        assert_eq!(row["diskGb"], disk);
        assert!(row["gen"].as_u64().unwrap() > 0);
    }
}

#[test]
fn seeds_stay_out_of_list_and_show_in_the_seed_listing() {
    let fake = FakeAgent37::start_with_key(KEY);
    let leases = cold_leases(&fake, 3);
    release_all(&fake, &leases);
    for (lease, ago) in leases.iter().zip([300, 200, 100]) {
        born(&fake, &lease["cloudId"], ago);
    }
    let (code, listed) = operate(&fake, "list", &json!({}));
    assert_eq!(code, 0);
    assert_eq!(
        listed["leases"],
        json!([]),
        "crabbox list never sees a seed"
    );
    let from = fake.requests().len();
    let (code, out) = operate(&fake, "seeds", &json!({}));
    assert_eq!(code, 0, "{out}");
    let rows = out["seeds"].as_array().unwrap();
    let got: Vec<&Value> = rows.iter().map(|r| &r["cloudId"]).collect();
    assert_eq!(
        got,
        ["vm00000003", "vm00000002", "vm00000001"],
        "newest first"
    );
    check_rows(&fake, rows);
    let ages: Vec<u64> = rows
        .iter()
        .map(|r| r["ageSeconds"].as_u64().unwrap())
        .collect();
    assert!(
        (100..110).contains(&ages[0]) && (300..310).contains(&ages[2]),
        "{ages:?}"
    );
    let writes = fake
        .requests()
        .iter()
        .skip(from)
        .filter(|r| r.method != "GET")
        .count();
    assert_eq!(writes, 0, "the listing only reads");
}

#[test]
fn the_seed_listing_shows_only_this_owners_seeds() {
    let fake = FakeAgent37::start_with_key(KEY);
    add_seed(&fake, ("mine", "k"), 60);
    fake.add_instance_with_metadata(
        "theirs",
        "crabbox-seed-k-1",
        "vm",
        "stopped",
        &[
            ("crabbox_owner", "you"),
            ("crabbox_state", "seed"),
            ("crabbox_cache_key", "k"),
        ],
    );
    let (code, out) = operate(&fake, "seeds", &json!({}));
    assert_eq!(code, 0, "{out}");
    let got: Vec<&Value> = out["seeds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["cloudId"])
        .collect();
    assert_eq!(got, ["mine"]);
}

#[test]
fn a_claim_writes_claiming_first_and_lease_once_the_box_settled_it() {
    let fake = FakeAgent37::start_with_key(KEY);
    let leases = cold_leases(&fake, 1);
    release_all(&fake, &leases);
    let seed = leases[0]["cloudId"].as_str().unwrap();
    let from = fake.requests().len();
    let (got, _) = acquire_extra(&fake, 2, &json!({"cacheScope": SCOPE}));
    assert_eq!(got["cloudId"], seed);
    let path = format!("/v1/instances/{seed}");
    let states: Vec<String> = fake
        .requests()
        .iter()
        .skip(from)
        .filter(|r| r.method == "PATCH" && r.path == path)
        .map(|r| {
            let body: Value = serde_json::from_str(&r.body).unwrap();
            body["metadata"]["crabbox_state"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(states, ["claiming", "lease"]);
}
