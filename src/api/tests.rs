use serde_json::json;

use super::*;

#[test]
fn base_url_trims_and_defaults() {
    assert_eq!(base_url(&json!({"apiUrl": "http://x:1//"})), "http://x:1");
    assert_eq!(base_url(&json!({})), DEFAULT_BASE_URL);
    assert_eq!(base_url(&json!({"apiUrl": 3})), DEFAULT_BASE_URL);
}

#[test]
fn envelope_error() {
    let body = r#"{"error":{"code":"tier_limit","message":"no vms"}}"#;
    assert_eq!(
        http_error(403, body),
        ApiError::Http {
            status: 403,
            code: Some("tier_limit".into()),
            message: "no vms".into()
        }
    );
}

#[test]
fn flat_error() {
    let body = r#"{"error":"invalid_api_key","message":"bad key"}"#;
    assert_eq!(
        http_error(401, body),
        ApiError::Http {
            status: 401,
            code: Some("invalid_api_key".into()),
            message: "bad key".into()
        }
    );
}

#[test]
fn unparsable_error_uses_status() {
    assert_eq!(
        http_error(502, "<html>"),
        ApiError::Http {
            status: 502,
            code: None,
            message: "HTTP 502".into()
        }
    );
}

/// Spec used by the create body tests.
fn spec() -> CreateSpec {
    CreateSpec {
        name: "crabbox-me--box".into(),
        region: "us".into(),
        cpu: 2,
        memory_gb: 4,
        template: None,
        disk_gb: None,
        metadata: vec![("crabbox_owner".into(), "me".into())],
    }
}

#[test]
fn create_body_carries_template_and_disk_when_set() {
    let body = create_body(&CreateSpec {
        template: Some("my-workspace".into()),
        disk_gb: Some(10),
        ..spec()
    });
    assert_eq!(
        (body.get("template"), body.get("resources")),
        (
            Some(&json!("my-workspace")),
            Some(&json!({"cpu": 2, "memory": 4, "disk": 10}))
        )
    );
}

#[test]
fn create_body_is_exact() {
    assert_eq!(
        create_body(&spec()),
        json!({
            "template": "agent37-ubuntu",
            "type": "vm",
            "region": "us",
            "name": "crabbox-me--box",
            "resources": {"cpu": 2, "memory": 4},
            "metadata": {"crabbox_owner": "me"},
            "auto_sleep": false
        })
    );
}

#[test]
fn create_body_has_no_env_or_budget() {
    let body = create_body(&spec());
    let object = body.as_object();
    assert!(object.is_some_and(|o| !o.contains_key("env") && !o.contains_key("budget")));
}

#[test]
fn segment_is_encoded() {
    assert_eq!(encode_segment("a/b?c"), "a%2Fb%3Fc");
}

#[test]
fn decodes_string_metadata_in_key_order() {
    let wire: WireInstance = serde_json::from_value(json!({
        "id": "a",
        "metadata": {"z": "1", "a": "2", "n": 3}
    }))
    .unwrap();
    assert_eq!(
        Instance::from(wire).metadata,
        vec![("a".to_string(), "2".to_string()), ("z".into(), "1".into())]
    );
}

#[test]
fn real_ssh_key_item_shape_deserializes() {
    let body = r#"{"data":[{"id":"0688","name":"engine-03","fingerprint":"SHA256:+H2O2zoCMvtKtSi+xm3brOT1TZEPrpx7LXC63NAPh+I","type":"ssh-ed25519","created":1792075879}]}"#;
    let listing: Listing<WireKey> = serde_json::from_str(body).unwrap();
    let key = listing.data.first().unwrap();
    assert_eq!(key.id, "0688");
    assert_eq!(
        key.fingerprint,
        "SHA256:+H2O2zoCMvtKtSi+xm3brOT1TZEPrpx7LXC63NAPh+I"
    );
}

#[test]
fn null_or_missing_metadata_is_empty() {
    for body in [json!({"id": "a", "metadata": null}), json!({"id": "a"})] {
        let wire: WireInstance = serde_json::from_value(body).unwrap();
        assert!(Instance::from(wire).metadata.is_empty());
    }
}
