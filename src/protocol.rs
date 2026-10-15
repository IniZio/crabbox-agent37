//! Protocol v1 framing: one JSON request in, one JSON response out.

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

/// A parsed request. Fields not yet needed are ignored.
#[derive(Debug, Clone)]
pub struct Request {
    /// Requested operation.
    pub operation: Operation,
    /// Provider config object, or `Value::Null` when absent.
    pub config: Value,
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

    Ok(Request { operation, config })
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
}
