//! `pause` and `resume`: stop and start the agent37 VM behind a lease.
//!
//! Pause is an agent37 stop and resume is a start. A start is a cold boot and the VM may
//! land on another host, so resume reads a fresh SSH host key instead of trusting the one
//! crabbox saw at acquire.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::agent37::{Agent37, Instance};
use crate::inventory::lease_instance;
use crate::lease::{
    build_lease, fetch_host_key, pause as poll_wait, prefix, proxy_command, settings, status_of,
};
use crate::protocol::{Desired, Lease, Request};

/// Why acquire refuses a stopped VM.
pub(crate) fn stopped(id: &str) -> String {
    format!("instance {id} is stopped; run crabbox resume")
}

/// True for a status in which the VM is not running and will not start by itself.
fn is_stopped(instance: &Instance) -> bool {
    matches!(status_of(instance), "stopped" | "sleeping")
}

/// The checked instance for `op`, or a not-found error.
fn locate(request: &Request, api: &dyn Agent37, owner: &str, op: &str) -> Result<Instance, String> {
    lease_instance(request, api, owner, op)?.ok_or_else(|| {
        let id = request
            .lease
            .as_ref()
            .and_then(|l| l.cloud_id.as_deref())
            .unwrap_or_default();
        format!("instance {id} not found")
    })
}

/// Reads the instance again, or fails once the deadline has passed.
fn refresh(
    api: &dyn Agent37,
    id: &str,
    deadline: Instant,
    secs: u64,
    last: &str,
    want: &str,
) -> Result<Instance, String> {
    if !poll_wait(deadline) {
        return Err(format!(
            "instance {id} not {want} after {secs}s (status {last})"
        ));
    }
    api.get_instance(id).map_err(|e| e.operator_message())
}

/// Stops the VM and waits until it reports stopped.
///
/// # Errors
/// Returns a message when the request does not match an owned VM, the VM failed, or an
/// API call or the wait fails.
pub fn pause(request: &Request, api: &dyn Agent37, diag: &mut dyn Write) -> Result<String, String> {
    let s = settings(&request.config)?;
    let mut instance = locate(request, api, &s.owner, "pause")?;
    let id = instance.id.clone();
    let deadline = Instant::now() + Duration::from_secs(s.timeout);
    let mut stopping = false;
    loop {
        match status_of(&instance) {
            "stopped" | "sleeping" if stopping => return Ok(format!("paused {id}")),
            "stopped" | "sleeping" => return Ok(format!("{id} already stopped")),
            "failed" => return Err(format!("instance {id} failed")),
            "running" if !stopping => {
                let _ = writeln!(diag, "pause: stopping {id}");
                api.stop_instance(&id).map_err(|e| e.operator_message())?;
                stopping = true;
            }
            _ => {}
        }
        let last = status_of(&instance).to_owned();
        instance = refresh(api, &id, deadline, s.timeout, &last, "stopped")?;
    }
}

/// Starts the VM if needed and returns the lease with a fresh host key.
///
/// # Errors
/// Returns a message when the request does not match an owned VM, the VM failed, the
/// lease identity is unavailable, or an API call or the wait fails.
pub fn resume(
    request: &Request,
    api: &dyn Agent37,
    exe: &Path,
    diag: &mut dyn Write,
) -> Result<Lease, String> {
    let s = settings(&request.config)?;
    let mut instance = locate(request, api, &s.owner, "resume")?;
    let id = instance.id.clone();
    let deadline = Instant::now() + Duration::from_secs(s.timeout);
    let mut started = false;
    loop {
        match status_of(&instance) {
            "running" => break,
            "failed" => return Err(format!("instance {id} failed")),
            _ if is_stopped(&instance) && !started => {
                let _ = writeln!(diag, "resume: starting {id}");
                api.start_instance(&id).map_err(|e| e.operator_message())?;
                started = true;
            }
            _ => {}
        }
        let last = status_of(&instance).to_owned();
        instance = refresh(api, &id, deadline, s.timeout, &last, "running")?;
    }
    let _ = writeln!(diag, "resume: reading host key");
    let key = fetch_host_key(api, &id, deadline)?;
    let _ = writeln!(diag, "resume: checking required tools");
    crate::tools::ensure(api, &id, deadline, diag)?;
    let lease = request.lease.as_ref();
    let pick = |given: Option<&String>, meta: &str| {
        given
            .cloned()
            .or_else(|| instance.meta(meta).map(str::to_owned))
    };
    let lease_id = pick(lease.and_then(|l| l.lease_id.as_ref()), "crabbox_lease_id")
        .ok_or("lease id is unavailable for resume")?;
    let slug = pick(lease.and_then(|l| l.slug.as_ref()), "crabbox_slug")
        .ok_or("lease slug is unavailable for resume")?;
    let name = lease
        .and_then(|l| l.name.clone())
        .or_else(|| {
            instance
                .name
                .as_deref()
                .and_then(|n| n.strip_prefix(&prefix(&s.owner)))
                .map(str::to_owned)
        })
        .ok_or("lease name is unavailable for resume")?;
    build_lease(
        Desired {
            lease_id,
            slug,
            name,
            class: None,
            cache_volumes: Vec::new(),
            cache_scope: None,
        },
        &id,
        "ready",
        Some(key),
        proxy_command(exe, s.key_file.as_deref())?,
    )
}
