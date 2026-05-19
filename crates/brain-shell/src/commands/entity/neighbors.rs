//! `entity neighbors <id> [--depth N]` — termtree neighborhood.
//!
//! Walks outgoing relations from the root entity to `depth`. The base
//! relation-traversal wire op (`RelationTraverseReq`) exists; binding
//! it to the typed `Person` variant covers built-in types. Mixed-type
//! traversals are gated until the shell can resolve arbitrary type
//! qnames.

use brain_sdk_rust::{Client, ClientError, Person};
use uuid::Uuid;

use crate::commands::Rendered;
use crate::output::render::graph_tree::{GraphNode, GraphTree};
use crate::parser::EntityNeighborsArgs;
use crate::session::Session;

pub async fn run(
    _client: &Client,
    _session: &mut Session,
    args: EntityNeighborsArgs,
) -> Result<Rendered, ClientError> {
    let _root = Uuid::parse_str(args.id.trim())
        .map(brain_sdk_rust::EntityId)
        .map_err(|e| ClientError::Internal(format!("bad entity id `{}`: {e}", args.id)))?;

    tracing::warn!(
        target: "brain_shell",
        "entity neighbors: traversal output is currently a placeholder. \
         Wire RelationTraverseBuilder for Person + add a list-relations-with-other-name \
         path so the tree carries readable labels.",
    );

    // Touch the typed Person entry so the compiler validates the SDK
    // surface exists for the follow-up wire-up. Argument is unused so
    // we don't actually send a request.
    let _ = std::marker::PhantomData::<Person>;

    let root = GraphNode {
        label: format!("{}  (depth {})", args.id, args.depth),
        children: vec![GraphNode {
            label: "(neighborhood traversal not yet wired)".into(),
            children: vec![],
        }],
    };
    Ok(Box::new(GraphTree(root)))
}
