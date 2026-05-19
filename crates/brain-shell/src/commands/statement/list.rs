//! `statement list [--subject E] [--predicate P] [--object E]` — table.

use brain_core::knowledge::{StatementObject, SubjectRef};
use brain_sdk_rust::{Client, ClientError, StatementHandle};
use uuid::Uuid;

use crate::commands::Rendered;
use crate::output::table::AdHocTable;
use crate::parser::StatementListArgs;
use crate::session::Session;

pub async fn run(
    client: &Client,
    _session: &mut Session,
    args: StatementListArgs,
) -> Result<Rendered, ClientError> {
    let mut builder = client.statements().list().limit(args.limit);
    if let Some(s) = &args.subject {
        let uuid = Uuid::parse_str(s.trim())
            .map_err(|e| ClientError::Internal(format!("bad subject id `{s}`: {e}")))?;
        builder = builder.where_subject(brain_sdk_rust::EntityId(uuid));
    }
    if let Some(p) = args.predicate.clone() {
        builder = builder.where_predicate(p);
    }
    if args.object.is_some() {
        tracing::warn!(
            target: "brain_shell",
            "statement list --object: server-side object-filter wire op not exposed via \
             the SDK list builder. Filter omitted; consider follow-up support.",
        );
    }

    let handles = builder.send().await?;

    let rows: Vec<Vec<String>> = handles.iter().map(format_handle).collect();
    Ok(Box::new(AdHocTable {
        headers: vec![
            "id".into(),
            "kind".into(),
            "subject".into(),
            "predicate".into(),
            "object".into(),
            "conf".into(),
        ],
        rows,
    }))
}

fn format_handle(h: &StatementHandle) -> Vec<String> {
    vec![
        h.id.0.to_string(),
        format!("{:?}", h.kind),
        match h.subject {
            SubjectRef::Entity(id) => id.0.to_string(),
            SubjectRef::Pending(audit) => format!("pending({})", audit.0),
        },
        h.predicate.clone(),
        match &h.object {
            StatementObject::Entity(id) => format!("entity:{}", id.0),
            StatementObject::Value(v) => format!("value:{:?}", v),
            StatementObject::Memory(m) => format!("memory:0x{:032x}", m.raw()),
            StatementObject::Statement(s) => format!("statement:{}", s.0),
        },
        format!("{:.2}", h.confidence),
    ]
}
