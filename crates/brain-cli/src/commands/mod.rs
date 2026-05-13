//! Subcommand implementations. Each command is a `pub fn
//! run(server, output) -> Result<String>` that returns the
//! rendered output; `main` prints it and sets the exit code.

pub mod health;
pub mod rebuild;
pub mod snapshot;
pub mod stats;
