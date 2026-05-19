//! Thin entry — see `lib.rs` for the work.

use std::process::ExitCode;

fn main() -> ExitCode {
    // Tokio is required by the SDK (`brain_sdk_rust::Client` uses
    // `tokio::time::timeout` internally). One multi-thread runtime
    // for the whole process; the REPL drives async ops on it.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("invariant: tokio runtime build");
    rt.block_on(brain_shell::run())
}
