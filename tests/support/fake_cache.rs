//! Fake `PATCH` and the cache scripts, run by bash against a scratch root per instance.
//!
//! A script's `P=;` becomes `P=<root>;`, so every path it touches lands under that root. A
//! guard directory first on `PATH` stands in for `docker`, `sudo`, `apt-get`, `systemctl` and
//! `pidof`, which fail, and for `mountpoint`, `umount`, `rm` and `fsfreeze`, which act on files
//! in the root: `.mounts` lists the paths that are mount points, which `rm` refuses as busy,
//! and `.unmounted` and `.frozen` record calls. No script reaches the host's daemons, mounts or
//! package manager.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{nested_error, FakeAgent37, State};

/// Commands the guard directory replaces; each fails without touching the host.
const GUARDED: [&str; 5] = ["docker", "sudo", "apt-get", "systemctl", "pidof"];

/// `(command, script)` of the stand-ins that act on the instance root `$FAKE_BOX`.
const STAND_INS: [(&str, &str); 4] = [
    (
        "mountpoint",
        r#"[ "$1" = -q ] && shift; grep -qxF "$1" "$FAKE_BOX/.mounts" 2>/dev/null"#,
    ),
    (
        "umount",
        r#"[ "$1" = -l ] && shift; echo "$1" >> "$FAKE_BOX/.unmounted"; grep -vxF "$1" "$FAKE_BOX/.mounts" > "$FAKE_BOX/.mounts.new"; mv "$FAKE_BOX/.mounts.new" "$FAKE_BOX/.mounts""#,
    ),
    (
        "rm",
        r#"for a; do if grep -qxF -- "$a" "$FAKE_BOX/.mounts" 2>/dev/null; then echo "rm: cannot remove '$a': Device or resource busy" >&2; exit 1; fi; done; exec /bin/rm "$@""#,
    ),
    ("fsfreeze", r#"echo "$*" >> "$FAKE_BOX/.frozen""#),
];

/// Writes the guard directory under `base` and returns it.
pub(super) fn guards(base: &Path) -> PathBuf {
    let dir = base.join("guards");
    std::fs::create_dir_all(&dir).unwrap();
    let guarded = GUARDED
        .iter()
        .map(|name| (*name, format!("echo \"guard: {name} $*\" >&2; exit 1")));
    let stand_ins = STAND_INS.iter().map(|(n, body)| (*n, (*body).to_owned()));
    for (name, body) in guarded.chain(stand_ins) {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
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
        .env("FAKE_BOX", &root)
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
    if state.rivals.patch_blocked.as_deref() == Some(id) {
        return (500, nested_error("internal", "Patch failed."));
    }
    let lost = match state.rivals.patch_lost.take() {
        Some((lost, then_fail)) if lost == id => Some(then_fail),
        other => {
            state.rivals.patch_lost = other;
            None
        }
    };
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let steal = state.rivals.steal_claims;
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
    let answer = instance.clone();
    match lost {
        Some(then_fail) => {
            if then_fail {
                state.rivals.patch_blocked = Some(id.to_owned());
            }
            (504, nested_error("timeout", "Patch timed out."))
        }
        None => (200, answer),
    }
}

/// Gives instance `id` to a rival lease that claimed and started it, inside the Box too.
fn rival_claim(state: &mut State, id: &str) {
    let run = state.boxes.path().join(id).join("run");
    std::fs::create_dir_all(&run).unwrap();
    let _ = std::os::unix::fs::symlink("cbx_rival", run.join("crabbox-claim"));
    if let Some(i) = state.instances.iter_mut().find(|i| i["id"] == id) {
        i["name"] = json!("crabbox-me--rival");
        i["metadata"]["crabbox_lease_id"] = json!("cbx_rival");
        i["metadata"]["crabbox_role"] = json!("lease");
        i["status"] = json!("running");
    }
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
        self.lock().rivals.steal_claims = steal;
    }
}

/// Rival acquires a test stages.
#[derive(Default)]
pub(super) struct Rivals {
    /// Whether a rival claims each seed right after this adapter does.
    steal_claims: bool,
    /// Two acquires racing for one seed, when armed.
    race: Option<Race>,
    /// Seed a rival claims and starts right after the given count of further lists.
    taken_after_list: Option<(String, u32)>,
    /// How the next `start` fails, when armed.
    start_fault: Option<StartFault>,
    /// Instance whose `PATCH`es fail.
    patch_blocked: Option<String>,
    /// Instance whose next `PATCH` lands but answers a timeout, and whether later ones fail.
    patch_lost: Option<(String, bool)>,
    /// Whether every claim inside a Box fails.
    claim_fails: bool,
    /// Instance whose `stop`s fail, and whether a rival claims it at the first one.
    stop_blocked: Option<(String, bool)>,
}

/// How the fake fails the next `start`: the Box reports `failed` and the call answers 500.
#[derive(Clone, Copy)]
#[allow(dead_code, reason = "each test binary uses a different subset")]
pub(crate) enum StartFault {
    /// Nothing else happens.
    Fails,
    /// Every later `PATCH` of that Box fails too.
    ThenPatchesFail,
    /// A rival lease claimed the Box just before.
    AfterRivalClaim,
    /// The start succeeds, but every claim inside the Box fails.
    ClaimFails,
    /// As [`StartFault::ClaimFails`], and every later `stop` of that Box fails.
    ClaimFailsThenStopFails,
    /// As [`StartFault::ClaimFailsThenStopFails`], and a rival claims the Box at that stop.
    ClaimFailsThenRivalAtStop,
}

/// The answer to an `exec` of `command` that the armed faults refuse.
pub(super) fn refused_exec(state: &State, command: &str) -> Option<(u16, Value)> {
    (state.rivals.claim_fails && command.starts_with(": crabbox-cache-claim;"))
        .then(|| (502, nested_error("exec_failed", "Exec failed.")))
}

/// The answer to a `stop` of `id` that the armed faults refuse.
pub(super) fn refused_stop(state: &mut State, id: &str) -> Option<(u16, Value)> {
    let (blocked, rival) = state.rivals.stop_blocked.as_mut()?;
    if blocked != id {
        return None;
    }
    if std::mem::take(rival) {
        rival_claim(state, id);
    }
    Some((500, nested_error("internal", "Stop failed.")))
}

/// Fails `start` of `id` when a fault is armed; `None` lets it start.
pub(super) fn start(state: &mut State, id: &str) -> Option<(u16, Value)> {
    let fault = state.rivals.start_fault.take()?;
    match fault {
        StartFault::ClaimFails => {
            state.rivals.claim_fails = true;
            return None;
        }
        StartFault::ClaimFailsThenStopFails | StartFault::ClaimFailsThenRivalAtStop => {
            state.rivals.claim_fails = true;
            let rival = matches!(fault, StartFault::ClaimFailsThenRivalAtStop);
            state.rivals.stop_blocked = Some((id.to_owned(), rival));
            return None;
        }
        _ => {}
    }
    let instance = state.instances.iter_mut().find(|i| i["id"] == id)?;
    instance["status"] = json!("failed");
    match fault {
        StartFault::Fails
        | StartFault::ClaimFails
        | StartFault::ClaimFailsThenStopFails
        | StartFault::ClaimFailsThenRivalAtStop => {}
        StartFault::ThenPatchesFail => state.rivals.patch_blocked = Some(id.to_owned()),
        StartFault::AfterRivalClaim => {
            instance["name"] = json!("crabbox-me--rival");
            instance["metadata"]["crabbox_lease_id"] = json!("cbx_rival");
            instance["metadata"]["crabbox_role"] = json!("lease");
        }
    }
    Some((
        500,
        nested_error("start_failed", "Instance failed to start."),
    ))
}

/// Progress of two acquires the fake makes race for one seed.
#[derive(Default)]
pub(super) struct Race {
    /// `GET /v1/instances` answered since the race began.
    lists: u32,
    /// `PATCH`es that arrived.
    patches: u32,
    /// `PATCH`es answered.
    answered: u32,
    /// `PATCH`es answered before the latest read of an instance.
    read_after: u32,
    /// Reads of an instance before any `PATCH` was answered.
    early_reads: u32,
}

/// Holds requests so two claims interleave: both acquires list the seed (two lists each) and
/// read it again, then the first `PATCH`es and reads it back, then the second does, and only then may
/// either start it. The first acquire waits at its start, so the read after the second
/// `PATCH` is the second acquire's.
pub(super) fn hold(state: &Mutex<State>, method: &str, path: &str) {
    let slot = {
        let mut s = state.lock().unwrap();
        let Some(race) = s.rivals.race.as_mut() else {
            return;
        };
        let slot = match method {
            "PATCH" => race.patches,
            "POST" if path.ends_with("/start") => u32::MAX,
            _ => return,
        };
        race.patches += u32::from(method == "PATCH");
        drop(s);
        slot
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let s = state.lock().unwrap();
        let ready = s.rivals.race.as_ref().is_some_and(|r| match slot {
            0 => r.lists >= 4 && r.early_reads >= 2,
            1 => r.read_after >= 1,
            u32::MAX => r.read_after >= 2,
            _ => true,
        });
        drop(s);
        if ready {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Records a routed request for the race, and lets a rival take the seed right after a list.
pub(super) fn observe(state: &mut State, method: &str, path: &str) {
    if let Some(race) = state.rivals.race.as_mut() {
        match (method, path) {
            ("GET", "/v1/instances") => race.lists += 1,
            ("PATCH", _) => race.answered += 1,
            ("GET", _) => {
                race.early_reads += u32::from(race.answered == 0);
                race.read_after = race.answered;
            }
            _ => {}
        }
    }
    if (method, path) == ("GET", "/v1/instances") {
        match state.rivals.taken_after_list.take() {
            Some((id, 1)) => rival_claim(state, &id),
            Some((id, n)) => state.rivals.taken_after_list = Some((id, n - 1)),
            None => {}
        }
    }
}

/// Clears instance `id`'s `/run`, as a stop clears its tmpfs.
pub(super) fn stop(state: &State, id: &str) {
    let _ = std::fs::remove_dir_all(state.boxes.path().join(id).join("run"));
}

impl FakeAgent37 {
    /// Makes the next two acquires race for one seed (see [`hold`]).
    #[allow(dead_code, reason = "each test binary uses a different subset")]
    pub(crate) fn start_race(&self) {
        self.lock().rivals.race = Some(Race::default());
    }

    /// Makes the next `start` fail as `fault` says.
    #[allow(dead_code, reason = "each test binary uses a different subset")]
    pub(crate) fn fail_next_start(&self, fault: StartFault) {
        self.lock().rivals.start_fault = Some(fault);
    }

    /// Makes a rival claim and start seed `id` right after `lists` more lists are answered.
    #[allow(dead_code, reason = "each test binary uses a different subset")]
    pub(crate) fn take_after_list(&self, id: &str, lists: u32) {
        self.lock().rivals.taken_after_list = Some((id.to_owned(), lists));
    }

    /// Makes the next `PATCH` of `id` land but answer a timeout; with `then_fail`, every later
    /// `PATCH` of `id` fails.
    #[allow(dead_code, reason = "each test binary uses a different subset")]
    pub(crate) fn lose_next_patch(&self, id: &str, then_fail: bool) {
        self.lock().rivals.patch_lost = Some((id.to_owned(), then_fail));
    }
}
