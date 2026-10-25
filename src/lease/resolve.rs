//! `resolve`: looks up the existing VM of a lease; never creates or deletes.

use std::io::Write;
use std::path::Path;

use super::{build_lease, find_by_name, prefix, proxy_command, read_host_key, settings, status_of};
use crate::agent37::{Agent37, ApiError, Instance};
use crate::protocol::{Desired, Expected, Lease, Request};

/// The one value among `values` that are present; they must agree.
fn agree(label: &str, values: &[Option<&str>]) -> Result<String, String> {
    let mut present = values.iter().flatten();
    let first = present
        .next()
        .ok_or_else(|| format!("resolve needs a {label}"))?;
    if present.any(|v| v != first) {
        return Err(format!("{label} differs between expected and desired"));
    }
    Ok((*first).to_owned())
}

/// The instance an expected and/or desired lease points at.
fn locate(
    api: &dyn Agent37,
    owner: &str,
    desired: Option<&Desired>,
    expected: &Expected,
    diag: &mut dyn Write,
) -> Result<Instance, String> {
    let derived = desired.map(|d| format!("{}{}", prefix(owner), d.name));
    let Some(cloud_id) = expected.cloud_id.as_deref() else {
        let name = derived.ok_or("resolve needs a cloud id or a desired lease")?;
        return find_by_name(api, &name, diag)?.ok_or_else(|| format!("instance {name} not found"));
    };
    let instance = api.get_instance(cloud_id).map_err(|e| match e {
        ApiError::Http { status: 404, .. } => format!("instance {cloud_id} not found"),
        other => other.operator_message(),
    })?;
    if crate::cache::is_seed(&instance) {
        return Err(format!("instance {cloud_id} not found"));
    }
    if !instance
        .name
        .as_deref()
        .is_some_and(|n| n.starts_with(&prefix(owner)))
    {
        return Err(format!("instance {cloud_id} belongs to another owner"));
    }
    if let Some(name) = derived {
        let found = find_by_name(api, &name, diag)?.map(|i| i.id);
        if found.as_deref() != Some(cloud_id) {
            return Err(format!(
                "instance id changed: expected {cloud_id}, found {}",
                found.as_deref().unwrap_or("none")
            ));
        }
    }
    Ok(instance)
}

/// Looks up the existing VM for a lease; never creates or deletes.
///
/// A running VM's host key is read with one exec, because crabbox prints accept-new ssh
/// options for a lease whose pin it does not hold locally, such as one acquired from another
/// crabbox state dir.
pub fn resolve(
    request: &Request,
    api: &dyn Agent37,
    exe: &Path,
    diag: &mut dyn Write,
) -> Result<Lease, String> {
    let s = settings(&request.config)?;
    let desired = request.desired.as_ref();
    let expected = request.expected.clone().unwrap_or_default();
    let proxy = proxy_command(exe, s.key_file.as_deref())?;
    let instance = locate(api, &s.owner, desired, &expected, diag)?;
    let lease_id = agree(
        "lease id",
        &[
            expected.lease_id.as_deref(),
            expected.attempt_lease_id.as_deref(),
            desired.map(|d| d.lease_id.as_str()),
        ],
    )?;
    let slug = agree(
        "slug",
        &[expected.slug.as_deref(), desired.map(|d| d.slug.as_str())],
    )?;
    let full = instance.name.as_deref().unwrap_or_default();
    let stripped = full.strip_prefix(&prefix(&s.owner)).unwrap_or(full);
    let name = desired.map_or(stripped, |d| &d.name);
    let (status, host_key) = match status_of(&instance) {
        "running" if request.release_only => ("ready", None),
        "running" => {
            let key = read_host_key(api, &instance.id)
                .map_err(|e| format!("host key unavailable: {}", e.operator_message()))??;
            ("ready", Some(key))
        }
        "stopped" | "sleeping" => ("stopped", None),
        other => (other, None),
    };
    let name = name.to_owned();
    let ident = Desired {
        lease_id,
        slug,
        name,
        class: None,
        cache_volumes: Vec::new(),
        cache_scope: None,
    };
    build_lease(ident, &instance.id, status, host_key, proxy)
}
