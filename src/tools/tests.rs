//! Unit tests for the tools and readiness module.

#![allow(
    clippy::indexing_slicing,
    clippy::unnecessary_wraps,
    reason = "tests index known-length recordings and share Result-returning helpers"
)]

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
    fn patch_instance(
        &self,
        _: &str,
        _: &str,
        _: &[(String, String)],
    ) -> Result<crate::agent37::Instance, ApiError> {
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

/// Runs `script` with `args` under bash on the host and returns its stdout.
fn host_bash(args: &[&str]) -> String {
    let out = std::process::Command::new("bash")
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn vendored_producer_lists_tmux_for_linux_minimal() {
    let dir =
        std::env::temp_dir().join(format!("crabbox-agent37-readiness-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("linux-readiness.generated.sh");
    std::fs::write(&script, READINESS).unwrap();
    let packages = host_bash(&[
        script.to_str().unwrap(),
        "--print-packages",
        "linux-minimal",
    ]);
    std::fs::remove_dir_all(&dir).unwrap();
    let packages: Vec<&str> = packages.split_whitespace().collect();
    for want in [
        "tmux",
        "jq",
        "curl",
        "git",
        "rsync",
        "openssh-server",
        "util-linux",
    ] {
        assert!(packages.contains(&want), "{packages:?} lacks {want}");
    }
}

#[test]
fn probe_without_a_readiness_manifest_reports_linux_minimal() {
    let probe = parse_probe(&host_bash(&["-c", &probe_command()]));
    assert!(
        probe.missing.iter().any(|m| m == LINUX_MINIMAL),
        "{probe:?}"
    );
}

#[test]
fn install_command_is_valid_shell() {
    let cmd = install_command(&[LINUX_MINIMAL.to_owned(), "rsync".to_owned()]);
    host_bash(&["-n", "-c", &cmd]);
    host_bash(&["-n", "-c", &probe_command()]);
}

#[test]
fn missing_readiness_writes_and_runs_the_producer() {
    let stub = Stub::new(vec![
        out("missing: linux-minimal\ninstall:none\n"),
        exit(0, "", ""),
        out("missing:\ninstall:ok\n"),
    ]);
    assert_eq!(run(&stub, 10), Ok(()));
    let seen = stub.seen.borrow();
    let install = &seen[1];
    assert!(install.contains(READINESS), "producer not written");
    assert!(install.contains(&format!(
        "$(bash {READINESS_PATH} --print-packages linux-minimal)"
    )));
    assert!(install.contains(&format!("&& bash {READINESS_PATH} && echo ok")));
    assert!(
        install.contains("' sh  </dev/null"),
        "linux-minimal passed as a package"
    );
}

/// sha256 of crabbox's `scripts/linux-readiness.generated.sh` as vendored.
const READINESS_SHA256: &str = "5f2df62714766bf506272842930d6acce07f79405fe02bf22bb93f5571478394";

#[test]
fn vendored_readiness_script_matches_crabbox() {
    use std::fmt::Write as _;
    let digest = ring::digest::digest(&ring::digest::SHA256, READINESS.as_bytes());
    let hex = digest.as_ref().iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    });
    assert_eq!(
        hex, READINESS_SHA256,
        "vendored script differs from crabbox"
    );
}
