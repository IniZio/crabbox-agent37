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

/// The agent37 REST calls the adapter makes.
pub trait Agent37 {
    /// `GET /v1/instances`.
    fn list_instances(&self) -> Result<Vec<Instance>, ApiError>;
    /// `GET /v1/ssh-keys`.
    fn list_ssh_keys(&self) -> Result<Vec<SshKey>, ApiError>;
    /// `POST /v1/instances` for a smallest-shape `type: vm` instance named `name`.
    fn create_probe_vm(&self, name: &str) -> Result<Instance, ApiError>;
    /// `DELETE /v1/instances/{id}`.
    fn delete_instance(&self, id: &str) -> Result<(), ApiError>;
}
