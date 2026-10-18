//! crabbox external provider adapter for agent37 VM instances.
//!
//! crabbox runs the binary once per operation: one request object on stdin, one
//! response object on stdout, diagnostics on stderr.

pub mod agent37;
pub mod api;
pub mod doctor;
mod fingerprint;
mod host_key;
pub mod inventory;
pub mod key;
pub mod lease;
pub mod power;
pub mod protocol;
mod settings;
mod tools;
pub mod tunnel;

use std::io::Write;

/// What one invocation writes to stdout and the process exit code.
pub struct Outcome {
    /// One JSON response object, without a trailing newline.
    pub stdout: String,
    /// 0 on success, 1 on any error response.
    pub exit_code: u8,
}

/// Handles one request. `env_key` is the value of `AGENT37_API_KEY`, if set.
pub fn run(input: &[u8], env_key: Option<String>, diag: &mut dyn Write) -> Outcome {
    match handle(input, env_key, diag) {
        Ok(stdout) => Outcome {
            stdout,
            exit_code: 0,
        },
        Err(error) => Outcome {
            stdout: protocol::error_response(&error),
            exit_code: 1,
        },
    }
}

/// A lease operation: acquire, resolve or resume.
type LeaseOp = fn(
    &protocol::Request,
    &dyn agent37::Agent37,
    &std::path::Path,
    &mut dyn Write,
) -> Result<protocol::Lease, String>;

/// Builds the HTTP client from the request config and the environment key.
fn client(
    request: &protocol::Request,
    env_key: Option<String>,
) -> Result<api::HttpAgent37, String> {
    let key = key::resolve(&request.config, env_key)?;
    Ok(api::HttpAgent37::new(
        api::base_url(&request.config),
        key,
        api::request_timeout(&request.config),
    ))
}

/// Runs a lease operation and renders its lease response.
fn lease_op(
    request: &protocol::Request,
    env_key: Option<String>,
    diag: &mut dyn Write,
    op: LeaseOp,
) -> Result<String, String> {
    let client = client(request, env_key)?;
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate adapter binary: {e}"))?;
    op(request, &client, &exe, diag).map(|lease| protocol::lease_response(&lease))
}

/// Dispatches a parsed request to its operation; returns the stdout JSON.
fn handle(input: &[u8], env_key: Option<String>, diag: &mut dyn Write) -> Result<String, String> {
    let request = protocol::parse_request(input)?;
    match request.operation {
        protocol::Operation::Doctor => {
            let client = client(&request, env_key)?;
            doctor::run(&request.config, &client, diag).map(|m| protocol::message_response(&m))
        }
        protocol::Operation::Acquire => lease_op(&request, env_key, diag, lease::acquire),
        protocol::Operation::Resolve => lease_op(&request, env_key, diag, lease::resolve),
        protocol::Operation::List => {
            let rows = inventory::list(&request, &client(&request, env_key)?)?;
            Ok(protocol::leases_response(&rows))
        }
        protocol::Operation::Release => {
            inventory::release(&request, &client(&request, env_key)?)?;
            Ok(protocol::ok_response())
        }
        protocol::Operation::Touch => Ok(protocol::ok_response()),
        protocol::Operation::Cleanup => inventory::cleanup(&request, &client(&request, env_key)?)
            .map(|m| protocol::message_response(&m)),
        protocol::Operation::Pause => power::pause(&request, &client(&request, env_key)?, diag)
            .map(|m| protocol::message_response(&m)),
        protocol::Operation::Resume => lease_op(&request, env_key, diag, power::resume),
    }
}
