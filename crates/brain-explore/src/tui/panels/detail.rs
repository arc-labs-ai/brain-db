//! Center panel: the focused card for whatever the browser has
//! selected — an entity card, a statement card, a relation card, or
//! a recall result. Reuses the user-domain renderers from
//! `brain_explore::render::*` adapted to ratatui buffers. Today a
//! stub.

#![allow(dead_code)]

/// Center-pane widget. The follow-up plan wires this to the existing
/// card renderers so the TUI shows the same card shapes the CLI
/// already prints.
#[derive(Debug, Default)]
pub struct DetailPanel;
