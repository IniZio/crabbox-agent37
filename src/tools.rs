//! Ensures the tools crabbox's readiness check needs exist on an instance.
//!
//! The base image can lack some of them (rsync in particular). Without them
//! crabbox waits out its whole ready timeout with no explanation, so the
//! adapter installs what is missing and fails with a clear error otherwise.
//! The disk persists across stop and start, so the install happens once.

use std::io::Write;
use std::time::Instant;

use crate::agent37::{Agent37, ApiError};
use crate::lease::pause;

/// Tools crabbox requires; each is also the name of its apt package.
const TOOLS: [&str; 5] = ["bash", "python3", "git", "rsync", "tar"];

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

/// Command that reports missing tools and the install state. A `running`
/// status whose job is gone (the instance stopped mid-install) reads `stale`.
fn probe_command() -> String {
    format!(
        ": crabbox-tools-probe; m=; for t in {tools}; do command -v \"$t\" >/dev/null 2>&1 || m=\"$m $t\"; done; \
         echo \"missing:$m\"; s=$(cat {STATUS} 2>/dev/null); \
         if [ \"$s\" = running ]; then p=$(cat {PID} 2>/dev/null); \
         if [ -z \"$p\" ] || ! kill -0 \"$p\" 2>/dev/null; then s=stale; fi; fi; echo \"install:${{s:-none}}\"; \
         if [ \"$s\" = failed ]; then tail -n 5 {LOG} 2>/dev/null; fi; exit 0",
        tools = TOOLS.join(" "),
    )
}

/// Command that starts a detached install of `packages`, which must all be
/// required tools.
fn install_command(packages: &[String]) -> String {
    debug_assert!(packages.iter().all(|p| TOOLS.contains(&p.as_str())));
    format!(
        ": crabbox-tools-install; command -v apt-get >/dev/null 2>&1 || {{ echo 'no apt-get' >&2; exit 3; }}; \
         rm -f {PID}; echo running > {STATUS}; \
         setsid nohup sh -c 'DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=120 update -qq \
         && apt-get -o DPkg::Lock::Timeout=120 install -y -qq --no-install-recommends \"$@\" \
         && echo ok > {STATUS} || echo failed > {STATUS}' \
         sh {pkgs} </dev/null >{LOG} 2>&1 & echo $! > {PID}",
        pkgs = packages.join(" "),
    )
}

/// Parses probe stdout; names outside [`TOOLS`] are dropped.
fn parse_probe(stdout: &str) -> Probe {
    let mut lines = stdout.lines();
    let missing = lines
        .next()
        .and_then(|l| l.strip_prefix("missing:"))
        .unwrap_or("")
        .split_whitespace()
        .filter(|t| TOOLS.contains(t))
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
#[allow(
    clippy::indexing_slicing,
    clippy::unnecessary_wraps,
    reason = "tests index known-length recordings and share Result-returning helpers"
)]
mod tests {
    use super::*;
    use crate::agent37::{CreateSpec, ExecOutput, Instance, SshKey};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::time::Duration;

    /// Stub returning queued exec results and recording the commands.
    struct Stub {
        replies: RefCell<VecDeque<Result<ExecOutput, ApiError>>>,
        seen: RefCell<Vec<String>>,
    }

    impl Stub {
        /// Stub that answers exec calls with `replies` in order, then repeats the last.
        fn new(replies: Vec<Result<ExecOutput, ApiError>>) -> Self {
            Self {
                replies: RefCell::new(replies.into()),
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl Agent37 for Stub {
        fn list_instances(&self) -> Result<Vec<Instance>, ApiError> {
            Err(ApiError::Decode("unused".into()))
        }
        fn list_ssh_keys(&self) -> Result<Vec<SshKey>, ApiError> {
            Err(ApiError::Decode("unused".into()))
        }
        fn delete_instance(&self, _: &str) -> Result<(), ApiError> {
            Err(ApiError::Decode("unused".into()))
        }
        fn get_instance(&self, _: &str) -> Result<Instance, ApiError> {
            Err(ApiError::Decode("unused".into()))
        }
        fn create_instance(&self, _: &CreateSpec) -> Result<Instance, ApiError> {
            Err(ApiError::Decode("unused".into()))
        }
        fn exec_as_root(&self, _: &str, command: &str) -> Result<ExecOutput, ApiError> {
            self.seen.borrow_mut().push(command.to_string());
            let mut q = self.replies.borrow_mut();
            if q.len() > 1 {
                q.pop_front().unwrap()
            } else {
                q[0].clone()
            }
        }
        fn stop_instance(&self, _: &str) -> Result<(), ApiError> {
            Err(ApiError::Decode("unused".into()))
        }
        fn start_instance(&self, _: &str) -> Result<(), ApiError> {
            Err(ApiError::Decode("unused".into()))
        }
    }

    /// Successful exec with the given stdout.
    fn out(stdout: &str) -> Result<ExecOutput, ApiError> {
        exit(0, stdout, "")
    }

    /// Exec with the given exit code and output.
    fn exit(code: i64, stdout: &str, stderr: &str) -> Result<ExecOutput, ApiError> {
        Ok(ExecOutput {
            exit_code: code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        })
    }

    /// Runs `ensure` against `stub` with a deadline `secs` from now.
    fn run(stub: &Stub, secs: u64) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        ensure(stub, "i-1", deadline, &mut Vec::new())
    }

    #[test]
    fn fast_path_is_one_exec() {
        let stub = Stub::new(vec![out("missing:\ninstall:none\n")]);
        assert_eq!(run(&stub, 5), Ok(()));
        let seen = stub.seen.borrow();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].starts_with(": crabbox-tools-probe; "));
    }

    #[test]
    fn installs_then_succeeds() {
        let stub = Stub::new(vec![
            out("missing: rsync\ninstall:none\n"),
            exit(0, "", ""),
            out("missing: rsync\ninstall:running\n"),
            out("missing:\ninstall:ok\n"),
        ]);
        assert_eq!(run(&stub, 10), Ok(()));
        let seen = stub.seen.borrow();
        assert!(seen[1].starts_with(": crabbox-tools-install; "));
        assert!(seen[1].contains("sh rsync </dev/null >/var/tmp/crabbox-agent37-tools.log 2>&1 & echo $! > /var/tmp/crabbox-agent37-tools.pid"));
    }

    #[test]
    fn no_apt_get_names_the_tool() {
        let stub = Stub::new(vec![
            out("missing: rsync git\ninstall:none\n"),
            exit(3, "", "no apt-get"),
        ]);
        let err = run(&stub, 5).unwrap_err();
        assert!(
            err.contains("rsync git") && err.contains("no apt-get"),
            "{err}"
        );
    }

    #[test]
    fn failed_install_includes_log_tail() {
        let stub = Stub::new(vec![
            out("missing: rsync\ninstall:none\n"),
            exit(0, "", ""),
            out("missing: rsync\ninstall:failed\nE: unable to locate package\n"),
        ]);
        let err = run(&stub, 5).unwrap_err();
        assert!(err.contains("lacks required tools: rsync"), "{err}");
        assert!(
            err.contains("installing them failed: E: unable to locate package"),
            "{err}"
        );
    }

    #[test]
    fn deadline_error_when_install_never_finishes() {
        let stub = Stub::new(vec![out("missing: rsync\ninstall:running\n")]);
        let err = run(&stub, 0).unwrap_err();
        assert_eq!(
            err,
            "instance i-1 lacks required tools: rsync; installing them did not finish before readyTimeoutSeconds"
        );
        assert_eq!(stub.seen.borrow().len(), 2);
    }

    #[test]
    fn stale_running_starts_an_install() {
        let stub = Stub::new(vec![
            out("missing: rsync\ninstall:stale\n"),
            exit(0, "", ""),
            out("missing:\ninstall:ok\n"),
        ]);
        assert_eq!(run(&stub, 10), Ok(()));
        assert!(stub.seen.borrow()[1].starts_with(": crabbox-tools-install; "));
    }

    #[test]
    fn first_probe_error_is_retried() {
        let stub = Stub::new(vec![
            Err(ApiError::Decode("boom".into())),
            out("missing:\ninstall:none\n"),
        ]);
        assert_eq!(run(&stub, 10), Ok(()));
        assert_eq!(stub.seen.borrow().len(), 2);
    }

    #[test]
    fn first_probe_error_reported_at_deadline() {
        let stub = Stub::new(vec![Err(ApiError::Decode("boom".into()))]);
        let err = run(&stub, 0).unwrap_err();
        assert!(err.contains("last error:") && err.contains("boom"), "{err}");
    }
}
