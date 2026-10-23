//! Exec answers for the scripts the adapter runs on a VM.

use std::collections::HashSet;

use serde_json::{json, Value};

use super::{FakeAgent37, InstallMode, State};

/// How dockerd on a fake VM reacts to the data-root script.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
#[allow(dead_code, reason = "each test binary uses a different subset")]
pub(crate) enum Dockerd {
    /// Already on `/root/docker`, or not running.
    #[default]
    OnBilledDisk,
    /// Runs on another root until a boot after `daemon.json` names `/root/docker`.
    ElsewhereUntilBoot,
    /// Stays on another root across boots.
    AlwaysElsewhere,
    /// The script fails, as with an unparsable `daemon.json`.
    ScriptFails,
}

/// Fake state of Docker's data root per instance.
#[derive(Default)]
pub(super) struct DockerRoot {
    /// How dockerd behaves.
    mode: Dockerd,
    /// Instances whose `daemon.json` names `/root/docker`.
    written: HashSet<String>,
    /// Instances booted after `daemon.json` was written.
    booted: HashSet<String>,
}

impl DockerRoot {
    /// Answers the data-root script for instance `id`.
    pub(super) fn run(&mut self, id: &str) -> Value {
        if self.mode == Dockerd::ScriptFails {
            return json!({ "exit_code": 1, "stdout": "", "stderr": "json.decoder.JSONDecodeError: Expecting value\n" });
        }
        self.written.insert(id.to_owned());
        let moved = match self.mode {
            Dockerd::ElsewhereUntilBoot => self.booted.contains(id),
            Dockerd::AlwaysElsewhere => false,
            _ => true,
        };
        let verdict = if moved { "ok" } else { "reboot" };
        json!({ "exit_code": 0, "stdout": format!("{verdict}\n"), "stderr": "" })
    }

    /// Records a boot of instance `id`.
    pub(super) fn boot(&mut self, id: &str) {
        if self.written.contains(id) {
            self.booted.insert(id.to_owned());
        }
    }
}

impl FakeAgent37 {
    /// Sets how dockerd reacts to the data-root script.
    #[allow(dead_code, reason = "each test binary uses a different subset")]
    pub(crate) fn set_dockerd(&self, mode: Dockerd) {
        self.lock().docker_root.mode = mode;
    }

    /// Whether `daemon.json` on instance `id` names `/root/docker`.
    #[allow(dead_code, reason = "each test binary uses a different subset")]
    pub(crate) fn docker_root_set(&self, id: &str) -> bool {
        self.lock().docker_root.written.contains(id)
    }
}

/// Answers the tool probe, finishing a running install once its probes are spent.
pub(super) fn tool_probe(state: &mut State) -> Value {
    if state.tool_install == "running" {
        state.install_probes_left = state.install_probes_left.saturating_sub(1);
        if state.install_probes_left == 0 {
            if state.install_mode == InstallMode::Fails {
                "failed".clone_into(&mut state.tool_install);
            } else {
                state.missing_tools.clear();
                "ok".clone_into(&mut state.tool_install);
            }
        }
    }
    let mut stdout = format!(
        "missing: {}\ninstall:{}\n",
        state.missing_tools.join(" "),
        state.tool_install
    );
    if state.tool_install == "failed" {
        stdout.push_str("E: Unable to locate package rsync\n");
    }
    json!({ "exit_code": 0, "stdout": stdout, "stderr": "" })
}

/// Answers the detached tool install start.
pub(super) fn tool_install(state: &mut State) -> Value {
    if state.install_mode == InstallMode::NoApt {
        return json!({ "exit_code": 3, "stdout": "", "stderr": "no apt-get\n" });
    }
    "running".clone_into(&mut state.tool_install);
    state.install_probes_left = state.install_probes;
    json!({ "exit_code": 0, "stdout": "", "stderr": "" })
}
