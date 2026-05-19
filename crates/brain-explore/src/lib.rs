//! brain-explore — terminal UI/UX layer for Brain.
//!
//! Used by both `brain-shell` (the user-facing REPL/CLI) and `brain-cli`
//! (the admin/operator tool) so both binaries render with identical look,
//! flags, and policy. Owning color, hyperlink, pager, and table conventions
//! in one place is the only way to keep two CLIs from drifting apart over
//! time.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub mod format;
pub mod table;
pub mod term;
pub mod theme;
pub mod util;

pub use format::{dispatch, OutputFormat, Render, RenderCtx};
pub use term::policy::TermPolicy;
pub use theme::{Theme, Token};
