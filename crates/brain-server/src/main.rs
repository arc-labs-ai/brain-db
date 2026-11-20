//! The `brain-server` binary.
//!
//! Everything lives in the library beside this file; a binary that is only an
//! entry point is what lets the integration tests import the crate normally.

use std::process::ExitCode;

fn main() -> ExitCode {
    brain_server::run()
}
