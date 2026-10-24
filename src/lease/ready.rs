//! Readying an acquired VM, from a fresh create or a claimed seed.

use std::io::Write;
use std::time::{Duration, Instant};

use super::fetch_host_key;
use crate::agent37::{Agent37, Instance};
use crate::cache::Plan;
use crate::protocol::{CacheVolumeEcho, Desired, Lease};

/// The cache volumes of one acquire: the plan, its seed key, and the lease they serve.
pub(super) struct CacheCtx<'a> {
    /// Validated volumes.
    pub(super) plan: Plan,
    /// Seed key of the plan and VM shape.
    pub(super) key: String,
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
        let pair = (&cache.plan, cache.key.as_str());
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

/// Starts and readies a claimed seed; on any failure deletes it so the caller goes cold.
pub(super) fn from_seed(
    api: &dyn Agent37,
    seed: &Instance,
    cache: &CacheCtx<'_>,
    timeout: u64,
    diag: &mut dyn Write,
) -> Option<(String, Vec<CacheVolumeEcho>)> {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let result = crate::cache::start(api, &seed.id, deadline)
        .and_then(|()| ready(api, seed, (true, true), cache, deadline, diag));
    match result {
        Ok(done) => Some(done),
        Err(err) => {
            let _ = writeln!(
                diag,
                "acquire: seed {} failed ({err}); creating cold",
                seed.id
            );
            let _ = api.delete_instance(&seed.id);
            None
        }
    }
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
