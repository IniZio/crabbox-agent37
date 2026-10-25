//! The one sweep: surplus, expired, stuck and old-format seeds, and stranded claims.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use serde_json::Value;

use super::meta::{
    generation, now, old_seed, seed_of, stopped_seed, stranded_claim, GEN, KEY, RELEASED_AT,
};
use super::seed::lock;
use crate::agent37::{Agent37, Instance};
use crate::lease::status_of;

/// Stopped seeds kept per seed key when the config sets no `cache.seedsPerKey`.
const SEEDS_PER_KEY: u64 = 3;
/// Seed age limit when the config sets no `cache.ttlDays`.
const TTL_DAYS: u64 = 7;
/// Seeds kept per owner when the config sets no `cache.maxSeeds`.
const MAX_SEEDS: u64 = 10;
/// Seconds a Box may stay `claiming` before it counts as stranded, unless twice
/// `readyTimeoutSeconds` is longer. A claim holds that state from its `PATCH` until the started
/// Box settles it, which the ready timeout bounds.
const CLAIM_SECS: u64 = 3600;
/// Seconds a seed may stay not stopped before it counts as stuck.
const STUCK_SECS: u64 = 3600;

/// True for a seed VM of `owner` that has not been stopped for over [`STUCK_SECS`] since it
/// became a seed. Nothing claims a seed that is not stopped, so it would run forever.
fn stuck_seed(instance: &Instance, owner: &str) -> bool {
    let since = instance
        .meta(RELEASED_AT)
        .or_else(|| instance.meta(GEN))
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    seed_of(instance, owner)
        && status_of(instance) != "stopped"
        && since.saturating_add(STUCK_SECS) < now()
}

/// A non-negative integer under `config.cache.<field>`, else `default`, and at least `min`.
fn limit(config: &Value, field: &str, (default, min): (u64, u64)) -> Result<u64, String> {
    let Some(v) = config
        .get("cache")
        .and_then(|c| c.get(field))
        .filter(|v| !v.is_null())
    else {
        return Ok(default);
    };
    v.as_u64()
        .filter(|n| *n >= min)
        .ok_or_else(|| format!("config.cache.{field} must be a whole number of at least {min}"))
}

/// What the sweep would delete.
#[derive(Debug, Default)]
pub(crate) struct Sweep<'a> {
    /// Seeds beyond `cache.seedsPerKey` of their key or `cache.maxSeeds` of the owner, past
    /// `cache.ttlDays`, stuck not stopped, or in the previous release's format.
    pub(crate) seeds: Vec<&'a str>,
    /// Boxes still `claiming` long after their claim wrote them.
    pub(crate) claims: Vec<&'a str>,
    /// Unix time before which a claim counts as stranded.
    pub(crate) claim_cutoff: u64,
}

/// Deletes each of `ids` that a fresh read under the host claim lock still shows as a
/// stopped seed of `owner`, a stuck one or an old-format one; returns the ids deleted. A claim renames a seed
/// before it starts it, so a Box being claimed is never deleted. Without the lock nothing is
/// deleted.
pub(crate) fn delete_seeds(api: &dyn Agent37, owner: &str, ids: &[String]) -> Vec<String> {
    let Some(_guard) = lock(owner) else {
        return Vec::new();
    };
    ids.iter()
        .filter(|id| {
            api.get_instance(id)
                .is_ok_and(|now| {
                    stopped_seed(&now, owner, None)
                        || stuck_seed(&now, owner)
                        || old_seed(&now, owner)
                })
                && api.delete_instance(id).is_ok()
        })
        .cloned()
        .collect()
}

/// Deletes each of `ids` that a fresh read still shows as a claim of `owner` written before
/// `cutoff`, so a claim given back to the seeds since the list is spared; returns the ids
/// deleted.
pub(crate) fn delete_stranded(
    api: &dyn Agent37,
    owner: &str,
    ids: &[String],
    cutoff: u64,
) -> Vec<String> {
    ids.iter()
        .filter(|id| {
            api.get_instance(id)
                .is_ok_and(|now| stranded_claim(&now, owner, cutoff))
                && api.delete_instance(id).is_ok()
        })
        .cloned()
        .collect()
}

/// The sweep of `owner`'s instances, per `config.cache`: seeds beyond `seedsPerKey` of a key
/// (3), older than `ttlDays` (7) or beyond `maxSeeds` of the owner (10), each oldest first,
/// seeds stuck not stopped, old-format seeds, and Boxes still `claiming` over [`CLAIM_SECS`]
/// (or twice `readyTimeoutSeconds`) after their claim.
pub(crate) fn gc<'a>(
    instances: &'a [Instance],
    owner: &str,
    config: &Value,
) -> Result<Sweep<'a>, String> {
    let per_key =
        usize::try_from(limit(config, "seedsPerKey", (SEEDS_PER_KEY, 1))?).unwrap_or(usize::MAX);
    let ttl = limit(config, "ttlDays", (TTL_DAYS, 0))?.saturating_mul(86_400);
    let max = usize::try_from(limit(config, "maxSeeds", (MAX_SEEDS, 0))?).unwrap_or(usize::MAX);
    let claim_age = CLAIM_SECS.max(crate::lease::settings(config)?.timeout.saturating_mul(2));
    let mut seeds: Vec<&Instance> = instances
        .iter()
        .filter(|i| stopped_seed(i, owner, None))
        .collect();
    seeds.sort_by_key(|i| Reverse((generation(i), i.id.clone())));
    let cutoff = now().saturating_sub(ttl);
    let mut by_key: BTreeMap<&str, Vec<&Instance>> = BTreeMap::new();
    for seed in &seeds {
        by_key
            .entry(seed.meta(KEY).unwrap_or_default())
            .or_default()
            .push(seed);
    }
    let mut doomed: Vec<&Instance> = by_key
        .values()
        .flat_map(|k| k.iter().skip(per_key).copied())
        .collect();
    doomed.extend(seeds.iter().filter(|i| generation(i) < cutoff));
    let kept: Vec<&Instance> = seeds
        .iter()
        .filter(|i| !doomed.iter().any(|d| d.id == i.id))
        .copied()
        .collect();
    doomed.extend(kept.iter().skip(max).copied());
    let stuck = instances
        .iter()
        .filter(|i| stuck_seed(i, owner) || old_seed(i, owner));
    let mut ids: Vec<&str> = doomed
        .into_iter()
        .chain(stuck)
        .map(|i| i.id.as_str())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let claim_cutoff = now().saturating_sub(claim_age);
    let claims = instances
        .iter()
        .filter(|i| stranded_claim(i, owner, claim_cutoff))
        .map(|i| i.id.as_str())
        .collect();
    Ok(Sweep {
        seeds: ids,
        claims,
        claim_cutoff,
    })
}
