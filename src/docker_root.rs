//! Keeps Docker's data on the VM's billed disk.
//!
//! On an agent37 VM `/` is a fixed 10 GB OS disk and the billed disk is `/root`, so a new VM
//! gets `"data-root": "/root/docker"` in `/etc/docker/daemon.json`, other keys kept. A VM has
//! no init system: its dockerd is started at boot by agent37's guest init, and killing it
//! reboots the VM. So a dockerd that already runs on another root is moved by a reboot.

use std::io::Write;
use std::time::Instant;

use crate::agent37::{Agent37, ExecOutput};
use crate::lease::{pause, status_of};

/// Merges the data root into `daemon.json`, then prints `ok`, or `reboot` when a running
/// dockerd still uses another root.
const SCRIPT: &str = r#": crabbox-docker-root; set -e; f=/etc/docker/daemon.json; mkdir -p /etc/docker /root/docker; python3 -c 'import json,os,sys; p=sys.argv[1]; d=json.load(open(p)) if os.path.exists(p) else {}; d.get("data-root") == "/root/docker" or open(p, "w").write(json.dumps({**d, "data-root": "/root/docker"}, indent=2) + "\n")' "$f"; if ! pidof dockerd >/dev/null; then echo ok; exit 0; fi; i=0; until r=$(docker info --format '{{.DockerRootDir}}' 2>/dev/null); do i=$((i+1)); if [ "$i" -ge 30 ]; then echo "dockerd runs but docker info fails" >&2; exit 5; fi; sleep 1; done; if [ "$r" = /root/docker ]; then echo ok; else echo reboot; fi"#;

/// Puts Docker's data root on the billed disk of `id` when this acquire `created` it.
///
/// Runs before the host key is read, because a reboot can move the VM to another host.
pub(crate) fn ensure(
    api: &dyn Agent37,
    id: &str,
    created: bool,
    deadline: Instant,
    diag: &mut dyn Write,
) -> Result<(), String> {
    if !created {
        return Ok(());
    }
    let what = format!("putting Docker data on /root/docker on {id}");
    if verdict(&what, api.exec_as_root(id, SCRIPT))? == "ok" {
        let _ = writeln!(diag, "acquire: Docker data root is /root/docker");
        return Ok(());
    }
    let _ = writeln!(
        diag,
        "acquire: rebooting {id} to move dockerd to /root/docker"
    );
    reboot(api, id, deadline).map_err(|e| format!("{what}: {e}"))?;
    let after = loop {
        match api.exec_as_root(id, SCRIPT) {
            Err(_) if pause(deadline) => {}
            out => break verdict(&what, out)?,
        }
    };
    if after != "ok" {
        return Err(format!(
            "{what}: dockerd is still not on /root/docker after a reboot"
        ));
    }
    let _ = writeln!(diag, "acquire: Docker data root is /root/docker");
    Ok(())
}

/// The script's verdict, or why it failed.
fn verdict(
    what: &str,
    out: Result<ExecOutput, crate::agent37::ApiError>,
) -> Result<String, String> {
    let out = out.map_err(|e| format!("{what}: {}", e.operator_message()))?;
    if out.exit_code != 0 {
        return Err(format!(
            "{what} exited {}: {}",
            out.exit_code,
            out.stderr.trim()
        ));
    }
    Ok(out.stdout.trim().to_owned())
}

/// Stops then starts `id`, waiting for each state until `deadline`.
fn reboot(api: &dyn Agent37, id: &str, deadline: Instant) -> Result<(), String> {
    api.stop_instance(id).map_err(|e| e.operator_message())?;
    wait_for(api, id, &["stopped", "sleeping"], deadline)?;
    api.start_instance(id).map_err(|e| e.operator_message())?;
    wait_for(api, id, &["running"], deadline)
}

/// Polls `id` until its status is one of `want`.
fn wait_for(api: &dyn Agent37, id: &str, want: &[&str], deadline: Instant) -> Result<(), String> {
    loop {
        let instance = api.get_instance(id).map_err(|e| e.operator_message())?;
        let status = status_of(&instance);
        if want.contains(&status) {
            return Ok(());
        }
        if status == "failed" || !pause(deadline) {
            return Err(format!(
                "instance {id} is {status}, not {}",
                want.join(" or ")
            ));
        }
    }
}
