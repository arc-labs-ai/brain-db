//! Stacked-card and tree renderers used by knowledge-layer commands.
//!
//! Each submodule provides one [`crate::output::Render`]-compatible
//! renderer. `entity_card` produces the flyctl-style stacked card;
//! `graph_tree` wraps `termtree` for neighborhood views;
//! `recall_with_graph` produces the per-hit stacked RECALL output with
//! linked entities and statements.

pub mod entity_card;
pub mod graph_tree;
pub mod recall_with_graph;
