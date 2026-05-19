//! `relation list [--from E] [--to E] [--type T]` — table.

use brain_sdk_rust::{Client, ClientError, EntityId, RelationHandle};
use uuid::Uuid;

use crate::commands::Rendered;
use crate::output::table::AdHocTable;
use crate::parser::RelationListArgs;
use crate::session::Session;

pub async fn run(
    client: &Client,
    _session: &mut Session,
    args: RelationListArgs,
) -> Result<Rendered, ClientError> {
    let from = match &args.from {
        Some(s) => Some(parse_entity_id(s)?),
        None => None,
    };
    let to = match &args.to {
        Some(s) => Some(parse_entity_id(s)?),
        None => None,
    };

    let handles: Vec<RelationHandle> = match (from, to) {
        (Some(entity), None) => {
            let mut b = client.relations().list_from(entity).limit(args.limit);
            if let Some(t) = args.type_qname.clone() {
                b = b.with_type(t);
            }
            b.send().await?
        }
        (None, Some(entity)) => {
            let mut b = client.relations().list_to(entity).limit(args.limit);
            if let Some(t) = args.type_qname.clone() {
                b = b.with_type(t);
            }
            b.send().await?
        }
        (Some(_), Some(_)) => {
            tracing::warn!(
                target: "brain_shell",
                "relation list --from + --to: cross-filter wire op not exposed; \
                 returning rows matching --from only.",
            );
            let entity = from.expect("matched Some");
            let mut b = client.relations().list_from(entity).limit(args.limit);
            if let Some(t) = args.type_qname.clone() {
                b = b.with_type(t);
            }
            let all = b.send().await?;
            let to_filter = to.expect("matched Some");
            all.into_iter()
                .filter(|r| r.to_entity == to_filter)
                .collect()
        }
        (None, None) => {
            return Err(ClientError::Internal(
                "relation list requires --from or --to (or both)".into(),
            ));
        }
    };

    let rows: Vec<Vec<String>> = handles
        .iter()
        .map(|r| {
            vec![
                r.id.0.to_string(),
                r.relation_type.clone(),
                r.from_entity.0.to_string(),
                r.to_entity.0.to_string(),
                format!("{:.2}", r.confidence),
            ]
        })
        .collect();
    Ok(Box::new(AdHocTable {
        headers: vec![
            "id".into(),
            "type".into(),
            "from".into(),
            "to".into(),
            "conf".into(),
        ],
        rows,
    }))
}

fn parse_entity_id(s: &str) -> Result<EntityId, ClientError> {
    Uuid::parse_str(s.trim())
        .map(EntityId)
        .map_err(|e| ClientError::Internal(format!("bad entity id `{s}`: {e}")))
}
