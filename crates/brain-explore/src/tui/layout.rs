//! Three-panel layout helper for the explorer screen: a left browser
//! (entity / statement list), a center detail card, and a right
//! neighborhood / graph tree. The follow-up plan turns this into a
//! ratatui `Layout` builder; today it is a compile-only stub.

#![allow(dead_code)]

/// Logical names for the three panels the explorer screen carves out
/// of the terminal. Kept here so panel modules can refer to them by
/// name before the real layout math lands.
#[derive(Debug, Clone, Copy)]
pub enum PanelSlot {
    Browser,
    Detail,
    Neighborhood,
}
