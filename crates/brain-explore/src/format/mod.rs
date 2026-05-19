//! Output format dispatch: the [`Render`] trait, its [`RenderCtx`], and the
//! [`OutputFormat`] enum every consumer crate exposes as `--output`.

pub mod output_format;
pub mod render_trait;

pub use output_format::{OutputFormat, ParseOutputFormatError};
pub use render_trait::{dispatch, Render, RenderCtx};
