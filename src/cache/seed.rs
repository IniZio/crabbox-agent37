//! Seeds: stopped Boxes kept after release, claimed by the next acquire of the same key.
//!
//! agent37 has no conditional write. A claim first `PATCH`es the seed's name and metadata to
//! the new lease (state `claiming`) and reads it back, under a host-wide file lock; that alone
//! lets two hosts both win. The started Box then settles it: [`script::claim`] makes a link on its tmpfs that
//! only one lease can create, and the winner writes its lease again. A seed is renamed so no
//! lease name ever points at it.

use std::cmp::Reverse;
use std::fs::File;
use std::io::Write;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::gc::{delete_seeds, gc};
use super::meta::{
    generation, is_seed, lease_metadata, now, sealed, seed_identity, stopped_seed, State, GEN, KEY,
};
use super::{script, Plan};
use crate::agent37::{Agent37, ApiError, Instance};
use crate::docker_root::{reboot, wait_for};
use crate::lease::{pause, status_of};
use crate::protocol::{CacheVolumeEcho, Desired};

/// Seconds a returned seed gets to stop.
const STOP_SECS: u64 = 120;

/// Exclusive lock on this owner's claim file; claims from one host run one at a time.
pub(super) fn lock(owner: &str) -> Option<File> {
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

/// Claims the newest stopped seed of `key` for the lease named `name`;
/// `None` means create cold. Each candidate is read again right before the `PATCH`, since
/// another host may have taken it since the list; a taken one passes to the next. The
/// returned instance already carries the lease's name and metadata (state `claiming`), but
/// another host may hold it too until [`take`] settles it. Without the host lock it goes cold.
pub(crate) fn claim(
    api: &dyn Agent37,
    owner: &str,
    desired: &Desired,
    name: &str,
    (plan, key): (&Plan, &str),
    diag: &mut dyn Write,
) -> Option<Instance> {
    let Some(guard) = lock(owner) else {
        let _ = writeln!(diag, "acquire: no claim lock; creating cold");
        return None;
    };
    let mut seeds: Vec<Instance> = api
        .list_instances()
        .ok()?
        .into_iter()
        .filter(|i| stopped_seed(i, owner, Some(key)))
        .collect();
    seeds.sort_by_key(|i| Reverse((generation(i), i.id.clone())));
    for seed in seeds {
        if !api
            .get_instance(&seed.id)
            .is_ok_and(|i| stopped_seed(&i, owner, Some(key)))
        {
            let _ = writeln!(diag, "acquire: seed {} was taken after the list", seed.id);
            continue;
        }
        let gen = seed.meta(GEN).unwrap_or("0").to_owned();
        let meta = lease_metadata(owner, desired, (plan, key), (State::Claiming, Some(&gen)));
        let now = match write_lease(api, &seed.id, name, &meta) {
            Ok(now) => now,
            Err(err) => {
                drop(guard);
                let did = unclaim(api, &seed.id, (owner, desired, name), (key, &gen));
                let _ = writeln!(
                    diag,
                    "acquire: claiming seed {} failed: {}; {did}",
                    seed.id,
                    err.operator_message()
                );
                return None;
            }
        };
        if holds(&now, name, desired) {
            let _ = writeln!(diag, "acquire: claimed seed {} (gen {gen})", now.id);
            return Some(now);
        }
        let _ = writeln!(diag, "acquire: lost seed {} to another acquire", now.id);
    }
    None
}

/// `PATCH`es the lease's name and metadata onto `id`, then reads it back. A failed `PATCH`
/// may still have landed.
fn write_lease(
    api: &dyn Agent37,
    id: &str,
    name: &str,
    meta: &[(String, String)],
) -> Result<Instance, ApiError> {
    let patched = api.patch_instance(id, name, meta)?;
    Ok(api.get_instance(id).unwrap_or(patched))
}

/// True when `instance` carries the lease's name and id and is no longer a seed.
fn holds(instance: &Instance, name: &str, desired: &Desired) -> bool {
    instance.name.as_deref() == Some(name)
        && instance.meta("crabbox_lease_id") == Some(desired.lease_id.as_str())
        && !is_seed(instance)
}

/// Gives back a claimed seed that failed before its in-Box claim, or whose claim `PATCH`
/// failed but may have landed, so the cold Box made next
/// is the only one named for this lease. A Box still carrying this lease gets its seed name
/// and metadata again, or is deleted when a fresh read does not show that; a Box carrying
/// another lease, or unreadable, is left alone. A returned seed that is not stopped is
/// stopped, since only a stopped seed is ever claimed. Returns what it did.
fn unclaim(
    api: &dyn Agent37,
    id: &str,
    (owner, desired, name): (&str, &Desired, &str),
    (key, gen): (&str, &str),
) -> &'static str {
    if !api.get_instance(id).is_ok_and(|i| holds(&i, name, desired)) {
        return "left to whoever holds it";
    }
    let (seed_name, meta) = seed_identity(owner, key, gen);
    let _ = api.patch_instance(id, &seed_name, &meta);
    let reverted = |i: &Instance| is_seed(i) && i.name.as_deref() == Some(seed_name.as_str());
    match api.get_instance(id) {
        Ok(i) if reverted(&i) && status_of(&i) == "stopped" => "returned to the seeds",
        Ok(i) if reverted(&i) => {
            let deadline = Instant::now() + Duration::from_secs(STOP_SECS);
            let stopped =
                api.stop_instance(id).is_ok() && wait_for(api, id, &["stopped"], deadline).is_ok();
            if stopped {
                return "stopped and returned to the seeds";
            }
            let guard = lock(owner);
            let ours = |i: &Instance| reverted(i) || holds(i, name, desired);
            match api.get_instance(id) {
                Ok(i) if status_of(&i) == "stopped" => "returned to the seeds",
                Ok(i) if guard.is_some() && ours(&i) && api.delete_instance(id).is_ok() => {
                    "deleted after its stop failed"
                }
                _ => "left alone after its stop failed",
            }
        }
        Ok(i) if holds(&i, name, desired) && api.delete_instance(id).is_ok() => "deleted",
        _ => "left alone",
    }
}

/// What starting a claimed seed came to.
pub(crate) enum Taken {
    /// It runs and is this lease's alone.
    Ours(Instance),
    /// Another lease claimed it inside the Box first; it is theirs and left alone.
    Lost,
    /// It failed before this lease could claim it inside the Box; see [`unclaim`].
    Failed(String),
}

/// Starts a claimed seed and claims it inside the Box. The winner writes its lease again,
/// over any rival's `PATCH`, and reads it back; `Err` means the Box is this lease's but
/// unusable.
pub(crate) fn take(
    api: &dyn Agent37,
    seed: &Instance,
    (owner, desired, name): (&str, &Desired, &str),
    (plan, key): (&Plan, &str),
    deadline: Instant,
) -> Result<Taken, String> {
    let id = &seed.id;
    let started = api.start_instance(id);
    let unclaimed = |why: String| {
        let gen = seed.meta(GEN).unwrap_or("0");
        Ok(Taken::Failed(format!(
            "{why}; {}",
            unclaim(api, id, (owner, desired, name), (key, gen))
        )))
    };
    if let Err(err) = wait_for(api, id, &["running"], deadline) {
        return unclaimed(started.err().map_or(err, |e| e.operator_message()));
    }
    let command = script::claim(&desired.lease_id);
    let holder = loop {
        match api.exec_as_root(id, &command) {
            Ok(out) if out.exit_code == 0 => break out.stdout.trim().to_owned(),
            _ if pause(deadline) => {}
            _ => return unclaimed(format!("claiming {id} inside the Box failed")),
        }
    };
    if holder != desired.lease_id {
        return Ok(Taken::Lost);
    }
    let meta = lease_metadata(owner, desired, (plan, key), (State::Lease, seed.meta(GEN)));
    match write_lease(api, id, name, &meta) {
        Ok(now) if holds(&now, name, desired) => Ok(Taken::Ours(now)),
        _ => Err(format!("{id} does not carry this lease after its claim")),
    }
}

/// Links every planned volume on running `instance` and records the lease's cache metadata.
/// Without a `key` nothing is recorded, so release deletes the Box.
///
/// Reboots once when a running daemon held a replaced directory.
pub(crate) fn setup(
    api: &dyn Agent37,
    instance: &Instance,
    (owner, desired): (&str, &Desired),
    (plan, key): (&Plan, Option<&str>),
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
    if let Some(key) = key.filter(|k| instance.meta(KEY) != Some(*k)) {
        let name = instance.name.as_deref().unwrap_or_default();
        let meta = lease_metadata(owner, desired, (plan, key), (State::Lease, gen));
        api.patch_instance(id, name, &meta)
            .map_err(|e| e.operator_message())?;
    }
    Ok(plan.echo(&restored, gen))
}

/// Turns a released, running Box with cache volumes into the newest seed of its key, then
/// sweeps what the pool no longer keeps. `Err` means the Box must be deleted instead.
fn make_seed(
    api: &dyn Agent37,
    (owner, config): (&str, &Value),
    instance: &Instance,
    timeout: u64,
) -> Result<(), String> {
    let id = &instance.id;
    let key = instance.meta(KEY).ok_or("no cache key")?;
    let prepare = script::release(sealed(instance))?;
    let out = api
        .exec_as_root(id, &prepare)
        .map_err(|e| e.operator_message())?;
    if out.exit_code != 0 {
        return Err(format!("seed prepare exited {}", out.exit_code));
    }
    let deadline = Instant::now() + Duration::from_secs(timeout);
    api.stop_instance(id).map_err(|e| e.operator_message())?;
    wait_for(api, id, &["stopped"], deadline)?;
    let (seed_name, meta) = seed_identity(owner, key, &now().to_string());
    api.patch_instance(id, &seed_name, &meta)
        .map_err(|e| e.operator_message())?;
    if let Ok(listed) = api.list_instances() {
        if let Ok(sweep) = gc(&listed, owner, config) {
            let surplus: Vec<String> = sweep.seeds.iter().map(|s| (*s).to_owned()).collect();
            delete_seeds(api, owner, &surplus);
        }
    }
    Ok(())
}

/// Releases `instance`: a running Box with cache volumes becomes a seed, anything else (or a
/// Box whose seeding fails) is deleted. Never fails because of the cache.
pub(crate) fn release(
    api: &dyn Agent37,
    (owner, config): (&str, &Value),
    instance: &Instance,
    timeout: u64,
) -> Result<(), ApiError> {
    let seeded = instance.meta(KEY).is_some()
        && status_of(instance) == "running"
        && make_seed(api, (owner, config), instance, timeout).is_ok();
    if seeded {
        Ok(())
    } else {
        api.delete_instance(&instance.id)
    }
}
