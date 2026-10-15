//! `doctor`: checks that the workspace, VM entitlement and SSH key are usable.

use std::io::Write;

use serde_json::Value;

use crate::agent37::{Agent37, ApiError, Instance};

/// Note shown when no listed instance proves VM entitlement.
const VM_UNKNOWN: &str = "VM availability unknown until first acquire; a 403 tier_limit on acquire means the workspace is not funded for VM instances";

/// One check's outcome: `Ok` is a pass message, `Err` a failure message.
type Check = Result<String, String>;

/// Runs every check and reports all results on one line each, joined by `; `.
pub fn run(config: &Value, api: &dyn Agent37, diag: &mut dyn Write) -> Result<String, String> {
    let (workspace, instances) = check_workspace(api);
    let (vm, key) = instances.as_ref().map_or_else(
        || (skipped(), skipped()),
        |list| (Ok(check_vm(list)), check_ssh_key(config, api)),
    );
    let checks = [workspace, vm, key];
    let lines: Vec<&str> = checks
        .iter()
        .map(|c| match c {
            Ok(line) | Err(line) => line.as_str(),
        })
        .collect();
    for line in &lines {
        let _ = writeln!(diag, "doctor: {line}");
    }
    let joined = lines.join("; ");
    if checks.iter().all(Result::is_ok) {
        Ok(format!("agent37 ready: {joined}"))
    } else {
        Err(format!("agent37 doctor failed: {joined}"))
    }
}

/// Result for a check that could not run because the workspace is unreachable.
fn skipped() -> Check {
    Err("skipped".to_owned())
}

/// Lists instances; the instances are `Some` only when the workspace answered.
fn check_workspace(api: &dyn Agent37) -> (Check, Option<Vec<Instance>>) {
    match api.list_instances() {
        Ok(list) => (
            Ok(format!("workspace reachable ({} instances)", list.len())),
            Some(list),
        ),
        Err(ApiError::Http { status: 401, .. }) => (
            Err("workspace unreachable: API key rejected (invalid_api_key)".to_owned()),
            None,
        ),
        Err(err) => (
            Err(format!("workspace unreachable: {}", err.operator_message())),
            None,
        ),
    }
}

/// Reports VM entitlement from the listing alone; doctor never creates or deletes.
fn check_vm(instances: &[Instance]) -> String {
    if instances.iter().any(|i| i.kind.as_deref() == Some("vm")) {
        "VM instances available".to_owned()
    } else {
        VM_UNKNOWN.to_owned()
    }
}

/// Confirms the public key beside `config.sshKey` is registered with the workspace.
fn check_ssh_key(config: &Value, api: &dyn Agent37) -> Check {
    let private = config
        .get("sshKey")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| "config.sshKey is not set".to_owned())?;
    let path = format!("{private}.pub");
    let text =
        std::fs::read_to_string(&path).map_err(|err| format!("cannot read {path}: {err}"))?;
    let local = crate::fingerprint::sha256(&text)
        .ok_or_else(|| format!("{path} is not an OpenSSH public key"))?;
    let keys = api
        .list_ssh_keys()
        .map_err(|err| format!("SSH key check failed: {}", err.operator_message()))?;
    if keys.iter().any(|k| k.fingerprint == local) {
        Ok(format!("SSH key registered ({local})"))
    } else {
        Err(format!(
            "SSH public key {path} is not registered with the workspace; run: agent37 ssh setup, or POST it to /v1/ssh-keys"
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use std::cell::RefCell;

    use serde_json::json;

    use super::*;
    use crate::agent37::{CreateSpec, ExecOutput, SshKey};

    /// A real OpenSSH public key line.
    const PUBLIC_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIwdRpo7I2deKXSGxd0wh0gdyKXzsFUSfgoBHpLeiW7V mine";

    /// `ssh-keygen -lf` fingerprint of [`PUBLIC_KEY`].
    const FINGERPRINT: &str = "SHA256:/7KRdnBqeLut7cNaD0h6P5YCJtR/VYfNlcB+DL3N6hI";

    /// Scripted API; records deletes.
    struct Fake {
        /// Result of listing instances.
        instances: Result<Vec<Instance>, ApiError>,
        /// Registered keys.
        keys: Vec<SshKey>,
        /// Ids passed to delete.
        deleted: RefCell<Vec<String>>,
    }

    impl Agent37 for Fake {
        fn list_instances(&self) -> Result<Vec<Instance>, ApiError> {
            self.instances.clone()
        }
        fn list_ssh_keys(&self) -> Result<Vec<SshKey>, ApiError> {
            Ok(self.keys.clone())
        }
        fn delete_instance(&self, id: &str) -> Result<(), ApiError> {
            self.deleted.borrow_mut().push(id.to_owned());
            Ok(())
        }
        fn get_instance(&self, _id: &str) -> Result<Instance, ApiError> {
            Err(ApiError::Unreachable("not used by doctor".into()))
        }
        fn create_instance(&self, _spec: &CreateSpec) -> Result<Instance, ApiError> {
            Err(ApiError::Unreachable("not used by doctor".into()))
        }
        fn stop_instance(&self, _id: &str) -> Result<(), ApiError> {
            Ok(())
        }
        fn start_instance(&self, _id: &str) -> Result<(), ApiError> {
            Ok(())
        }
        fn exec_as_root(&self, _id: &str, _command: &str) -> Result<ExecOutput, ApiError> {
            Err(ApiError::Unreachable("not used by doctor".into()))
        }
    }

    /// Instance with the given id and kind.
    fn inst(id: &str, kind: &str) -> Instance {
        Instance {
            id: id.into(),
            name: None,
            kind: Some(kind.into()),
            status: None,
            metadata: Vec::new(),
        }
    }

    /// HTTP error with an optional agent37 code.
    fn http(status: u16, code: Option<&str>) -> ApiError {
        ApiError::Http {
            status,
            code: code.map(Into::into),
            message: "m".into(),
        }
    }

    /// A workspace where every check passes.
    fn fake() -> Fake {
        Fake {
            instances: Ok(vec![inst("a", "vm")]),
            keys: vec![SshKey {
                id: "k".into(),
                fingerprint: FINGERPRINT.into(),
            }],
            deleted: RefCell::new(vec![]),
        }
    }

    /// Writes a `.pub` file and runs doctor against it.
    fn go(api: &Fake) -> Result<String, String> {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("id");
        std::fs::write(dir.path().join("id.pub"), format!("{PUBLIC_KEY}\n")).unwrap();
        run(&json!({"sshKey": key}), api, &mut Vec::new())
    }

    #[test]
    fn all_pass_with_existing_vm() {
        let api = fake();
        let msg = go(&api).unwrap();
        assert_eq!(
            msg,
            "agent37 ready: workspace reachable (1 instances); VM instances available; SSH key registered (SHA256:/7KRdnBqeLut7cNaD0h6P5YCJtR/VYfNlcB+DL3N6hI)"
        );
    }

    #[test]
    fn no_vm_instance_gives_the_note_and_ok_when_other_checks_pass() {
        let mut api = fake();
        api.instances = Ok(vec![inst("a", "default")]);
        let msg = go(&api).unwrap();
        assert!(msg.starts_with("agent37 ready: "));
        assert!(msg.contains(VM_UNKNOWN));
    }

    #[test]
    fn the_fake_records_no_create_or_delete_call() {
        let mut api = fake();
        api.instances = Ok(vec![]);
        assert!(go(&api).is_ok());
        assert!(api.deleted.borrow().is_empty());
    }

    #[test]
    fn rejected_key_skips_other_checks() {
        let mut api = fake();
        api.instances = Err(http(401, Some("invalid_api_key")));
        let msg = go(&api).unwrap_err();
        assert!(msg.contains(
            "workspace unreachable: API key rejected (invalid_api_key); skipped; skipped"
        ));
    }

    #[test]
    fn unregistered_key_fails() {
        let mut api = fake();
        api.keys.clear();
        let msg = go(&api).unwrap_err();
        assert!(msg.contains("is not registered with the workspace; run: agent37 ssh setup"));
    }

    #[test]
    fn non_openssh_public_key_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("id");
        std::fs::write(dir.path().join("id.pub"), "garbage\n").unwrap();
        let msg = run(&json!({"sshKey": key}), &fake(), &mut Vec::new()).unwrap_err();
        assert!(msg.contains("is not an OpenSSH public key"));
    }

    #[test]
    fn missing_ssh_key_config_fails() {
        let msg = run(&json!({}), &fake(), &mut Vec::new()).unwrap_err();
        assert!(msg.contains("config.sshKey is not set"));
    }
}
