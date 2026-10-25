//! Readying an acquired VM, from a fresh create or a claimed seed.

use std::io::Write;
use std::time::{Duration, Instant};

use super::fetch_host_key;
use crate::agent37::{Agent37, Instance};
use crate::cache::{Plan, Taken};
use crate::protocol::{CacheVolumeEcho, Desired, Lease};

/// The cache volumes of one acquire: the plan, its seed key, and the lease they serve.
pub(super) struct CacheCtx<'a> {
    /// Validated volumes.
    pub(super) plan: Plan,
    /// Seed key; `None` without a valid cache scope, so the Box is never seeded.
    pub(super) key: Option<String>,
    /// Owner label.
    pub(super) owner: &'a str,
    /// The lease.
    pub(super) desired: &'a Desired,
}

/// Readies a running VM: tools, cache links, Docker root, host key.
///
/// A failed cache setup fails a claimed seed (`from_seed`) and skips the cache otherwise.
pub(super) fn ready(
    api: &dyn Agent37,
    instance: &Instance,
    (created, from_seed): (bool, bool),
    cache: &CacheCtx<'_>,
    deadline: Instant,
    diag: &mut dyn Write,
) -> Result<(String, Vec<CacheVolumeEcho>), String> {
    let id = &instance.id;
    let _ = writeln!(diag, "acquire: checking required tools");
    crate::tools::ensure(api, id, deadline, diag)?;
    let echo = if cache.plan.is_empty() {
        cache.plan.echo(&[], None)
    } else {
        let pair = (&cache.plan, cache.key.as_deref());
        match crate::cache::setup(
            api,
            instance,
            (cache.owner, cache.desired),
            pair,
            deadline,
            diag,
        ) {
            Ok(echo) => echo,
            Err(err) if from_seed => return Err(err),
            Err(err) => {
                let _ = writeln!(diag, "acquire: cache skipped: {err}");
                cache.plan.all_skipped(&err)
            }
        }
    };
    crate::docker_root::ensure(api, id, created, deadline, diag)?;
    let _ = writeln!(diag, "acquire: reading host key");
    Ok((fetch_host_key(api, id, deadline)?, echo))
}

/// What readying a claimed seed came to.
enum Seeded {
    /// The seed is this lease's Box: its host key and the cache echo.
    Ready(String, Vec<CacheVolumeEcho>),
    /// Another acquire won it inside the Box; the next seed may still be free.
    Lost,
    /// It failed; the lease goes cold.
    Failed,
}

/// Starts, claims and readies a claimed seed. It deletes the seed only when this lease won
/// the claim inside the Box and the seed then failed.
fn from_seed(
    api: &dyn Agent37,
    seed: &Instance,
    (cache, name): (&CacheCtx<'_>, &str),
    timeout: u64,
    diag: &mut dyn Write,
) -> Seeded {
    let Some(key) = cache.key.as_deref() else {
        return Seeded::Failed;
    };
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let lease = (cache.owner, cache.desired, name);
    let taken = crate::cache::take(api, seed, lease, (&cache.plan, key), deadline);
    let result = match taken {
        Ok(Taken::Ours(now)) => ready(api, &now, (true, true), cache, deadline, diag),
        Ok(Taken::Lost) => {
            let _ = writeln!(diag, "acquire: lost seed {} inside the Box", seed.id);
            return Seeded::Lost;
        }
        Ok(Taken::Failed(err)) => {
            let _ = writeln!(
                diag,
                "acquire: seed {} failed ({err}); creating cold",
                seed.id
            );
            return Seeded::Failed;
        }
        Err(err) => Err(err),
    };
    match result {
        Ok((host_key, echo)) => Seeded::Ready(host_key, echo),
        Err(err) => {
            let _ = writeln!(
                diag,
                "acquire: seed {} failed ({err}); deleting it, creating cold",
                seed.id
            );
            let _ = api.delete_instance(&seed.id);
            Seeded::Failed
        }
    }
}

/// Claims the seeds of the cache key newest first until one becomes this lease's Box, and
/// readies it; returns its id, host key and cache echo. `None` means create cold: no key, no
/// free seed, or a seed that failed. Another acquire winning a seed passes to the next one.
pub(super) fn restore(
    api: &dyn Agent37,
    (cache, name): (&CacheCtx<'_>, &str),
    timeout: u64,
    diag: &mut dyn Write,
) -> Option<(String, String, Vec<CacheVolumeEcho>)> {
    let key = cache.key.as_deref()?;
    let pair = (&cache.plan, key);
    while let Some(seed) = crate::cache::claim(api, cache.owner, cache.desired, name, pair, diag) {
        match from_seed(api, &seed, (cache, name), timeout, diag) {
            Seeded::Ready(host_key, echo) => {
                let gen = crate::cache::seed_gen(&seed).unwrap_or("0");
                let _ = writeln!(diag, "acquire: restored from seed gen {gen}");
                return Some((seed.id, host_key, echo));
            }
            Seeded::Lost => {}
            Seeded::Failed => break,
        }
    }
    None
}

/// `lease` carrying the cache echo.
pub(super) fn with_echo(
    lease: Result<Lease, String>,
    echo: Vec<CacheVolumeEcho>,
) -> Result<Lease, String> {
    lease.map(|mut l| {
        l.cache_volumes = echo;
        l
    })
}
