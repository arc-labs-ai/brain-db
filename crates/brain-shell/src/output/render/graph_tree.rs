//! Termtree-based neighborhood renderer for `entity neighbors`.
//!
//! Wraps `termtree::Tree` so a multi-hop entity graph prints as a
//! depth-bounded ASCII tree. Each node carries a label (typically
//! `name (Type)`); the renderer doesn't itself fetch — the caller
//! supplies the already-traversed [`GraphNode`] tree built from
//! relation list responses.

use std::io::{self, Write};

use serde_json::{json, Value};

use crate::output::Render;

/// One node in the neighborhood. The caller supplies the depth-bounded
/// tree; this renderer is purely presentational.
#[derive(Debug, Clone)]
pub struct GraphNode {
    pub label: String,
    pub children: Vec<GraphNode>,
}

/// `Render` wrapper around a [`GraphNode`] root.
pub struct GraphTree(pub GraphNode);

impl Render for GraphTree {
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()> {
        let tree = build_termtree(&self.0);
        write!(w, "{tree}")
    }

    fn to_json_value(&self) -> Value {
        node_to_json(&self.0)
    }
}

fn build_termtree(node: &GraphNode) -> termtree::Tree<String> {
    let mut t = termtree::Tree::new(node.label.clone());
    for child in &node.children {
        t.push(build_termtree(child));
    }
    t
}

fn node_to_json(node: &GraphNode) -> Value {
    json!({
        "label": node.label,
        "children": node.children.iter().map(node_to_json).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_root_with_two_children() {
        let root = GraphNode {
            label: "Priya (Person)".into(),
            children: vec![
                GraphNode {
                    label: "Acme Corp (Org) [works_at]".into(),
                    children: vec![],
                },
                GraphNode {
                    label: "Bob (Person) [knows]".into(),
                    children: vec![],
                },
            ],
        };
        let mut buf = Vec::new();
        GraphTree(root).render_table(&mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Priya"));
        assert!(s.contains("Acme"));
        assert!(s.contains("Bob"));
        // termtree uses unicode box characters by default.
        assert!(s.contains('├') || s.contains('└'));
    }

    #[test]
    fn json_round_trip_preserves_structure() {
        let root = GraphNode {
            label: "root".into(),
            children: vec![GraphNode {
                label: "child".into(),
                children: vec![],
            }],
        };
        let v = GraphTree(root).to_json_value();
        assert_eq!(v["label"], "root");
        assert_eq!(v["children"][0]["label"], "child");
    }
}
