//! The explorer's runtime state. The follow-up plan grows this into
//! the full state machine — current focus, selected entity, expanded
//! tree nodes, last query, retry timers. Current agent_id stays in
//! AppState so the status bar can read it: agent identity is a
//! first-class noun in the Brain UX and lives at the top of the
//! layout, not buried in a generic "connection info" modal.

#![allow(dead_code)]

/// Placeholder shape for the explorer's app state. Field set is
/// intentionally minimal today; the follow-up plan adds focus, modal
/// stack, selection cursors, and per-panel substate.
#[derive(Debug, Default)]
pub struct AppState {
    /// Identity of the agent whose memory the explorer is browsing.
    /// The status-bar widget will read this and render it pinned at
    /// the top of the screen.
    pub agent_id: Option<String>,
}
