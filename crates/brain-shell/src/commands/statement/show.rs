//! `statement show <id>` — single-card view (evidence + chain).

use brain_core::knowledge::{StatementObject, SubjectRef};
use brain_explore::{ObjectRef, StatementCard};
use brain_sdk_rust::{Client, ClientError, StatementId};
use uuid::Uuid;

use crate::commands::Rendered;
use crate::parser::StatementShowArgs;
use crate::session::Session;

pub async fn run(
    client: &Client,
    _session: &mut Session,
    args: StatementShowArgs,
) -> Result<Rendered, ClientError> {
    let uuid = Uuid::parse_str(args.id.trim())
        .map_err(|e| ClientError::Internal(format!("bad statement id `{}`: {e}", args.id)))?;
    let id = StatementId::from_uuid(uuid);
    let handle = match client.statements().get(id).await? {
        Some(h) => h,
        None => {
            return Err(ClientError::Internal(format!(
                "statement not found: {}",
                args.id
            )))
        }
    };

    // Map the SDK handle to brain-explore's card. The handle carries
    // ids; canonical names need a follow-up lookup the shell doesn't
    // do today, so we surface the id strings for subject and entity
    // objects.
    let subject_canonical = match handle.subject {
        SubjectRef::Entity(id) => id.0.to_string(),
        SubjectRef::Pending(audit) => format!("pending({})", audit.0),
    };
    let object = match &handle.object {
        StatementObject::Entity(id) => ObjectRef::Entity {
            id: id.0.to_string(),
            name: id.0.to_string(),
        },
        StatementObject::Value(v) => ObjectRef::Literal(format!("{v:?}")),
        StatementObject::Memory(m) => ObjectRef::Literal(format!("memory 0x{:032x}", m.raw())),
        StatementObject::Statement(s) => ObjectRef::Literal(format!("statement {}", s.0)),
    };
    let card = StatementCard {
        id: handle.id.0.to_string(),
        kind: format!("{:?}", handle.kind),
        subject_canonical,
        predicate_qname: handle.predicate.clone(),
        object,
        confidence: handle.confidence,
        evidence_memories: Vec::new(),
    };
    Ok(Box::new(card))
}
