//! Right panel: a graph-tree view of the neighborhood around the
//! focused entity — relations out, relations in, and supporting
//! statements. Mirrors the `GraphTree` renderer used by
//! `brain recall --with-graph`. Today a stub.

#![allow(dead_code)]

/// Right-pane widget. The follow-up plan implements a collapsible
/// tree backed by `tui-tree-widget` or an equivalent.
#[derive(Debug, Default)]
pub struct NeighborhoodPanel;
