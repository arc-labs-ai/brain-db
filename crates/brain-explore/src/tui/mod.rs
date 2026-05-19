//! Interactive Ratatui-based explorer. Stub structure; the real
//! implementation lands in a follow-up plan. The exported `explore()`
//! function exists so the binary at `src/bin/brain-explore.rs` can
//! call it. Submodules below mirror the layout the follow-up plan
//! will populate so file paths stay stable across the migration.

#![allow(dead_code)]

pub mod app;
pub mod event;
pub mod layout;
pub mod panels;
pub mod state;
pub mod widgets;

/// Connection arguments for the explorer. Will be expanded with auth
/// + agent selection in the follow-up plan; today it is a placeholder
/// so callers can already plumb through a server address.
#[derive(Debug, Default)]
pub struct Args {
    /// Optional override for the server endpoint. None means "use the
    /// same discovery path as `brain-shell` / `brain-cli`."
    pub server_addr: Option<String>,
}

/// Run the interactive explorer. Currently a placeholder — returns an
/// error so a caller that wires this up before the follow-up plan
/// lands gets a clear signal rather than a silent no-op.
pub fn explore(_args: Args) -> anyhow::Result<()> {
    anyhow::bail!(
        "brain-explore TUI is reserved for a follow-up plan; \
        this stub is in place so the crate structure stays put."
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn explore_stub_returns_error() {
        assert!(super::explore(super::Args::default()).is_err());
    }
}
