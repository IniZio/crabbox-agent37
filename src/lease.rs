//! `acquire` and `resolve`: an idempotent agent37 VM lease for a crabbox lease.
//!
//! No agent37 endpoint exposes the SSH host key, so it is read as root over exec from the
//! `HostKey` named in the sshd config under `/.agent37/etc`. When no `.pub` exists the public
//! key is taken from the private key file on the VM (only the public part is returned);
//! `/etc/ssh` is the fallback.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::agent37::{Agent37, ApiError, CreateSpec, Instance};
use crate::host_key;
use crate::protocol::{Desired, Expected, Lease, Request, Ssh};
pub(crate) use crate::settings::{disk_for, label_ok, settings, Settings};

/// Remote check that the tools crabbox needs are installed.
const READY_CHECK: &str = "command -v bash >/dev/null && command -v git >/dev/null && command -v rsync >/dev/null && command -v tar >/dev/null";
/// Pause between status and exec polls.
const POLL: Duration = Duration::from_secs(1);

/// Prefix shared by every instance of one owner.
pub(crate) fn prefix(owner: &str) -> String {
    format!("crabbox-{owner}--")
}

/// vCPU and memory (GB) for a crabbox class.
fn class_size(class: Option<&str>) -> Result<(u32, u32), String> {
    match class {
        None | Some("standard") => Ok((2, 4)),
        Some("fast") => Ok((4, 8)),
        Some("large") => Ok((8, 16)),
        Some("beast") => Ok((16, 32)),
        Some(other) => Err(format!(
            "unknown class {other}: use standard, fast, large or beast"
        )),
    }
}

/// `p` bare when it only has safe characters, else POSIX single-quoted.
fn quote(p: &str) -> String {
    let safe = !p.is_empty()
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._+-".contains(c));
    if safe {
        p.to_owned()
    } else {
        format!("'{}'", p.replace('\'', "'\\''"))
    }
}

/// ssh `ProxyCommand` that tunnels through the adapter.
pub(crate) fn proxy_command(exe: &Path, key_file: Option<&str>) -> Result<String, String> {
    let exe = exe.to_str().ok_or("adapter path is not valid UTF-8")?;
    let Some(key_file) = key_file else {
        return Ok(format!("{} tunnel %h", quote(exe)));
    };
    let abs = if Path::new(key_file).is_absolute() {
        key_file.to_owned()
    } else {
        std::fs::canonicalize(key_file)
            .map_err(|e| format!("config.keyFile cannot be resolved: {e}"))?
            .to_str()
            .ok_or("config.keyFile path is not valid UTF-8")?
            .to_owned()
    };
    Ok(format!(
        "env AGENT37_KEY_FILE={} {} tunnel %h",
        quote(&abs),
        quote(exe)
    ))
}

/// Status string of an instance.
pub(crate) fn status_of(instance: &Instance) -> &str {
    instance.status.as_deref().unwrap_or("unknown")
}

/// Finds the single `vm` instance with exactly `name`.
fn find_by_name(api: &dyn Agent37, name: &str) -> Result<Option<Instance>, String> {
    let list = api.list_instances().map_err(|e| e.operator_message())?;
    let mut matches = list.into_iter().filter(|i| i.name.as_deref() == Some(name));
    let found = matches.next();
    if matches.next().is_some() {
        return Err(format!("more than one instance is named {name}"));
    }
    match found {
        Some(i) if i.kind.as_deref() != Some("vm") => Err(format!("instance {} is not a VM", i.id)),
        other => Ok(other),
    }
}

/// Existing instance for the lease, or a newly created one (`true`).
fn find_or_create(
    api: &dyn Agent37,
    settings: &Settings,
    desired: &Desired,
    name: &str,
    diag: &mut dyn Write,
) -> Result<(Instance, bool), String> {
    let (cpu, memory_gb) = class_size(desired.class.as_deref())?;
    let disk_gb = disk_for(settings.disk_source, settings.disk_gb, cpu, memory_gb)?;
    if let Some(existing) = find_by_name(api, name)? {
        return Ok((existing, false));
    }
    let spec = CreateSpec {
        name: name.to_owned(),
        region: settings.region.clone(),
        cpu,
        memory_gb,
        template: settings.template.clone(),
        disk_gb,
        metadata: vec![
            ("crabbox_owner".to_owned(), settings.owner.clone()),
            ("crabbox_lease_id".to_owned(), desired.lease_id.clone()),
            ("crabbox_slug".to_owned(), desired.slug.clone()),
        ],
    };
    let created = match api.create_instance(&spec) {
        Ok(created) => created,
        Err(
            e @ (ApiError::Unreachable(_)
            | ApiError::Decode(_)
            | ApiError::Http {
                status: 500..=599, ..
            }),
        ) => {
            return Err(reap_unconfirmed(api, name, e.operator_message()));
        }
        Err(e) => return Err(e.operator_message()),
    };
    let _ = writeln!(
        diag,
        "acquire: created {} with a {disk_gb} GB disk",
        created.id
    );
    Ok((created, true))
}

/// Deletes any instance named `name` after a create whose outcome is unknown.
///
/// A timeout, a 5xx or an unparsable reply does not prove agent37 refused the request; the
/// instance may exist with no lease pointing at it.
fn reap_unconfirmed(api: &dyn Agent37, name: &str, err: String) -> String {
    let list = match api.list_instances() {
        Ok(list) => list,
        Err(e) => {
            return format!(
                "{err}; the instance may exist, and listing to find it failed: {}",
                e.operator_message()
            );
        }
    };
    let mut note = err;
    for stray in list.into_iter().filter(|i| i.name.as_deref() == Some(name)) {
        note = cleanup(api, &stray.id, true, note);
    }
    note
}

/// Polls until the instance runs; fails on `failed`, `stopped` or the deadline.
fn wait_running(
    api: &dyn Agent37,
    first: Instance,
    deadline: Instant,
    secs: u64,
) -> Result<(), String> {
    let mut instance = first;
    loop {
        match status_of(&instance) {
            "running" => return Ok(()),
            "failed" => return Err(format!("instance {} failed", instance.id)),
            "stopped" => return Err(crate::power::stopped(&instance.id)),
            _ => {}
        }
        if !pause(deadline) {
            return Err(format!(
                "instance {} not running after {secs}s (status {})",
                instance.id,
                status_of(&instance)
            ));
        }
        instance = api
            .get_instance(&instance.id)
            .map_err(|e| e.operator_message())?;
    }
}

/// Sleeps one poll interval or the time left; false when the deadline has passed.
pub(crate) fn pause(deadline: Instant) -> bool {
    let remaining = deadline.saturating_duration_since(Instant::now());
    std::thread::sleep(POLL.min(remaining));
    !remaining.is_zero()
}

/// Host key from one exec call.
fn read_host_key(api: &dyn Agent37, id: &str) -> Result<Result<String, String>, ApiError> {
    let out = api.exec_as_root(id, &host_key::production())?;
    if out.exit_code != 0 {
        return Ok(Err(format!(
            "reading host key on {id} exited {}: {}",
            out.exit_code,
            out.stderr.trim()
        )));
    }
    let mut fields = out.stdout.split_whitespace();
    Ok(match (fields.next(), fields.next()) {
        (Some(kind), Some(data)) if kind.starts_with("ssh-ed25519") => Ok(format!("{kind} {data}")),
        _ => Err(format!("host key of {id} is not an ssh-ed25519 key")),
    })
}

/// Host key, retrying API errors every second until the deadline.
pub(crate) fn fetch_host_key(api: &dyn Agent37, id: &str, end: Instant) -> Result<String, String> {
    loop {
        match read_host_key(api, id) {
            Ok(parsed) => return parsed,
            Err(err) if !pause(end) => {
                return Err(format!("host key unavailable: {}", err.operator_message()));
            }
            Err(_) => {}
        }
    }
}

/// Appends what happened to a VM created by a failed acquire.
fn cleanup(api: &dyn Agent37, id: &str, created: bool, err: String) -> String {
    if !created {
        return err;
    }
    match api.delete_instance(id) {
        Ok(()) => format!("{err}; instance {id} deleted"),
        Err(e) => format!(
            "{err}; deleting instance {id} also failed: {}",
            e.operator_message()
        ),
    }
}

/// Builds the lease for `id`.
pub(crate) fn build_lease(
    ident: Desired,
    id: &str,
    status: &str,
    host_key: Option<String>,
    proxy_command: String,
) -> Result<Lease, String> {
    if !label_ok(id) {
        return Err(format!("instance id {id} is not a valid host label"));
    }
    Ok(Lease {
        lease_id: ident.lease_id,
        slug: ident.slug,
        name: ident.name,
        cloud_id: id.to_owned(),
        status: status.to_owned(),
        ssh: Ssh {
            user: "root".to_owned(),
            host: format!("{id}.agent37.app"),
            port: "22".to_owned(),
            proxy_command,
            ready_check: READY_CHECK.to_owned(),
            host_key,
        },
    })
}

/// Finds or creates the VM for `request.desired` and waits until it is reachable.
pub fn acquire(
    request: &Request,
    api: &dyn Agent37,
    exe: &Path,
    diag: &mut dyn Write,
) -> Result<Lease, String> {
    let s = settings(&request.config)?;
    let desired = request
        .desired
        .as_ref()
        .ok_or("acquire requires a desired lease")?;
    let proxy = proxy_command(exe, s.key_file.as_deref())?;
    let name = format!("{}{}", prefix(&s.owner), desired.name);
    let (instance, created) = find_or_create(api, &s, desired, &name, diag)?;
    let id = instance.id.clone();
    let deadline = Instant::now() + Duration::from_secs(s.timeout);
    let ready = wait_running(api, instance, deadline, s.timeout).and_then(|()| {
        let _ = writeln!(diag, "acquire: checking required tools");
        crate::tools::ensure(api, &id, deadline, diag)?;
        crate::docker_root::ensure(api, &id, created, deadline, diag)?;
        let _ = writeln!(diag, "acquire: reading host key");
        fetch_host_key(api, &id, deadline)
    });
    match ready {
        Ok(key) => build_lease(desired.clone(), &id, "ready", Some(key), proxy),
        Err(err) => Err(cleanup(api, &id, created, err)),
    }
}

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
) -> Result<Instance, String> {
    let derived = desired.map(|d| format!("{}{}", prefix(owner), d.name));
    let Some(cloud_id) = expected.cloud_id.as_deref() else {
        let name = derived.ok_or("resolve needs a cloud id or a desired lease")?;
        return find_by_name(api, &name)?.ok_or_else(|| format!("instance {name} not found"));
    };
    let instance = api.get_instance(cloud_id).map_err(|e| match e {
        ApiError::Http { status: 404, .. } => format!("instance {cloud_id} not found"),
        other => other.operator_message(),
    })?;
    if !instance
        .name
        .as_deref()
        .is_some_and(|n| n.starts_with(&prefix(owner)))
    {
        return Err(format!("instance {cloud_id} belongs to another owner"));
    }
    if let Some(name) = derived {
        let found = find_by_name(api, &name)?.map(|i| i.id);
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
    _diag: &mut dyn Write,
) -> Result<Lease, String> {
    let s = settings(&request.config)?;
    let desired = request.desired.as_ref();
    let expected = request.expected.clone().unwrap_or_default();
    let proxy = proxy_command(exe, s.key_file.as_deref())?;
    let instance = locate(api, &s.owner, desired, &expected)?;
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
    };
    build_lease(ident, &instance.id, status, host_key, proxy)
}
