//! Protocol v1 framing: one JSON request in, one JSON response out.

use serde::Serialize;
use serde_json::Value;

/// The only protocol version this adapter speaks.
const VERSION: u64 = 1;

/// An operation named by the crabbox external provider protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Readiness check.
    Doctor,
    /// Create a lease.
    Acquire,
    /// Look up an existing lease.
    Resolve,
    /// Enumerate leases.
    List,
    /// Release a lease.
    Release,
    /// Refresh a lease.
    Touch,
    /// Remove stale resources.
    Cleanup,
}

impl Operation {
    /// Wire name of the operation.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Doctor => "doctor",
            Self::Acquire => "acquire",
            Self::Resolve => "resolve",
            Self::List => "list",
            Self::Release => "release",
            Self::Touch => "touch",
            Self::Cleanup => "cleanup",
        }
    }

    /// Inverse of [`Operation::as_str`].
    fn from_wire(name: &str) -> Option<Self> {
        [
            Self::Doctor,
            Self::Acquire,
            Self::Resolve,
            Self::List,
            Self::Release,
            Self::Touch,
            Self::Cleanup,
        ]
        .into_iter()
        .find(|op| op.as_str() == name)
    }
}

/// The lease crabbox wants created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    /// Crabbox lease id.
    pub lease_id: String,
    /// Short lease slug.
    pub slug: String,
    /// Lease name.
    pub name: String,
    /// Optional machine class.
    pub class: Option<String>,
}

/// Identifiers crabbox expects an existing lease to carry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expected {
    /// Crabbox lease id.
    pub lease_id: Option<String>,
    /// Lease id of the acquire attempt.
    pub attempt_lease_id: Option<String>,
    /// Short lease slug.
    pub slug: Option<String>,
    /// Provider-side instance id.
    pub cloud_id: Option<String>,
}

/// SSH connection details of a lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Ssh {
    /// Login user.
    pub user: String,
    /// Host name.
    pub host: String,
    /// Port, as the protocol's string.
    pub port: String,
    /// `ProxyCommand` line.
    pub proxy_command: String,
    /// Shell command that proves readiness.
    pub ready_check: String,
    /// OpenSSH public host key line, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_key: Option<String>,
}

/// A lease as reported back to crabbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    /// Crabbox lease id.
    pub lease_id: String,
    /// Short lease slug.
    pub slug: String,
    /// Lease name.
    pub name: String,
    /// Provider-side instance id.
    pub cloud_id: String,
    /// Lease status.
    pub status: String,
    /// SSH connection details.
    pub ssh: Ssh,
}

/// A parsed request. Fields not yet needed are ignored.
#[derive(Debug, Clone)]
pub struct Request {
    /// Requested operation.
    pub operation: Operation,
    /// Provider config object, or `Value::Null` when absent.
    pub config: Value,
    /// Lease to create, for acquire.
    pub desired: Option<Desired>,
    /// Identifiers to match, for resolve and others.
    pub expected: Option<Expected>,
    /// Keep the lease after the command finishes.
    pub keep: bool,
}

/// Non-empty string field, else `None`.
fn opt_str(object: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Parse an optional `key` object with `parse`; `null` counts as absent.
fn optional_object<T>(
    root: &serde_json::Map<String, Value>,
    key: &str,
    parse: impl FnOnce(&serde_json::Map<String, Value>) -> Result<T, String>,
) -> Result<Option<T>, String> {
    match root.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(object)) => parse(object).map(Some),
        Some(_) => Err(format!("{key} must be an object")),
    }
}

/// Parse a single request object.
///
/// # Errors
///
/// Returns a human-readable message for malformed input. Messages never
/// include config values, which may carry secrets.
pub fn parse_request(input: &[u8]) -> Result<Request, String> {
    let value: Value =
        serde_json::from_slice(input).map_err(|e| format!("invalid request JSON: {e}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "request must be a JSON object".to_owned())?;

    match object.get("protocolVersion").and_then(Value::as_u64) {
        Some(VERSION) => {}
        Some(other) => {
            return Err(format!(
                "unsupported protocol version {other}; this adapter speaks version {VERSION}"
            ));
        }
        None => {
            return Err(format!(
                "missing or invalid protocolVersion; this adapter speaks version {VERSION}"
            ));
        }
    }

    let name = object
        .get("operation")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing operation".to_owned())?;
    let operation = Operation::from_wire(name)
        .ok_or_else(|| format!("unknown operation {}", Value::String(name.to_owned())))?;

    let config = match object.get("config") {
        None | Some(Value::Null) => Value::Null,
        Some(config @ Value::Object(_)) => config.clone(),
        Some(_) => return Err("config must be an object".to_owned()),
    };

    let desired = optional_object(object, "desired", |o| {
        let required = |key: &str| {
            opt_str(o, key).ok_or_else(|| format!("desired.{key} must be a non-empty string"))
        };
        Ok(Desired {
            lease_id: required("leaseId")?,
            slug: required("slug")?,
            name: required("name")?,
            class: opt_str(o, "class"),
        })
    })?;
    let expected = optional_object(object, "expected", |o| {
        Ok(Expected {
            lease_id: opt_str(o, "leaseId"),
            attempt_lease_id: opt_str(o, "attemptLeaseId"),
            slug: opt_str(o, "slug"),
            cloud_id: opt_str(o, "cloudId"),
        })
    })?;
    let keep = object.get("keep").map_or(Ok(false), |v| {
        v.as_bool()
            .ok_or_else(|| "keep must be a boolean".to_owned())
    })?;

    Ok(Request {
        operation,
        config,
        desired,
        expected,
        keep,
    })
}

/// Success response carrying a message.
#[must_use]
pub fn message_response(message: &str) -> String {
    format!(
        r#"{{"protocolVersion":{VERSION},"message":{}}}"#,
        Value::String(message.to_owned())
    )
}

/// Error response; crabbox reads `error` before the version.
#[must_use]
pub fn error_response(error: &str) -> String {
    format!(r#"{{"error":{}}}"#, Value::String(error.to_owned()))
}

/// Success response carrying a lease.
#[must_use]
pub fn lease_response(lease: &Lease) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Envelope<'a> {
        protocol_version: u64,
        lease: &'a Lease,
    }
    let envelope = Envelope {
        protocol_version: VERSION,
        lease,
    };
    serde_json::to_string(&envelope).unwrap_or_default()
}

#[cfg(test)]
mod tests {
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
        let err = parse_request(br#"{"protocolVersion":1,"operation":"doctor","config":"x"}"#)
            .unwrap_err();
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
        };
        let base = r#"{"protocolVersion":1,"lease":{"leaseId":"l1","slug":"s","name":"n","cloudId":"vm1","status":"ready","ssh":{"user":"root","host":"vm1.agent37.app","port":"22","proxyCommand":"x tunnel %h","readyCheck":"true""#;
        assert_eq!(lease_response(&lease), format!("{base}}}}}}}"));
        lease.ssh.host_key = Some("ssh-ed25519 AAAA".to_owned());
        let with_key = format!(r#"{base},"hostKey":"ssh-ed25519 AAAA"}}}}}}"#);
        assert_eq!(lease_response(&lease), with_key);
    }
}
