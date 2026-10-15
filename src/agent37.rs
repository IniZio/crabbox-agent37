//! The agent37 operations the adapter needs, behind a trait so tests can fake them.

use std::fmt;

/// One agent37 instance, as returned by list and create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// Instance id; also the DNS label in `<id>.agent37.app`.
    pub id: String,
    /// Instance name, if one was set at create.
    pub name: Option<String>,
    /// `vm` or `default`.
    pub kind: Option<String>,
    /// Lifecycle status such as `running` or `stopped`.
    pub status: Option<String>,
}

/// One SSH public key registered with the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshKey {
    /// Key id.
    pub id: String,
    /// One OpenSSH public key line.
    pub public_key: String,
}

/// What `create_instance` asks agent37 for. The request body is built only from these fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateSpec {
    /// Instance name.
    pub name: String,
    /// Region, `us` or `eu`.
    pub region: String,
    /// vCPU count.
    pub cpu: u32,
    /// Memory in GB.
    pub memory_gb: u32,
    /// String metadata attached to the instance.
    pub metadata: Vec<(String, String)>,
}

/// Result of one exec call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    /// Process exit code; nonzero is still a successful call.
    pub exit_code: i64,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
}

/// A failed agent37 call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// No HTTP response: DNS, connect, TLS or timeout failure.
    Unreachable(String),
    /// A non-2xx response, with the agent37 error code when the body carried one.
    Http {
        /// HTTP status.
        status: u16,
        /// agent37 error code, for example `tier_limit` or `invalid_api_key`.
        code: Option<String>,
        /// Human-readable message from the body, or a generic one.
        message: String,
    },
    /// A 2xx response whose body did not parse.
    Decode(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(detail) => write!(f, "agent37 unreachable: {detail}"),
            Self::Http {
                status,
                code: Some(code),
                message,
            } => write!(f, "agent37 HTTP {status} {code}: {message}"),
            Self::Http {
                status,
                code: None,
                message,
            } => write!(f, "agent37 HTTP {status}: {message}"),
            Self::Decode(detail) => write!(f, "agent37 response did not parse: {detail}"),
        }
    }
}

/// Operator message for a workspace whose tier does not allow VM instances.
pub const NOT_FUNDED_FOR_VM: &str = "workspace not funded for VM instances";

impl ApiError {
    /// Text to show an operator: a known cause in plain words, else the Display text.
    #[must_use]
    pub fn operator_message(&self) -> String {
        match self {
            Self::Http {
                status: 403,
                code: Some(code),
                ..
            } if code == "tier_limit" => NOT_FUNDED_FOR_VM.to_owned(),
            other => other.to_string(),
        }
    }
}

/// The agent37 REST calls the adapter makes.
pub trait Agent37 {
    /// `GET /v1/instances`.
    fn list_instances(&self) -> Result<Vec<Instance>, ApiError>;
    /// `GET /v1/ssh-keys`.
    fn list_ssh_keys(&self) -> Result<Vec<SshKey>, ApiError>;
    /// `DELETE /v1/instances/{id}`.
    fn delete_instance(&self, id: &str) -> Result<(), ApiError>;
    /// `GET /v1/instances/{id}`.
    fn get_instance(&self, id: &str) -> Result<Instance, ApiError>;
    /// `POST /v1/instances`.
    fn create_instance(&self, spec: &CreateSpec) -> Result<Instance, ApiError>;
    /// `POST /v1/instances/{id}/exec`, run as root.
    fn exec_as_root(&self, id: &str, command: &str) -> Result<ExecOutput, ApiError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HTTP error with an optional agent37 code.
    fn http(status: u16, code: Option<&str>) -> ApiError {
        ApiError::Http {
            status,
            code: code.map(Into::into),
            message: "m".into(),
        }
    }

    #[test]
    fn tier_limit_403_maps_to_not_funded() {
        assert_eq!(
            http(403, Some("tier_limit")).operator_message(),
            NOT_FUNDED_FOR_VM
        );
    }

    #[test]
    fn other_403_code_is_not_mapped() {
        let err = http(403, Some("forbidden"));
        assert_ne!(err.operator_message(), NOT_FUNDED_FOR_VM);
        assert_eq!(err.operator_message(), err.to_string());
    }

    #[test]
    fn unauthorized_is_not_mapped() {
        assert_ne!(
            http(401, Some("tier_limit")).operator_message(),
            NOT_FUNDED_FOR_VM
        );
    }
}
