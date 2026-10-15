//! `doctor`: checks that the workspace, VM entitlement and SSH key are usable.

use std::io::Write;

use serde_json::Value;

use crate::agent37::{Agent37, ApiError, Instance};

/// Name of the throwaway VM created when no listed instance proves VM entitlement.
const PROBE_NAME: &str = "crabbox-agent37-doctor-probe";

/// One check's outcome: `Ok` is a pass message, `Err` a failure message.
type Check = Result<String, String>;

/// Runs every check and reports all results on one line each, joined by `; `.
pub fn run(config: &Value, api: &dyn Agent37, diag: &mut dyn Write) -> Result<String, String> {
    let (workspace, instances) = check_workspace(api);
    let (vm, key) = instances.as_ref().map_or_else(
        || (skipped(), skipped()),
        |list| (check_vm(api, list), check_ssh_key(config, api)),
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
        Err(err) => (Err(format!("workspace unreachable: {err}")), None),
    }
}

/// Confirms the workspace can run VMs, probing with a create only when listing cannot tell.
fn check_vm(api: &dyn Agent37, instances: &[Instance]) -> Check {
    let available = "VM instances available".to_owned();
    if instances.iter().any(|i| i.kind.as_deref() == Some("vm")) {
        return Ok(available);
    }
    match api.create_probe_vm(PROBE_NAME) {
        Ok(probe) => api
            .delete_instance(&probe.id)
            .map(|()| available)
            .map_err(|err| {
                format!(
                    "probe VM {} was created but could not be deleted ({err}); remove it manually",
                    probe.id
                )
            }),
        Err(ApiError::Http {
            status: 403,
            code: Some(code),
            ..
        }) if code == "tier_limit" => Err("workspace not funded for VM instances".to_owned()),
        Err(err) => Err(format!("VM availability unknown: {err}")),
    }
}

/// Key type and base64 blob of an OpenSSH public key line, ignoring the comment.
fn key_identity(line: &str) -> Option<(&str, &str)> {
    let mut fields = line.split_whitespace();
    Some((fields.next()?, fields.next()?))
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
    let (kind, blob) =
        key_identity(&text).ok_or_else(|| format!("{path} is not an OpenSSH public key"))?;
    let keys = api
        .list_ssh_keys()
        .map_err(|err| format!("SSH key check failed: {err}"))?;
    if keys
        .iter()
        .any(|k| key_identity(&k.public_key) == Some((kind, blob)))
    {
        let skip = blob.chars().count().saturating_sub(8);
        let tail: String = blob.chars().skip(skip).collect();
        Ok(format!("SSH key registered ({kind} ...{tail})"))
    } else {
        Err(format!(
            "SSH public key {path} is not registered with the workspace; run: agent37 ssh setup, or POST it to /v1/ssh-keys"
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use std::cell::{Cell, RefCell};

    use serde_json::json;

    use super::*;
    use crate::agent37::SshKey;

    /// Scripted API; counts creates and records deletes.
    struct Fake {
        /// Result of listing instances.
        instances: Result<Vec<Instance>, ApiError>,
        /// Registered keys.
        keys: Vec<SshKey>,
        /// Result of the probe create.
        create: Result<Instance, ApiError>,
        /// Number of probe creates.
        creates: Cell<u32>,
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
        fn create_probe_vm(&self, _name: &str) -> Result<Instance, ApiError> {
            self.creates.set(self.creates.get() + 1);
            self.create.clone()
        }
        fn delete_instance(&self, id: &str) -> Result<(), ApiError> {
            self.deleted.borrow_mut().push(id.to_owned());
            Ok(())
        }
    }

    /// Instance with the given id and kind.
    fn inst(id: &str, kind: &str) -> Instance {
        Instance {
            id: id.into(),
            name: None,
            kind: Some(kind.into()),
            status: None,
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
                public_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIABCDEFGH other-comment".into(),
            }],
            create: Ok(inst("probe", "vm")),
            creates: Cell::new(0),
            deleted: RefCell::new(vec![]),
        }
    }

    /// Writes a `.pub` file and runs doctor against it.
    fn go(api: &Fake) -> Result<String, String> {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("id");
        std::fs::write(
            dir.path().join("id.pub"),
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIABCDEFGH mine\n",
        )
        .unwrap();
        run(&json!({"sshKey": key}), api, &mut Vec::new())
    }

    #[test]
    fn all_pass_and_existing_vm_skips_probe() {
        let api = fake();
        let msg = go(&api).unwrap();
        assert_eq!(
            msg,
            "agent37 ready: workspace reachable (1 instances); VM instances available; SSH key registered (ssh-ed25519 ...ABCDEFGH)"
        );
        assert_eq!(api.creates.get(), 0);
    }

    #[test]
    fn tier_limit_means_not_funded() {
        let mut api = fake();
        api.instances = Ok(vec![inst("a", "default")]);
        api.create = Err(http(403, Some("tier_limit")));
        let msg = go(&api).unwrap_err();
        assert!(msg.starts_with("agent37 doctor failed: "));
        assert!(msg.contains("workspace not funded for VM instances"));
    }

    #[test]
    fn probe_success_is_deleted() {
        let mut api = fake();
        api.instances = Ok(vec![]);
        assert!(go(&api).is_ok());
        assert_eq!(api.creates.get(), 1);
        assert_eq!(*api.deleted.borrow(), ["probe"]);
    }

    #[test]
    fn rejected_key_skips_other_checks() {
        let mut api = fake();
        api.instances = Err(http(401, Some("invalid_api_key")));
        let msg = go(&api).unwrap_err();
        assert!(msg.contains(
            "workspace unreachable: API key rejected (invalid_api_key); skipped; skipped"
        ));
        assert_eq!(api.creates.get(), 0);
    }

    #[test]
    fn unregistered_key_fails() {
        let mut api = fake();
        api.keys.clear();
        let msg = go(&api).unwrap_err();
        assert!(msg.contains("is not registered with the workspace; run: agent37 ssh setup"));
    }

    #[test]
    fn missing_ssh_key_config_fails() {
        let msg = run(&json!({}), &fake(), &mut Vec::new()).unwrap_err();
        assert!(msg.contains("config.sshKey is not set"));
    }
}
