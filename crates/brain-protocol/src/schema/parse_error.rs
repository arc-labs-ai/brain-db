//! Schema-DSL parse errors. Spec §21/01.
//!
//! Each variant carries a 1-based `line` / `col` position to keep
//! diagnostics actionable. Format mirrors compiler errors —
//! `syntax error at 12:34: expected 'kind:'`.

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    #[error("syntax error at {line}:{col}: {message}")]
    Syntax {
        line: usize,
        col: usize,
        message: String,
    },

    #[error("invalid number at {line}:{col}: {value:?}")]
    InvalidNumber {
        line: usize,
        col: usize,
        value: String,
    },

    #[error("invalid JSON at {line}:{col}: {message}")]
    InvalidJson {
        line: usize,
        col: usize,
        message: String,
    },

    #[error("invalid duration at {line}:{col}: {value:?}")]
    InvalidDuration {
        line: usize,
        col: usize,
        value: String,
    },

    #[error("invalid cost expression at {line}:{col}: {message}")]
    InvalidCost {
        line: usize,
        col: usize,
        message: String,
    },

    #[error("missing required field {field:?} at {line}:{col}")]
    MissingField {
        line: usize,
        col: usize,
        field: String,
    },
}
