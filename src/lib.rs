//! crabbox external provider adapter for agent37 VM instances.
//!
//! crabbox runs the binary once per operation: one request object on stdin, one
//! response object on stdout, diagnostics on stderr.

pub mod agent37;
pub mod api;
pub mod doctor;
pub mod key;
pub mod protocol;

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
        Ok(message) => Outcome {
            stdout: protocol::message_response(&message),
            exit_code: 0,
        },
        Err(error) => Outcome {
            stdout: protocol::error_response(&error),
            exit_code: 1,
        },
    }
}

/// Dispatches a parsed request to its operation.
fn handle(input: &[u8], env_key: Option<String>, diag: &mut dyn Write) -> Result<String, String> {
    let request = protocol::parse_request(input)?;
    match request.operation {
        protocol::Operation::Doctor => {
            let key = key::resolve(&request.config, env_key)?;
            let client = api::HttpAgent37::new(api::base_url(&request.config), key);
            doctor::run(&request.config, &client, diag)
        }
        other => Err(format!(
            "operation {} is not supported by this adapter version",
            other.as_str()
        )),
    }
}
