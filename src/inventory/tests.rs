use super::*;
use crate::agent37::{CreateSpec, ExecOutput, SshKey};
use crate::protocol::{Expected, LeaseRef, Operation};
use std::cell::RefCell;

struct Fake {
    instances: Vec<Instance>,
    deleted: RefCell<Vec<String>>,
}

fn not_found() -> ApiError {
    ApiError::Http {
        status: 404,
        code: None,
        message: "gone".into(),
    }
}

impl Agent37 for Fake {
    fn list_instances(&self) -> Result<Vec<Instance>, ApiError> {
        Ok(self.instances.clone())
    }
    fn list_ssh_keys(&self) -> Result<Vec<SshKey>, ApiError> {
        Err(ApiError::Decode("unused".into()))
    }
    fn delete_instance(&self, id: &str) -> Result<(), ApiError> {
        self.deleted.borrow_mut().push(id.to_owned());
        Ok(())
    }
    fn get_instance(&self, id: &str) -> Result<Instance, ApiError> {
        self.instances
            .iter()
            .find(|i| i.id == id)
            .cloned()
            .ok_or_else(not_found)
    }
    fn create_instance(&self, _: &CreateSpec) -> Result<Instance, ApiError> {
        Err(ApiError::Decode("unused".into()))
    }
    fn stop_instance(&self, _: &str) -> Result<(), ApiError> {
        Ok(())
    }
    fn start_instance(&self, _: &str) -> Result<(), ApiError> {
        Ok(())
    }
    fn patch_instance(
        &self,
        _: &str,
        _: &str,
        _: &[(String, String)],
    ) -> Result<crate::agent37::Instance, ApiError> {
        Err(ApiError::Decode("unused".into()))
    }
    fn exec_as_root(&self, _: &str, _: &str) -> Result<ExecOutput, ApiError> {
        Err(ApiError::Decode("unused".into()))
    }
}

fn inst(id: &str, name: &str, owner: &str, lease: Option<&str>, slug: Option<&str>) -> Instance {
    let mut metadata = vec![("crabbox_owner".to_owned(), owner.to_owned())];
    metadata.extend(lease.map(|l| ("crabbox_lease_id".to_owned(), l.to_owned())));
    metadata.extend(slug.map(|s| ("crabbox_slug".to_owned(), s.to_owned())));
    Instance {
        id: id.into(),
        name: Some(name.into()),
        kind: Some("vm".into()),
        status: Some("running".into()),
        metadata,
    }
}

fn fake(instances: Vec<Instance>) -> Fake {
    Fake {
        instances,
        deleted: RefCell::default(),
    }
}

fn request(operation: Operation) -> Request {
    Request {
        operation,
        config: serde_json::json!({"owner": "me"}),
        desired: None,
        expected: None,
        keep: false,
        lease: None,
        dry_run: false,
        release_only: false,
    }
}

fn release_req(cloud_id: Option<&str>, name: Option<&str>, lease_id: Option<&str>) -> Request {
    let mut r = request(Operation::Release);
    r.lease = Some(LeaseRef {
        lease_id: lease_id.map(Into::into),
        slug: None,
        name: name.map(Into::into),
        cloud_id: cloud_id.map(Into::into),
    });
    r
}

fn mine(id: &str) -> Instance {
    inst(id, "crabbox-me--box", "me", Some("l1"), Some("s1"))
}

#[test]
fn owned_needs_prefix_and_metadata() {
    assert!(owned(&mine("a"), "me"));
    assert!(!owned(&inst("a", "crabbox-me--x", "you", None, None), "me"));
    assert!(!owned(&inst("a", "crabbox-you--x", "me", None, None), "me"));
}

#[test]
fn list_filters_maps_and_sorts() {
    let mut stopped = inst("c", "crabbox-me--aa", "me", Some("l2"), Some("s2"));
    stopped.status = Some("sleeping".into());
    let mut no_kind = mine("d");
    no_kind.kind = Some("default".into());
    let api = fake(vec![
        mine("b"),
        stopped,
        no_kind,
        inst("e", "crabbox-you--zz", "you", Some("l"), Some("s")),
        inst("f", "crabbox-me--orphan", "me", None, None),
        inst("g", "crabbox-me--", "me", Some("l"), Some("s")),
    ]);
    let rows = list(&request(Operation::List), &api, &mut std::io::sink()).unwrap();
    let got: Vec<_> = rows.iter().map(|r| (&*r.cloud_id, &*r.status)).collect();
    assert_eq!(got, [("c", "stopped"), ("b", "ready")]);
    assert_eq!(rows.first().map(|r| r.name.as_str()), Some("aa"));
}

#[test]
fn release_deletes_exact_id() {
    let api = fake(vec![mine("a"), mine("b")]);
    release(&release_req(Some("a"), Some("box"), Some("l1")), &api).unwrap();
    assert_eq!(*api.deleted.borrow(), ["a"]);
}

#[test]
fn release_missing_instance_is_success() {
    let api = fake(vec![]);
    release(&release_req(Some("a"), None, None), &api).unwrap();
    assert!(api.deleted.borrow().is_empty());
}

#[test]
fn release_requires_cloud_id() {
    let err = release(&release_req(None, None, None), &fake(vec![])).unwrap_err();
    assert_eq!(err, "release requires lease.cloudId");
}

#[test]
fn release_refuses_mismatches() {
    let other = inst("x", "crabbox-you--box", "you", Some("l1"), Some("s1"));
    let api = fake(vec![mine("a"), other]);
    let cases = [
        release_req(Some("x"), None, None),
        release_req(Some("a"), Some("other"), None),
        release_req(Some("a"), None, Some("nope")),
    ];
    for req in &cases {
        assert!(release(req, &api).is_err());
    }
    let mut expected_id = release_req(Some("a"), None, None);
    expected_id.expected = Some(Expected {
        lease_id: None,
        attempt_lease_id: None,
        slug: None,
        cloud_id: Some("z".into()),
    });
    assert!(release(&expected_id, &api).is_err());
    assert!(api.deleted.borrow().is_empty());
}

#[test]
fn cleanup_removes_only_owned_unlistable() {
    let api = fake(vec![
        mine("keep"),
        inst("o1", "crabbox-me--half", "me", Some("l"), None),
        inst("o2", "crabbox-me--", "me", Some("l"), Some("s")),
        inst("theirs", "crabbox-you--half", "you", None, None),
    ]);
    let mut dry = request(Operation::Cleanup);
    dry.dry_run = true;
    assert_eq!(
        cleanup(&dry, &api).unwrap(),
        "would delete 2 instance(s): o1, o2"
    );
    assert!(api.deleted.borrow().is_empty());
    assert_eq!(
        cleanup(&request(Operation::Cleanup), &api).unwrap(),
        "deleted 2 instance(s): o1, o2"
    );
    assert_eq!(*api.deleted.borrow(), ["o1", "o2"]);
}

#[test]
fn cleanup_with_nothing_to_do() {
    let api = fake(vec![mine("a")]);
    let msg = cleanup(&request(Operation::Cleanup), &api).unwrap();
    assert_eq!(msg, "nothing to clean up");
}
