//! Reads one protocol request on stdin and writes one response on stdout.

use std::io::{Read, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut stderr = std::io::stderr();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("tunnel") {
        let code = crabbox_agent37::tunnel::run(
            args.get(1..).unwrap_or_default(),
            std::env::var("AGENT37_API_KEY").ok(),
            std::env::var("AGENT37_KEY_FILE").ok(),
            std::env::var("AGENT37_TUNNEL_URL").ok(),
            std::env::var("CRABBOX_AGENT37_TUNNEL_LOG").ok().as_deref(),
            &mut stderr,
        );
        return ExitCode::from(code);
    }
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
