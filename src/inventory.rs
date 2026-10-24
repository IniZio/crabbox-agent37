//! Owner-scoped list, release and cleanup over a workspace shared with teammates.
//!
//! Every operation filters on [`owned`] first, so another owner's instance is
//! never listed, matched or deleted.

use crate::agent37::{Agent37, ApiError, Instance};
use crate::lease::{prefix, settings};
use crate::protocol::{LeaseRow, Request};

/// True only when both the name prefix and the owner metadata match.
#[must_use]
pub fn owned(instance: &Instance, owner: &str) -> bool {
    instance
        .name
        .as_deref()
        .is_some_and(|n| n.starts_with(&prefix(owner)))
        && instance.meta("crabbox_owner") == Some(owner)
}

/// The listable identity of an owned VM, or `None` when it has none.
fn row(instance: &Instance, owner: &str) -> Option<LeaseRow> {
    if !owned(instance, owner)
        || instance.kind.as_deref() != Some("vm")
        || crate::cache::is_seed(instance)
    {
        return None;
    }
    let name = instance.name.as_deref()?.strip_prefix(&prefix(owner))?;
    if name.is_empty() {
        return None;
    }
    let status = match instance.status.as_deref() {
        Some("running") => "ready",
        Some("stopped" | "sleeping") => "stopped",
        Some(other) => other,
        None => "unknown",
    };
    Some(LeaseRow {
        lease_id: instance.meta("crabbox_lease_id")?.to_owned(),
        slug: instance.meta("crabbox_slug")?.to_owned(),
        name: name.to_owned(),
        cloud_id: instance.id.clone(),
        status: status.to_owned(),
    })
}

/// True for an owned instance that lacks the identity `list` needs; a seed has none by design.
fn unlistable(instance: &Instance, owner: &str) -> bool {
    owned(instance, owner)
        && !crate::cache::is_seed(instance)
        && (instance.meta("crabbox_lease_id").is_none()
            || instance.meta("crabbox_slug").is_none()
            || instance
                .name
                .as_deref()
                .and_then(|n| n.strip_prefix(&prefix(owner)))
                .is_none_or(str::is_empty))
}

/// Lists this owner's VM leases, sorted by name then cloud id.
///
/// # Errors
/// Returns the operator message when config or the API call fails.
pub fn list(request: &Request, api: &dyn Agent37) -> Result<Vec<LeaseRow>, String> {
    let owner = settings(&request.config)?.owner;
    let instances = api.list_instances().map_err(|e| e.operator_message())?;
    let mut rows: Vec<LeaseRow> = instances.iter().filter_map(|i| row(i, &owner)).collect();
    rows.sort_by(|a, b| (&a.name, &a.cloud_id).cmp(&(&b.name, &b.cloud_id)));
    Ok(rows)
}

/// A 404 means the instance is already gone, which counts as success.
fn tolerate_missing(result: Result<(), ApiError>) -> Result<(), String> {
    match result {
        Ok(()) | Err(ApiError::Http { status: 404, .. }) => Ok(()),
        Err(e) => Err(e.operator_message()),
    }
}

/// Finds the instance a request names and checks it belongs to this owner and lease.
///
/// Returns `None` when the instance no longer exists. Changes nothing remotely.
///
/// # Errors
/// Returns a message when the request is incomplete or does not match the instance.
pub(crate) fn lease_instance(
    request: &Request,
    api: &dyn Agent37,
    owner: &str,
    op: &str,
) -> Result<Option<Instance>, String> {
    let lease = request.lease.as_ref();
    let id = lease
        .and_then(|l| l.cloud_id.as_deref())
        .ok_or_else(|| format!("{op} requires lease.cloudId"))?;
    let expected = request.expected.as_ref();
    if let Some(want) = expected.and_then(|e| e.cloud_id.as_deref()) {
        if want != id {
            return Err(format!(
                "expected cloudId {want} does not match lease cloudId {id}"
            ));
        }
    }
    let instance = match api.get_instance(id) {
        Ok(instance) => instance,
        Err(ApiError::Http { status: 404, .. }) => return Ok(None),
        Err(e) => return Err(e.operator_message()),
    };
    if instance.id != id {
        return Err(format!("instance {id} resolved to {}", instance.id));
    }
    if owned(&instance, owner) && crate::cache::is_seed(&instance) {
        return Ok(None);
    }
    if !owned(&instance, owner) {
        return Err(format!("instance {id} belongs to another owner"));
    }
    if instance.kind.as_deref() != Some("vm") {
        return Err(format!("instance {id} is not a VM"));
    }
    if let Some(name) = lease.and_then(|l| l.name.as_deref()) {
        if instance.name.as_deref() != Some(format!("{}{name}", prefix(owner)).as_str()) {
            return Err(format!("instance {id} does not match lease name {name}"));
        }
    }
    let actual = instance.meta("crabbox_lease_id");
    let wanted = lease
        .and_then(|l| l.lease_id.as_deref())
        .into_iter()
        .chain(expected.and_then(|e| e.lease_id.as_deref()));
    for want in wanted {
        if actual != Some(want) {
            return Err(format!("instance {id} does not match lease id {want}"));
        }
    }
    Ok(Some(instance))
}

/// Releases exactly the requested instance after checking it belongs to this lease: a
/// running Box with cache volumes is kept stopped as a seed, anything else is deleted.
///
/// # Errors
/// Returns a message, without deleting, when the request does not match the instance.
pub fn release(request: &Request, api: &dyn Agent37) -> Result<(), String> {
    let s = settings(&request.config)?;
    match lease_instance(request, api, &s.owner, "release")? {
        Some(instance) => {
            tolerate_missing(crate::cache::release(api, &s.owner, &instance, s.timeout))
        }
        None => Ok(()),
    }
}

/// Deletes owned instances `list` cannot report, and expired or surplus seeds.
///
/// crabbox can never release an instance with no lease id, slug or name; an interrupted
/// acquire or manual create leaves them. A seed past `cache.ttlDays` or beyond the newest
/// `cache.maxSeeds` goes too, if a fresh read still shows it a stopped seed.
///
/// # Errors
/// Returns the operator message when config or an API call fails.
pub fn cleanup(request: &Request, api: &dyn Agent37) -> Result<String, String> {
    let owner = settings(&request.config)?.owner;
    let instances = api.list_instances().map_err(|e| e.operator_message())?;
    let mut ids: Vec<String> = instances
        .iter()
        .filter(|i| unlistable(i, &owner))
        .map(|i| i.id.clone())
        .collect();
    let seeds: Vec<String> = crate::cache::gc(&instances, &owner, &request.config)?
        .into_iter()
        .map(str::to_owned)
        .collect();
    if ids.is_empty() && seeds.is_empty() {
        return Ok("nothing to clean up".to_owned());
    }
    let verb = if request.dry_run {
        ids.extend(seeds);
        "would delete"
    } else {
        for id in &ids {
            tolerate_missing(api.delete_instance(id))?;
        }
        ids.extend(crate::cache::delete_seeds(api, &owner, &seeds, None));
        "deleted"
    };
    ids.sort_unstable();
    Ok(format!(
        "{verb} {} instance(s): {}",
        ids.len(),
        ids.join(", ")
    ))
}

#[cfg(test)]
mod tests;
