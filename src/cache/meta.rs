//! What an agent37 VM's metadata says about a cache lease or seed.
//!
//! Only the adapter turns a lease into a seed and back. A Box is one lease lifetime and never
//! names a seed.

use std::time::{SystemTime, UNIX_EPOCH};

use super::{script, Plan};
use crate::agent37::Instance;
use crate::lease::{prefix, status_of};
use crate::protocol::Desired;

/// Where a VM is in its life: [`State`] as text.
pub(super) const STATE: &str = "crabbox_state";
/// Seed key of a lease or seed.
pub(super) const KEY: &str = "crabbox_cache_key";
/// Sealed volumes of a lease, from [`script::sealed`].
const SEALED: &str = "crabbox_cache_sealed";
/// A seed's generation (release time, unix seconds); on a lease, the seed it came from.
pub(super) const GEN: &str = "crabbox_seed_gen";
/// When the adapter last wrote a lease (unix seconds); a claim far past this is stranded.
pub(super) const CLAIMED_AT: &str = "crabbox_claimed_at";
/// When a Box last became a seed (unix seconds); a seed not stopped long after is stuck.
pub(super) const RELEASED_AT: &str = "crabbox_released_at";
/// Every seed's name starts with this, so no lease name ever points at one.
pub(super) const SEED_PREFIX: &str = "crabbox-seed-";

/// The life stage `crabbox_state` records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum State {
    /// A Box a crabbox lease runs on.
    Lease,
    /// A stopped Box kept for the next acquire of its key.
    Seed,
    /// A seed an acquire renamed but has not yet started and settled.
    Claiming,
}

impl State {
    /// The metadata value.
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Lease => "lease",
            Self::Seed => "seed",
            Self::Claiming => "claiming",
        }
    }
}

/// The recorded state of `instance`, if it has one.
fn state_of(instance: &Instance) -> Option<State> {
    [State::Lease, State::Seed, State::Claiming]
        .into_iter()
        .find(|s| instance.meta(STATE) == Some(s.as_str()))
}

/// True for a Box kept as a seed.
pub(crate) fn is_seed(instance: &Instance) -> bool {
    state_of(instance) == Some(State::Seed)
}

/// True for a Box a lease with cache volumes runs on, claimed or still being claimed.
pub(crate) fn is_lease(instance: &Instance) -> bool {
    matches!(state_of(instance), Some(State::Lease | State::Claiming))
}

/// The generation of the seed a lease came from, or of a seed itself.
pub(crate) fn seed_gen(instance: &Instance) -> Option<&str> {
    instance.meta(GEN)
}

/// True for a seed VM of `owner`. A seed's name carries no owner, so ownership is the
/// metadata alone, behind the seed name prefix.
pub(crate) fn seed_of(instance: &Instance, owner: &str) -> bool {
    is_seed(instance)
        && instance.meta("crabbox_owner") == Some(owner)
        && instance.kind.as_deref() == Some("vm")
        && instance
            .name
            .as_deref()
            .is_some_and(|n| n.starts_with(SEED_PREFIX))
}

/// True for a stopped seed VM of `owner`, of `key` when one is given: the only Box a claim
/// takes or a cleanup deletes.
pub(super) fn stopped_seed(instance: &Instance, owner: &str, key: Option<&str>) -> bool {
    seed_of(instance, owner)
        && key.is_none_or(|k| instance.meta(KEY) == Some(k))
        && status_of(instance) == "stopped"
}

/// True for a Box of `owner` still `claiming` that a claim wrote before `cutoff`: a claim that
/// failed and could not give it back. A `lease` is never stranded by age, since a Box may stay
/// open as long as its user wants.
pub(super) fn stranded_claim(instance: &Instance, owner: &str, cutoff: u64) -> bool {
    let written = instance
        .meta(CLAIMED_AT)
        .and_then(|t| t.parse::<u64>().ok());
    state_of(instance) == Some(State::Claiming)
        && crate::inventory::owned(instance, owner)
        && instance.kind.as_deref() == Some("vm")
        && written.is_some_and(|t| t < cutoff)
}

/// True for a seed the previous release made: `crabbox_role=seed`, no `crabbox_state`, and a
/// name `crabbox-<owner>--seed.<gen>`. Nothing claims one; the sweep deletes it.
pub(crate) fn old_seed(instance: &Instance, owner: &str) -> bool {
    state_of(instance).is_none()
        && instance.meta("crabbox_role") == Some("seed")
        && crate::inventory::owned(instance, owner)
        && instance.kind.as_deref() == Some("vm")
        && instance
            .name
            .as_deref()
            .is_some_and(|n| n.starts_with(&format!("{}seed.", prefix(owner))))
}

/// A seed's generation; unreadable sorts oldest.
pub(super) fn generation(instance: &Instance) -> u64 {
    instance.meta(GEN).and_then(|g| g.parse().ok()).unwrap_or(0)
}

/// The cache scope a seed key was made from: the part before the first `-`.
pub(crate) fn scope_of(key: &str) -> &str {
    key.split_once('-').map_or(key, |(scope, _)| scope)
}

/// Seconds since the epoch.
pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The state and timestamp every lease the adapter makes carries.
pub(crate) fn lease_marks() -> Vec<(String, String)> {
    vec![
        (STATE.to_owned(), State::Lease.as_str().to_owned()),
        (CLAIMED_AT.to_owned(), now().to_string()),
    ]
}

/// Metadata of a lease that uses cache volumes; `gen` is the seed it came from.
pub(super) fn lease_metadata(
    owner: &str,
    desired: &Desired,
    (plan, key): (&Plan, &str),
    (state, gen): (State, Option<&str>),
) -> Vec<(String, String)> {
    let mut meta = vec![
        ("crabbox_owner", owner.to_owned()),
        (STATE, state.as_str().to_owned()),
        (CLAIMED_AT, now().to_string()),
        ("crabbox_lease_id", desired.lease_id.clone()),
        ("crabbox_slug", desired.slug.clone()),
        (KEY, key.to_owned()),
        (SEALED, script::sealed(plan)),
    ];
    if let Some(gen) = gen {
        meta.push((GEN, gen.to_owned()));
    }
    meta.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}

/// Sealed volumes a lease recorded.
pub(super) fn sealed(instance: &Instance) -> &str {
    instance.meta(SEALED).unwrap_or("[]")
}

/// A seed's name, `crabbox-seed-<scope8>-<gen>`, and metadata for generation `gen` of `key`;
/// [`RELEASED_AT`] is now.
pub(super) fn seed_identity(owner: &str, key: &str, gen: &str) -> (String, Vec<(String, String)>) {
    let released = now().to_string();
    let meta = [
        ("crabbox_owner", owner),
        (STATE, State::Seed.as_str()),
        (KEY, key),
        (GEN, gen),
        (RELEASED_AT, &released),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
    .collect();
    let scope: String = scope_of(key).chars().take(8).collect();
    (format!("{SEED_PREFIX}{scope}-{gen}"), meta)
}
