//! Seed garbage collection: expired, surplus and stuck seeds.

use serde_json::Value;

use super::seed::{generation, is_seed, lock, now, stopped_seed, GEN, SINCE};
use crate::agent37::{Agent37, Instance};
use crate::inventory::owned;
use crate::lease::status_of;

/// Seed age limit when the config sets no `cache.ttlDays`.
const TTL_DAYS: u64 = 7;
/// Seeds kept per owner when the config sets no `cache.maxSeeds`.
const MAX_SEEDS: u64 = 10;
/// Seconds a seed may stay not stopped before it counts as stuck.
const STUCK_SECS: u64 = 3600;

/// True for a seed VM of `owner` that has not been stopped for over [`STUCK_SECS`] since it
/// became a seed. Nothing claims a seed that is not stopped, so it would run forever.
fn stuck_seed(instance: &Instance, owner: &str) -> bool {
    let since = instance
        .meta(SINCE)
        .or_else(|| instance.meta(GEN))
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    owned(instance, owner)
        && is_seed(instance)
        && instance.kind.as_deref() == Some("vm")
        && status_of(instance) != "stopped"
        && since.saturating_add(STUCK_SECS) < now()
}

/// A non-negative integer under `config.cache.<field>`, else `default`.
fn limit(config: &Value, field: &str, default: u64) -> Result<u64, String> {
    match config.get("cache").and_then(|c| c.get(field)) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_u64()
            .ok_or_else(|| format!("config.cache.{field} must be a whole number")),
    }
}

/// Deletes each of `ids` that a fresh read under the host claim lock still shows as a
/// stopped seed of `owner` (and of `key` when given), or a stuck one; returns the ids
/// deleted. A claim renames a seed before it starts it, so a Box being claimed is never
/// deleted. Without the lock nothing is deleted.
pub(crate) fn delete_seeds(
    api: &dyn Agent37,
    owner: &str,
    ids: &[String],
    key: Option<&str>,
) -> Vec<String> {
    let Some(_guard) = lock(owner) else {
        return Vec::new();
    };
    ids.iter()
        .filter(|id| {
            api.get_instance(id)
                .is_ok_and(|now| stopped_seed(&now, owner, key) || stuck_seed(&now, owner))
                && api.delete_instance(id).is_ok()
        })
        .cloned()
        .collect()
}

/// Ids of this owner's seeds past `cache.ttlDays`, beyond the newest `cache.maxSeeds`, or
/// stuck not stopped.
pub(crate) fn gc<'a>(
    instances: &'a [Instance],
    owner: &str,
    config: &Value,
) -> Result<Vec<&'a str>, String> {
    let ttl = limit(config, "ttlDays", TTL_DAYS)?.saturating_mul(86_400);
    let max = usize::try_from(limit(config, "maxSeeds", MAX_SEEDS)?).unwrap_or(usize::MAX);
    let mut seeds: Vec<&Instance> = instances
        .iter()
        .filter(|i| stopped_seed(i, owner, None))
        .collect();
    seeds.sort_by_key(|i| std::cmp::Reverse((generation(i), i.id.clone())));
    let cutoff = now().saturating_sub(ttl);
    let stuck = instances.iter().filter(|i| stuck_seed(i, owner));
    Ok(seeds
        .into_iter()
        .enumerate()
        .filter(|(n, i)| *n >= max || generation(i) < cutoff)
        .map(|(_, i)| i)
        .chain(stuck)
        .map(|i| i.id.as_str())
        .collect())
}
