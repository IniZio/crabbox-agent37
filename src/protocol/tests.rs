use super::*;

#[test]
fn parses_doctor() {
    let req = parse_request(
        b"{\"protocolVersion\":1,\"operation\":\"doctor\",\"config\":{\"a\":1},\"x\":2}\n",
    )
    .unwrap();
    assert_eq!(req.operation, Operation::Doctor);
    assert_eq!(req.config.get("a"), Some(&Value::from(1)));
}

#[test]
fn absent_config_is_null() {
    let req = parse_request(br#"{"protocolVersion":1,"operation":"list"}"#).unwrap();
    assert_eq!(req.config, Value::Null);
}

#[test]
fn rejects_version_two() {
    let err = parse_request(br#"{"protocolVersion":2,"operation":"doctor"}"#).unwrap_err();
    assert_eq!(
        err,
        "unsupported protocol version 2; this adapter speaks version 1"
    );
}

#[test]
fn rejects_missing_version() {
    assert!(parse_request(br#"{"operation":"doctor"}"#).is_err());
}

#[test]
fn rejects_unknown_operation() {
    let err = parse_request(br#"{"protocolVersion":1,"operation":"foo"}"#).unwrap_err();
    assert_eq!(err, "unknown operation \"foo\"");
}

#[test]
fn rejects_missing_operation() {
    assert!(parse_request(br#"{"protocolVersion":1}"#).is_err());
}

#[test]
fn rejects_invalid_json() {
    assert!(parse_request(b"not json").is_err());
}

#[test]
fn rejects_non_object_config() {
    let err =
        parse_request(br#"{"protocolVersion":1,"operation":"doctor","config":"x"}"#).unwrap_err();
    assert!(err.contains("config"));
}

#[test]
fn errors_do_not_echo_config() {
    let err = parse_request(
        br#"{"protocolVersion":3,"operation":"doctor","config":{"apiKey":"sk_live_secret"}}"#,
    )
    .unwrap_err();
    assert!(!err.contains("sk_live_secret"));
}

#[test]
fn responses_escape_quotes() {
    assert_eq!(
        message_response("say \"hi\""),
        r#"{"protocolVersion":1,"message":"say \"hi\""}"#
    );
    assert_eq!(error_response("a\"b"), r#"{"error":"a\"b"}"#);
    assert_eq!(
        message_response("ready"),
        r#"{"protocolVersion":1,"message":"ready"}"#
    );
}

fn parse(tail: &str) -> Result<Request, String> {
    let head = r#"{"protocolVersion":1,"operation":"acquire","#;
    parse_request(format!("{head}{tail}").as_bytes())
}

#[test]
fn parses_acquire_with_desired_class_keep() {
    let d = r#""desired":{"leaseId":"l1","slug":"s","name":"n","class":"fast"}"#;
    let req = parse(&format!(r#"{d},"keep":true}}"#)).unwrap();
    assert_eq!(req.desired.unwrap().class.as_deref(), Some("fast"));
    assert!(req.keep);
}

#[test]
fn class_absent_and_desired_errors() {
    let req = parse(r#""desired":{"leaseId":"l1","slug":"s","name":"n"}}"#).unwrap();
    assert_eq!(req.desired.unwrap().class, None);
    assert!(parse(r#""desired":{"leaseId":"l1","slug":"s"}}"#).is_err());
    assert!(parse(r#""desired":"x"}"#).is_err());
}

#[test]
fn expected_empty_fields_are_none() {
    let req = parse(r#""expected":{"leaseId":"l1","slug":""}}"#).unwrap();
    let (id, slug) = req.expected.map(|e| (e.lease_id, e.slug)).unwrap();
    assert_eq!((id.as_deref(), slug), (Some("l1"), None));
    assert!(parse(r#""expected":3}"#).is_err());
    assert!(parse(r#""keep":"y"}"#).is_err());
}

#[test]
fn lease_response_shape() {
    let mut lease = Lease {
        lease_id: "l1".to_owned(),
        slug: "s".to_owned(),
        name: "n".to_owned(),
        cloud_id: "vm1".to_owned(),
        status: "ready".to_owned(),
        ssh: Ssh {
            user: "root".to_owned(),
            host: "vm1.agent37.app".to_owned(),
            port: "22".to_owned(),
            proxy_command: "x tunnel %h".to_owned(),
            ready_check: "true".to_owned(),
            host_key: None,
        },
        cache_volumes: Vec::new(),
    };
    let base = r#"{"protocolVersion":1,"lease":{"leaseId":"l1","slug":"s","name":"n","cloudId":"vm1","status":"ready","ssh":{"user":"root","host":"vm1.agent37.app","port":"22","proxyCommand":"x tunnel %h","readyCheck":"true""#;
    assert_eq!(lease_response(&lease), format!("{base}}}}}}}"));
    lease.ssh.host_key = Some("ssh-ed25519 AAAA".to_owned());
    let with_key = format!(r#"{base},"hostKey":"ssh-ed25519 AAAA"}}}}}}"#);
    assert_eq!(lease_response(&lease), with_key);
}

#[test]
fn parses_lease_ref_ignoring_other_keys() {
    let req = parse(
        r#""lease":{"leaseId":"l","slug":"s","name":"n","cloudId":"c","status":"x","labels":{}}}"#,
    )
    .unwrap();
    assert_eq!(
        req.lease,
        Some(LeaseRef {
            lease_id: Some("l".into()),
            slug: Some("s".into()),
            name: Some("n".into()),
            cloud_id: Some("c".into()),
        })
    );
}

#[test]
fn lease_absent_or_wrong_type() {
    assert_eq!(parse(r#""x":1}"#).unwrap().lease, None);
    assert!(parse(r#""lease":"l"}"#).is_err());
}

#[test]
fn parses_dry_run_and_release_only() {
    let req = parse(r#""dryRun":true,"releaseOnly":true}"#).unwrap();
    assert!(req.dry_run);
    assert!(req.release_only);
    let req = parse(r#""x":1}"#).unwrap();
    assert!(!req.dry_run);
    assert!(!req.release_only);
}

#[test]
fn rejects_non_bool_flags() {
    assert_eq!(
        parse(r#""dryRun":"y"}"#).unwrap_err(),
        "dryRun must be a boolean"
    );
    assert_eq!(
        parse(r#""releaseOnly":1}"#).unwrap_err(),
        "releaseOnly must be a boolean"
    );
}

#[test]
fn empty_leases_response_is_array() {
    assert_eq!(leases_response(&[]), r#"{"protocolVersion":1,"leases":[]}"#);
}

#[test]
fn leases_response_uses_camel_case() {
    let row = LeaseRow {
        lease_id: "l".into(),
        slug: "s".into(),
        name: "n".into(),
        cloud_id: "c".into(),
        status: "running".into(),
    };
    assert_eq!(
        leases_response(&[row]),
        r#"{"protocolVersion":1,"leases":[{"leaseId":"l","slug":"s","name":"n","cloudId":"c","status":"running"}]}"#
    );
}

#[test]
fn ok_response_is_bare_version() {
    assert_eq!(ok_response(), r#"{"protocolVersion":1}"#);
}

/// `desired` with the given `cacheVolumes` JSON.
fn with_volumes(volumes: &str) -> Result<Request, String> {
    parse(&format!(
        r#""desired":{{"leaseId":"l","slug":"s","name":"n","cacheVolumes":{volumes}}}}}"#
    ))
}

#[test]
fn parses_cache_volumes_and_rejects_bad_shapes() {
    let req = with_volumes(
        r#"[{"group":"g","name":"docker","key":"g-docker","paths":["~/docker"],"sizeGB":3,"sealed":true}]"#,
    )
    .unwrap();
    let volumes = req.desired.unwrap().cache_volumes;
    assert_eq!(
        volumes,
        [CacheVolumeRequest {
            group: "g".into(),
            name: "docker".into(),
            key: "g-docker".into(),
            paths: vec!["~/docker".into()],
            size_gb: 3,
            sealed: true,
        }]
    );
    assert!(with_volumes("null")
        .unwrap()
        .desired
        .unwrap()
        .cache_volumes
        .is_empty());
    for bad in [
        "{}",
        "[1]",
        r#"[{"paths":"x"}]"#,
        r#"[{"paths":[1]}]"#,
        r#"[{"sizeGB":-1}]"#,
        r#"[{"sealed":"y"}]"#,
    ] {
        assert!(with_volumes(bad).is_err(), "{bad}");
    }
}

#[test]
fn cache_echo_serialises_name_outcome_and_detail() {
    let echo = CacheVolumeEcho {
        name: "npm".into(),
        outcome: "restored".into(),
        detail: "gen 1".into(),
    };
    let text = serde_json::to_string(&echo).unwrap();
    assert_eq!(
        text,
        r#"{"name":"npm","outcome":"restored","detail":"gen 1"}"#
    );
}
