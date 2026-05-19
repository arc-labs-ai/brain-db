//! `brain` — interactive shell + one-shot CLI for the Brain
//! cognitive substrate. The `psql` / `redis-cli` / `mongosh`
//! equivalent. Speaks the binary wire protocol via
//! [`brain_sdk_rust::Client`].
//!
//! # Two modes, one parser
//!
//! Both invocation modes are parsed by the same `clap` tree
//! ([`parser::Cli`]). Each REPL line is tokenised and fed through
//! `Cli::try_parse_from` exactly as if it were argv:
//!
//! ```text
//! $ brain encode "hello" --context 1          # one-shot
//! $ brain
//! brain> encode "hello" --context 1           # REPL
//! ```
//!
//! # Module map
//!
//! - [`parser`]  — clap [`Cli`] + [`Command`] + tokeniser.
//! - [`commands`] — one module per verb, all returning a uniform
//!   [`output::Rendered`] envelope.
//! - [`connection`] — wrap [`brain_sdk_rust::Client`] with the
//!   shell's defaults (timeout, retry, agent id).
//! - [`session`] — REPL state across lines (active txn, sticky
//!   context, recent ids).
//! - [`output`] — table + JSON renderers.
//! - [`repl`] — rustyline editor, completion, event loop.
//! - [`cli`] — one-shot dispatch.

#![forbid(unsafe_code)]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

use std::process::ExitCode;

pub mod cli;
pub mod commands;
pub mod connection;
pub mod output;
pub mod parser;
pub mod repl;
pub mod session;

/// Top-level entry — parses argv, dispatches to one-shot or REPL.
/// Returns a process exit code (0 success, 1 op failure, 2 usage
/// error).
pub async fn run() -> ExitCode {
    cli::dispatch_argv(std::env::args().collect()).await
}
