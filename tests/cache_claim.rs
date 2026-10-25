//! A seed claim never writes to a Box another lease took, and a claim `PATCH` that failed
//! but landed is given back.

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

use cache_client::{acquire, all, calls, instance, outcomes, release, KEY};
use fake_agent37::{FakeAgent37, StartFault};
use serde_json::Value;

/// Releases lease 1 into a seed and returns its id.
fn seeded(fake: &FakeAgent37) -> Value {
    let first = acquire(fake, 1, "standard");
    assert_eq!(release(fake, &first), 0);
    first["cloudId"].clone()
}

/// Asserts `seed` carries the rival lease, not lease 2, and that lease 2 went cold.
fn rival_keeps(fake: &FakeAgent37, seed: &Value, second: &Value) {
    assert_ne!(&second["cloudId"], seed);
    assert_eq!(outcomes(second), all("empty"), "lease 2 goes cold");
    let now = instance(fake, seed);
    assert_eq!(now["name"], "crabbox-me--rival");
    assert_eq!(now["metadata"]["crabbox_lease_id"], "cbx_rival");
}

#[test]
fn a_seed_another_host_took_after_the_list_is_not_written() {
    let fake = FakeAgent37::start_with_key(KEY);
    let seed = seeded(&fake);
    let from = fake.requests().len();
    fake.take_after_list(seed.as_str().unwrap(), 2);
    let second = acquire(&fake, 2, "standard");
    rival_keeps(&fake, &seed, &second);
    assert_eq!(calls(&fake, from, "PATCH", &seed, ""), 0, "no write");
}

#[test]
fn a_claim_patch_that_landed_but_failed_returns_the_seed() {
    let fake = FakeAgent37::start_with_key(KEY);
    let seed = seeded(&fake);
    fake.lose_next_patch(seed.as_str().unwrap(), false);
    let second = acquire(&fake, 2, "standard");
    assert_ne!(second["cloudId"], seed);
    let now = instance(&fake, &seed);
    assert_eq!(now["status"], "stopped");
    assert_eq!(now["metadata"]["crabbox_state"], "seed");
    let gen = now["metadata"]["crabbox_seed_gen"].as_str().unwrap();
    assert_eq!(now["name"], format!("crabbox-seed-01234567-{gen}"));
}

#[test]
fn a_landed_claim_patch_that_cannot_be_reverted_is_deleted() {
    let fake = FakeAgent37::start_with_key(KEY);
    let seed = seeded(&fake);
    fake.lose_next_patch(seed.as_str().unwrap(), true);
    let second = acquire(&fake, 2, "standard");
    assert_ne!(second["cloudId"], seed);
    assert_eq!(instance(&fake, &seed), Value::Null);
}

#[test]
fn a_seed_a_rival_took_while_its_stop_failed_is_not_deleted() {
    let fake = FakeAgent37::start_with_key(KEY);
    let seed = seeded(&fake);
    let from = fake.requests().len();
    fake.fail_next_start(StartFault::ClaimFailsThenRivalAtStop);
    let second = acquire(&fake, 2, "standard");
    rival_keeps(&fake, &seed, &second);
    assert_eq!(calls(&fake, from, "POST", &seed, "/stop"), 1);
    assert_eq!(calls(&fake, from, "DELETE", &seed, ""), 0, "no delete");
}
