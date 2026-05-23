//! One module per verb. Each `run` borrows the SDK `Client`,
//! mutates the `Session` where appropriate, and returns a boxed
//! [`Render`] for the dispatch loop to format.

pub mod encode;
pub mod entity;
pub mod forget;
pub mod info;
pub mod link;
pub mod mention;
pub mod plan;
pub mod reason;
pub mod recall;
pub mod relation;
pub mod statement;
pub mod subscribe;
pub mod txn;
pub mod unlink;

use brain_explore::{
    term::{ColorMode as ExploreColorMode, HyperlinkMode as ExploreHyperlinkMode},
    OutputFormat, Render, RenderCtx, TermPolicy, Theme,
};
use brain_protocol::ErrorCodeWire;
use brain_sdk_rust::ClientError;

use crate::parser::{ColorMode, HyperlinkMode, OutputFormatArg};

/// Boxed `Render` return for every verb — the dispatch loop owns
/// the buffer + format choice.
pub type Rendered = Box<dyn Render>;

/// Build the `RenderCtx` the brain-explore dispatcher consumes.
///
/// Centralised so the one-shot dispatcher and the REPL loop don't drift
/// on how they translate the shell's clap-flag enums into brain-explore's
/// types. `color` / `hyperlinks` flow through `TermPolicy::detect` which
/// reconciles them with NO_COLOR / CLICOLOR / isatty.
#[must_use]
pub fn render_ctx(
    output: OutputFormatArg,
    color: ColorMode,
    hyperlinks: HyperlinkMode,
) -> RenderCtx {
    let explore_color: ExploreColorMode = color.into();
    let explore_hyperlinks: ExploreHyperlinkMode = hyperlinks.into();
    RenderCtx {
        policy: TermPolicy::detect(explore_color, explore_hyperlinks),
        theme: Theme::default(),
        format: OutputFormat::from(output),
    }
}

/// `true` when the server's response says the transaction we used is
/// no longer usable from this session — either the id was never
/// created (`TxnNotFound`) or it exists but is no longer Active
/// (`TransactionTimeout`). Both mean: discard the active_txn the
/// session is tracking and warn the user.
///
/// Returns `false` for every other error, including
/// `IdempotencyConflict` (which is unrelated to txn lifetime).
#[must_use]
pub fn is_txn_terminal(err: &ClientError) -> bool {
    let Some(code) = err.code() else {
        return false;
    };
    code == ErrorCodeWire::TxnNotFound as u16 || code == ErrorCodeWire::TransactionTimeout as u16
}

/// Translate a [`ClientError`] into the renderer's card type. The
/// wire `code` (when present) drives the heading; client-side
/// failures (Connect/Io/Pool/Closed/Internal) collapse to the
/// `Client` category with code 0.
#[must_use]
pub fn client_error_to_renderable(err: &ClientError) -> brain_explore::RenderableError {
    use brain_explore::RenderableError;
    match err {
        ClientError::Server { code, message } => RenderableError::from_server(*code, message),
        ClientError::RetryExhausted {
            last_error,
            attempts,
            total_duration,
        } => {
            // Surface the underlying error's code (so the heading
            // reads "RateLimited" or whatever) plus the retry
            // exhaustion in the message body.
            let inner_code = last_error.code().unwrap_or(0);
            let msg = format!(
                "retry exhausted after {attempts} attempt(s) in {:?}: {last_error}",
                total_duration,
            );
            RenderableError::from_server(inner_code, msg)
        }
        // Client-side variants — no wire code; preserve the Display
        // message which already names the failure mode.
        _ => RenderableError::client_side(err.to_string()),
    }
}
