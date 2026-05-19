//! `brain-explore` binary. Currently a placeholder — the real
//! interactive TUI lands in a follow-up plan. We ship this stub so
//! the binary target is reserved in cargo and the rest of the
//! library can already be consumed by brain-shell + brain-cli.

use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!("brain-explore: the interactive explorer ships in a follow-up plan.");
    eprintln!("for now, use `brain` (the REPL/CLI) or `brain-cli` (the admin tool).");
    // EX_USAGE — well-formed invocation, just nothing to do yet.
    ExitCode::from(64)
}
