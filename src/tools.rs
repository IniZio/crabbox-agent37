//! Ensures an instance meets crabbox's linux-minimal readiness and has the
//! tools crabbox's external ready check needs.
//!
//! The base image lacks some of them (rsync, tmux). Without them crabbox waits
//! out its whole ready timeout with no explanation, or a Box command fails
//! later, so the adapter installs what is missing and fails with a clear error
//! otherwise. The disk persists across stop and start, and crabbox's readiness
//! manifest records the result, so the install happens once.

use std::io::Write;
use std::time::Instant;

use crate::agent37::{Agent37, ApiError};
use crate::lease::pause;

/// Tools crabbox's external ready check requires; each is also the name of its apt package.
const TOOLS: [&str; 5] = ["bash", "python3", "git", "rsync", "tar"];

/// Probe entry for a Box that lacks a verified crabbox linux-minimal readiness manifest.
const LINUX_MINIMAL: &str = "linux-minimal";

/// Vendored unchanged from crabbox `scripts/linux-readiness.generated.sh`;
/// refresh by copying that file over and updating `READINESS_SHA256` in the
/// tests. It lists the linux-minimal packages, writes the readiness manifest
/// and verifies it.
const READINESS: &str = include_str!("tools/linux-readiness.generated.sh");

/// Where the producer lives on the Box; crabbox's own `--install` puts it here.
const READINESS_PATH: &str = "/usr/local/libexec/crabbox/linux-readiness.generated.sh";

/// File holding `running`, `ok` or `failed` for the background install.
const STATUS: &str = "/var/tmp/crabbox-agent37-tools.status";

/// File holding the PID of the background install job.
const PID: &str = "/var/tmp/crabbox-agent37-tools.pid";

/// Output of the background install.
const LOG: &str = "/var/tmp/crabbox-agent37-tools.log";

/// What one probe reported.
#[derive(Debug, PartialEq, Eq)]
struct Probe {
    /// Required tools not found on PATH.
    missing: Vec<String>,
    /// Install state: `none`, `running`, `stale`, `ok` or `failed`.
    install: String,
    /// Tail of the install log, only present when the install failed.
    log_tail: String,
}

/// Command that reports missing tools, a missing readiness manifest, and the
/// install state. A builder manifest also proves linux-minimal. A `running`
/// status whose job is gone (the instance stopped mid-install) reads `stale`.
fn probe_command() -> String {
    format!(
        ": crabbox-tools-probe; m=; for t in {tools}; do command -v \"$t\" >/dev/null 2>&1 || m=\"$m $t\"; done; \
         {{ bash {READINESS_PATH} --verify linux-minimal || bash {READINESS_PATH} --verify linux-builder; }} \
         >/dev/null 2>&1 || m=\"$m {LINUX_MINIMAL}\"; \
         echo \"missing:$m\"; s=$(cat {STATUS} 2>/dev/null); \
         if [ \"$s\" = running ]; then p=$(cat {PID} 2>/dev/null); \
         if [ -z \"$p\" ] || ! kill -0 \"$p\" 2>/dev/null; then s=stale; fi; fi; echo \"install:${{s:-none}}\"; \
         if [ \"$s\" = failed ]; then tail -n 5 {LOG} 2>/dev/null; fi; exit 0",
        tools = TOOLS.join(" "),
    )
}

/// Command that writes crabbox's readiness producer to the Box and starts a
/// detached install of the linux-minimal packages plus the `missing` tools,
/// then runs the producer to write and verify the readiness manifest.
fn install_command(missing: &[String]) -> String {
    debug_assert!(missing
        .iter()
        .all(|p| TOOLS.contains(&p.as_str()) || p == LINUX_MINIMAL));
    let tools: Vec<&str> = missing
        .iter()
        .map(String::as_str)
        .filter(|p| *p != LINUX_MINIMAL)
        .collect();
    format!(
        ": crabbox-tools-install; command -v apt-get >/dev/null 2>&1 || {{ echo 'no apt-get' >&2; exit 3; }}; \
         mkdir -p {dir} && cat > {READINESS_PATH}.new <<'CRABBOX_AGENT37_READINESS' || exit 4\n\
         {READINESS}CRABBOX_AGENT37_READINESS\n\
         chmod 0755 {READINESS_PATH}.new && mv -f {READINESS_PATH}.new {READINESS_PATH} || exit 4; \
         rm -f {PID}; echo running > {STATUS}; \
         setsid nohup sh -c 'DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=120 update -qq \
         && apt-get -o DPkg::Lock::Timeout=120 install -y -qq --no-install-recommends \
         $(bash {READINESS_PATH} --print-packages linux-minimal) \"$@\" \
         && bash {READINESS_PATH} && echo ok > {STATUS} || echo failed > {STATUS}' \
         sh {tools} </dev/null >{LOG} 2>&1 & echo $! > {PID}",
        dir = READINESS_PATH.rsplit_once('/').map_or("/", |(d, _)| d),
        tools = tools.join(" "),
    )
}

/// Parses probe stdout; names outside [`TOOLS`] and [`LINUX_MINIMAL`] are dropped.
fn parse_probe(stdout: &str) -> Probe {
    let mut lines = stdout.lines();
    let missing = lines
        .next()
        .and_then(|l| l.strip_prefix("missing:"))
        .unwrap_or("")
        .split_whitespace()
        .filter(|t| TOOLS.contains(t) || *t == LINUX_MINIMAL)
        .map(String::from)
        .collect();
    let install = lines
        .next()
        .and_then(|l| l.strip_prefix("install:"))
        .unwrap_or("none")
        .trim()
        .to_string();
    let log_tail = lines.collect::<Vec<_>>().join("\n");
    Probe {
        missing,
        install,
        log_tail,
    }
}

/// Runs one probe on the instance.
fn probe(api: &dyn Agent37, id: &str) -> Result<Probe, ApiError> {
    let out = api.exec_as_root(id, &probe_command())?;
    Ok(parse_probe(&out.stdout))
}

/// Probes until one succeeds, retrying API errors until `deadline`.
fn first_probe(api: &dyn Agent37, id: &str, deadline: Instant) -> Result<Probe, String> {
    loop {
        match probe(api, id) {
            Ok(p) => return Ok(p),
            Err(e) if !pause(deadline) => {
                return Err(format!(
                    "checking required tools on {id} did not succeed before readyTimeoutSeconds (last error: {})",
                    e.operator_message()
                ));
            }
            Err(_) => {}
        }
    }
}

/// Makes sure every required tool exists on instance `id`, installing missing
/// ones in the background and polling until `deadline`.
pub(crate) fn ensure(
    api: &dyn Agent37,
    id: &str,
    deadline: Instant,
    diag: &mut dyn Write,
) -> Result<(), String> {
    let first = first_probe(api, id, deadline)?;
    if first.missing.is_empty() {
        return Ok(());
    }
    let names = first.missing.join(" ");
    let lacks = format!("instance {id} lacks required tools: {names}");
    let _ = writeln!(diag, "tools: installing {names}");
    // `none`, `ok`, `failed` and `stale` all start a fresh install.
    if first.install != "running" {
        let out = api
            .exec_as_root(id, &install_command(&first.missing))
            .map_err(|e| {
                format!(
                    "{lacks}; starting the install failed: {}",
                    e.operator_message()
                )
            })?;
        if out.exit_code == 3 {
            return Err(format!("{lacks}; no apt-get to install them"));
        }
        if out.exit_code != 0 {
            return Err(format!(
                "{lacks}; starting the install exited {}: {}",
                out.exit_code,
                out.stderr.trim()
            ));
        }
    }
    loop {
        let last_error = match probe(api, id) {
            Ok(p) if p.missing.is_empty() => return Ok(()),
            Ok(p) if p.install == "failed" => {
                return Err(format!(
                    "instance {id} lacks required tools: {}; installing them failed: {}",
                    p.missing.join(" "),
                    p.log_tail
                ));
            }
            Ok(_) => None,
            Err(e) => Some(e.operator_message()),
        };
        if !pause(deadline) {
            let mut msg =
                format!("{lacks}; installing them did not finish before readyTimeoutSeconds");
            if let Some(e) = last_error {
                msg = format!("{msg} (last error: {e})");
            }
            return Err(msg);
        }
    }
}

#[cfg(test)]
mod tests;
