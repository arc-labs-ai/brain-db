//! Subcommand implementations. Each command is a `pub fn
//! run(server, output) -> Result<String>` that returns the
//! rendered output; `main` prints it and sets the exit code.

pub mod agent;
pub mod audit;
pub mod config;
pub mod diagnostics;
pub mod extract;
pub mod health;
pub mod rebuild;
pub mod shard;
pub mod snapshot;
pub mod stats;
pub mod worker;
