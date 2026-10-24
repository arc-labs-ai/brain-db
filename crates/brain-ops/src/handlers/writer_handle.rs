//! Shared ground for every handler that writes through `submit(Write)`.
//!
//! `OpsContext` hands out the writer as a trait object, but the unified write
//! path is not part of that trait: `submit(Write)`, the idempotency cache and
//! the phase acks all live on the concrete [`RealWriterHandle`]. So every
//! handler on the unified path has to recover the concrete type first, and
//! sixteen of them did — through a `pub(crate)` helper that lived in `link.rs`
//! and carried a `_pub` suffix marking that it had been widened for exactly
//! that reach-around. Nothing about recovering a writer handle belongs to
//! LINK; `link.rs` was just the first module to migrate.
//!
//! The module boundary is "how a handler gets the concrete writer it submits
//! through", not "whichever handler migrated to the unified path first".

use crate::context::OpsContext;
use crate::error::OpError;
use crate::writer::RealWriterHandle;

/// Recover the concrete [`RealWriterHandle`] behind `ctx.executor.writer`.
///
/// The failure is an internal invariant break, not a client error: a request
/// only reaches a unified-path handler when the executor was built with the
/// real writer, so a stand-in writer here means the process was wired wrong.
pub(super) fn real_writer_handle(ctx: &OpsContext) -> Result<&RealWriterHandle, OpError> {
    ctx.executor
        .writer
        .as_any()
        .downcast_ref::<RealWriterHandle>()
        .ok_or_else(|| OpError::Internal("unified path requires RealWriterHandle".into()))
}
