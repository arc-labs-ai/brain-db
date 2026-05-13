//! brain-cli internal library — exposed so integration tests
//! can call command functions directly without spawning the
//! binary.

#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]

pub mod cli;
pub mod commands;
pub mod http;
pub mod output;
