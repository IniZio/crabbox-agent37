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
use crate::cache::{Plan, Shape};
use crate::host_key;
use crate::protocol::{Desired, Lease, Request, Ssh};
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
pub(crate) fn find_by_name(api: &dyn Agent37, name: &str) -> Result<Option<Instance>, String> {
    let list = api.list_instances().map_err(|e| e.operator_message())?;
    let mut matches = list
        .into_iter()
        .filter(|i| i.name.as_deref() == Some(name) && !crate::cache::is_seed(i));
    let found = matches.next();
    if matches.next().is_some() {
        return Err(format!("more than one instance is named {name}"));
    }
    match found {
        Some(i) if i.kind.as_deref() != Some("vm") => Err(format!("instance {} is not a VM", i.id)),
        other => Ok(other),
    }
}

/// Creates the VM for the lease with the given shape.
fn create(
    api: &dyn Agent37,
    settings: &Settings,
    desired: &Desired,
    name: &str,
    shape: &Shape<'_>,
    diag: &mut dyn Write,
) -> Result<Instance, String> {
    let disk_gb = shape.disk_gb;
    let spec = CreateSpec {
        name: name.to_owned(),
        region: settings.region.clone(),
        cpu: shape.cpu,
        memory_gb: shape.memory_gb,
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
    Ok(created)
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
        cache_volumes: Vec::new(),
    })
}

/// Finds or creates the VM for `request.desired` and waits until it is reachable.
///
/// With cache volumes and no VM yet, the newest seed of the key is claimed first.
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
    let (cpu, memory_gb) = class_size(desired.class.as_deref())?;
    let disk_gb = disk_for(s.disk_source, s.disk_gb, cpu, memory_gb)?;
    let shape = Shape {
        cpu,
        memory_gb,
        disk_gb,
        template: s.template.as_deref(),
    };
    let plan = Plan::new(&desired.cache_volumes);
    let cache = CacheCtx {
        key: plan.key(&shape),
        plan,
        owner: &s.owner,
        desired,
    };
    let existing = find_by_name(api, &name)?;
    if existing.is_none() && !cache.plan.is_empty() {
        let pair = (&cache.plan, cache.key.as_str());
        if let Some(seed) = crate::cache::claim(api, &s.owner, desired, &name, pair, diag) {
            if let Some((key, echo)) = from_seed(api, &seed, &cache, s.timeout, diag) {
                return with_echo(
                    build_lease(desired.clone(), &seed.id, "ready", Some(key), proxy),
                    echo,
                );
            }
        }
    }
    let (instance, created) = match existing {
        Some(found) => (found, false),
        None => (create(api, &s, desired, &name, &shape, diag)?, true),
    };
    let id = instance.id.clone();
    let deadline = Instant::now() + Duration::from_secs(s.timeout);
    let ready = wait_running(api, instance.clone(), deadline, s.timeout)
        .and_then(|()| ready(api, &instance, (created, false), &cache, deadline, diag));
    match ready {
        Ok((key, echo)) => with_echo(
            build_lease(desired.clone(), &id, "ready", Some(key), proxy),
            echo,
        ),
        Err(err) => Err(cleanup(api, &id, created, err)),
    }
}

mod ready;
mod resolve;
use ready::{from_seed, ready, with_echo, CacheCtx};
pub use resolve::resolve;
