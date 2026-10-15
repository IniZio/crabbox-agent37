//! Reads one protocol request on stdin and writes one response on stdout.

use std::io::{Read, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut stderr = std::io::stderr();
    let mut input = Vec::new();
    if let Err(error) = std::io::stdin().read_to_end(&mut input) {
        let _ = writeln!(stderr, "crabbox-agent37: reading stdin: {error}");
    }
    let env_key = std::env::var("AGENT37_API_KEY").ok();
    let outcome = crabbox_agent37::run(&input, env_key, &mut stderr);
    let mut stdout = std::io::stdout();
    if writeln!(stdout, "{}", outcome.stdout).is_err() {
        return ExitCode::from(1);
    }
    ExitCode::from(outcome.exit_code)
}
