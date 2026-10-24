//! Seeds: stopped Boxes kept after release, claimed by the next acquire of the same key.
//!
//! agent37 has no conditional write. A claim is `PATCH` of the seed's name and metadata to the
//! new lease, then a fresh `GET` that must still show this lease; a host-wide file lock
//! serialises claims from one host. A seed is renamed so no lease name ever points at it.

use std::fs::File;
use std::io::Write;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::{script, Plan};
use crate::agent37::{Agent37, Instance};
use crate::docker_root::{reboot, wait_for};
use crate::inventory::owned;
use crate::lease::{prefix, status_of};
use crate::protocol::{CacheVolumeEcho, Desired};

/// `seed` on a seed, `lease` on a lease made with cache volumes. agent37 merges `PATCH`ed
/// metadata into the stored keys (its docs say it replaces them), so a seed keeps the lease id
/// and slug of the lease it was, and only this key tells the two apart.
const ROLE: &str = "crabbox_role";
/// Seed key of a lease or seed.
const KEY: &str = "crabbox_cache_key";
/// Sealed volumes of a lease, from [`script::sealed`].
const SEALED: &str = "crabbox_cache_sealed";
/// A seed's generation (release time, unix seconds); on a lease, the seed it came from.
const GEN: &str = "crabbox_seed_gen";
/// Seed age limit when the config sets no `cache.ttlDays`.
const TTL_DAYS: u64 = 7;
/// Seeds kept per owner when the config sets no `cache.maxSeeds`.
const MAX_SEEDS: u64 = 10;

/// True for a stopped Box kept as a seed.
pub(crate) fn is_seed(instance: &Instance) -> bool {
    instance.meta(ROLE) == Some("seed")
}

/// A seed's generation; unreadable sorts oldest.
fn generation(instance: &Instance) -> u64 {
    instance.meta(GEN).and_then(|g| g.parse().ok()).unwrap_or(0)
}

/// Seconds since the epoch.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Metadata of a lease that uses cache volumes; `gen` is the seed it came from.
fn lease_metadata(
    owner: &str,
    desired: &Desired,
    plan: &Plan,
    key: &str,
    gen: Option<&str>,
) -> Vec<(String, String)> {
    let mut meta = vec![
        ("crabbox_owner", owner.to_owned()),
        (ROLE, "lease".to_owned()),
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

/// Exclusive lock on this owner's claim file; claims from one host run one at a time.
fn lock(owner: &str) -> Option<File> {
    let path = std::env::temp_dir().join(format!("crabbox-agent37-{owner}.claim.lock"));
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    file.lock().ok()?;
    Some(file)
}

/// Claims the newest stopped seed of `key` for the lease named `name`; `None` means create
/// cold. The returned instance already carries the lease's name and metadata.
pub(crate) fn claim(
    api: &dyn Agent37,
    owner: &str,
    desired: &Desired,
    name: &str,
    (plan, key): (&Plan, &str),
    diag: &mut dyn Write,
) -> Option<Instance> {
    let _guard = lock(owner);
    let seed = api
        .list_instances()
        .ok()?
        .into_iter()
        .filter(|i| owned(i, owner) && is_seed(i) && i.meta(KEY) == Some(key))
        .filter(|i| i.kind.as_deref() == Some("vm") && status_of(i) == "stopped")
        .max_by_key(|i| (generation(i), i.id.clone()))?;
    let gen = seed.meta(GEN).unwrap_or("0").to_owned();
    let meta = lease_metadata(owner, desired, plan, key, Some(&gen));
    let patched = api.patch_instance(&seed.id, name, &meta).ok()?;
    let now = api.get_instance(&seed.id).unwrap_or(patched);
    let won = now.name.as_deref() == Some(name)
        && now.meta("crabbox_lease_id") == Some(desired.lease_id.as_str())
        && !is_seed(&now);
    let _ = if won {
        writeln!(diag, "acquire: claimed seed {} (gen {gen})", now.id)
    } else {
        writeln!(diag, "acquire: lost seed {} to another acquire", now.id)
    };
    won.then_some(now)
}

/// Starts a claimed seed and waits until it runs.
pub(crate) fn start(api: &dyn Agent37, id: &str, deadline: Instant) -> Result<(), String> {
    api.start_instance(id).map_err(|e| e.operator_message())?;
    wait_for(api, id, &["running"], deadline)
}

/// Links every planned volume on running `instance` and records the lease's cache metadata.
///
/// Reboots once when a running daemon held a replaced directory.
pub(crate) fn setup(
    api: &dyn Agent37,
    instance: &Instance,
    (owner, desired): (&str, &Desired),
    (plan, key): (&Plan, &str),
    deadline: Instant,
    diag: &mut dyn Write,
) -> Result<Vec<CacheVolumeEcho>, String> {
    let id = &instance.id;
    let out = api
        .exec_as_root(id, &script::acquire(plan))
        .map_err(|e| e.operator_message())?;
    if out.exit_code != 0 {
        return Err(format!(
            "cache setup on {id} exited {}: {}",
            out.exit_code,
            out.stderr.trim()
        ));
    }
    let (restored, reboot_due) = script::parse_acquire(&out.stdout);
    if reboot_due {
        let _ = writeln!(diag, "acquire: rebooting {id} onto the cache links");
        reboot(api, id, deadline)?;
    }
    let gen = instance.meta(GEN);
    if instance.meta(KEY) != Some(key) {
        let name = instance.name.as_deref().unwrap_or_default();
        let meta = lease_metadata(owner, desired, plan, key, gen);
        api.patch_instance(id, name, &meta)
            .map_err(|e| e.operator_message())?;
    }
    Ok(plan.echo(&restored, gen))
}

/// Turns a released, running Box with cache volumes into the newest seed of its key, then
/// deletes older seeds of that key. `Err` means the Box must be deleted instead.
fn make_seed(
    api: &dyn Agent37,
    owner: &str,
    instance: &Instance,
    timeout: u64,
) -> Result<(), String> {
    let id = &instance.id;
    let key = instance.meta(KEY).ok_or("no cache key")?;
    let prepare = script::release(instance.meta(SEALED).unwrap_or("[]"))?;
    let out = api
        .exec_as_root(id, &prepare)
        .map_err(|e| e.operator_message())?;
    if out.exit_code != 0 {
        return Err(format!("seed prepare exited {}", out.exit_code));
    }
    let deadline = Instant::now() + Duration::from_secs(timeout);
    api.stop_instance(id).map_err(|e| e.operator_message())?;
    wait_for(api, id, &["stopped"], deadline)?;
    let gen = now();
    let meta: Vec<(String, String)> = [
        ("crabbox_owner", owner),
        (ROLE, "seed"),
        (KEY, key),
        (GEN, &gen.to_string()),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
    .collect();
    api.patch_instance(id, &format!("{}seed.{gen}", prefix(owner)), &meta)
        .map_err(|e| e.operator_message())?;
    let older = api.list_instances().unwrap_or_default();
    for old in older.iter().filter(|i| {
        owned(i, owner)
            && is_seed(i)
            && i.meta(KEY) == Some(key)
            && status_of(i) == "stopped"
            && (generation(i), &i.id) < (gen, id)
    }) {
        let _ = api.delete_instance(&old.id);
    }
    Ok(())
}

/// Releases `instance`: a running Box with cache volumes becomes a seed, anything else (or a
/// Box whose seeding fails) is deleted. Never fails because of the cache.
pub(crate) fn release(
    api: &dyn Agent37,
    owner: &str,
    instance: &Instance,
    timeout: u64,
) -> Result<(), crate::agent37::ApiError> {
    let seeded = instance.meta(KEY).is_some()
        && status_of(instance) == "running"
        && make_seed(api, owner, instance, timeout).is_ok();
    if seeded {
        Ok(())
    } else {
        api.delete_instance(&instance.id)
    }
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

/// Ids of this owner's seeds past `cache.ttlDays` or beyond the newest `cache.maxSeeds`.
pub(crate) fn gc<'a>(
    instances: &'a [Instance],
    owner: &str,
    config: &Value,
) -> Result<Vec<&'a str>, String> {
    let ttl = limit(config, "ttlDays", TTL_DAYS)?.saturating_mul(86_400);
    let max = usize::try_from(limit(config, "maxSeeds", MAX_SEEDS)?).unwrap_or(usize::MAX);
    let mut seeds: Vec<&Instance> = instances
        .iter()
        .filter(|i| owned(i, owner) && is_seed(i))
        .collect();
    seeds.sort_by_key(|i| std::cmp::Reverse((generation(i), i.id.clone())));
    let cutoff = now().saturating_sub(ttl);
    Ok(seeds
        .into_iter()
        .enumerate()
        .filter(|(n, i)| *n >= max || generation(i) < cutoff)
        .map(|(_, i)| i.id.as_str())
        .collect())
}
