//! Fake `PATCH` and the cache scripts, run by bash against a scratch root per instance.
//!
//! A script's `P=;` becomes `P=<root>;`, so every path it touches lands under that root. A
//! guard directory first on `PATH` stands in for `docker`, `sudo`, `apt-get`, `systemctl`,
//! `pidof`, `mountpoint` and `umount`, so no script reaches the host's daemons, mounts or
//! package manager.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use super::{nested_error, FakeAgent37, State};

/// Commands the guard directory replaces; each fails without touching the host.
const GUARDED: [&str; 7] = [
    "docker",
    "sudo",
    "apt-get",
    "systemctl",
    "pidof",
    "mountpoint",
    "umount",
];

/// Writes the guard directory under `base` and returns it.
pub(super) fn guards(base: &Path) -> PathBuf {
    let dir = base.join("guards");
    std::fs::create_dir_all(&dir).unwrap();
    for name in GUARDED {
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!("#!/bin/sh\necho \"guard: {name} $*\" >&2\nexit 1\n"),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

/// Runs a cache script for instance `id` under its scratch root.
pub(super) fn run_script(state: &State, id: &str, command: &str) -> Value {
    let root = state.boxes.path().join(id);
    std::fs::create_dir_all(&root).unwrap();
    let text = command.replacen("P=;", &format!("P='{}';", root.display()), 1);
    let path = format!(
        "{}:{}",
        state.boxes.path().join("guards").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new("bash")
        .arg("-c")
        .arg(text)
        .env("PATH", path)
        .env("DOCKER_HOST", "unix:///nonexistent/moor-test")
        .env("CONTAINER_HOST", "unix:///nonexistent/moor-test")
        .output()
        .unwrap();
    json!({
        "exit_code": out.status.code().unwrap_or(-1),
        "stdout": String::from_utf8_lossy(&out.stdout),
        "stderr": String::from_utf8_lossy(&out.stderr),
    })
}

/// Handles `PATCH /v1/instances/{id}`: sets `name` and merges `metadata` into the stored
/// keys, as agent37 does.
pub(super) fn patch(state: &mut State, id: &str, body: &str) -> (u16, Value) {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let steal = state.steal_claims;
    let Some(instance) = state.instances.iter_mut().find(|i| i["id"] == id) else {
        return (404, nested_error("not_found", "Instance not found."));
    };
    let was_seed = instance["metadata"]["crabbox_role"] == "seed";
    if let Some(name) = parsed.get("name") {
        instance["name"] = name.clone();
    }
    if let Some(Value::Object(metadata)) = parsed.get("metadata") {
        if !instance["metadata"].is_object() {
            instance["metadata"] = json!({});
        }
        for (k, v) in metadata {
            instance["metadata"][k] = v.clone();
        }
    }
    if steal && was_seed {
        let owner = instance["metadata"]["crabbox_owner"].clone();
        instance["name"] = json!(format!("crabbox-{}--rival", owner.as_str().unwrap_or("")));
        instance["metadata"]["crabbox_lease_id"] = json!("cbx_rival");
    }
    (200, instance.clone())
}

impl FakeAgent37 {
    /// Scratch root standing in for instance `id`'s filesystem.
    #[allow(dead_code, reason = "each test binary uses a different subset")]
    pub(crate) fn box_root(&self, id: &str) -> PathBuf {
        let root = self.lock().boxes.path().join(id);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// Makes a rival acquire claim every seed right after this adapter's claim lands.
    #[allow(dead_code, reason = "each test binary uses a different subset")]
    pub(crate) fn set_steal_claims(&self, steal: bool) {
        self.lock().steal_claims = steal;
    }
}
