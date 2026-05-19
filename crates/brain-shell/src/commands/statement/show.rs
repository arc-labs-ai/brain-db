//! `statement show <id>` — single-card view (evidence + chain).

use brain_core::knowledge::{StatementObject, SubjectRef};
use brain_sdk_rust::{Client, ClientError, StatementId};
use std::io::{self, Write};
use uuid::Uuid;

use crate::commands::Rendered;
use crate::output::Render;
use crate::parser::StatementShowArgs;
use crate::session::Session;
use serde_json::{json, Value};

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
    Ok(Box::new(StatementCard(handle)))
}

struct StatementCard(brain_sdk_rust::StatementHandle);

impl Render for StatementCard {
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()> {
        let h = &self.0;
        writeln!(w, "Statement {}", h.id.0)?;
        writeln!(w, "  kind        = {:?}", h.kind)?;
        let subj = match h.subject {
            SubjectRef::Entity(id) => format!("entity {}", id.0),
            SubjectRef::Pending(audit) => format!("pending audit {}", audit.0),
        };
        writeln!(w, "  subject     = {subj}")?;
        writeln!(w, "  predicate   = {}", h.predicate)?;
        let obj = match &h.object {
            StatementObject::Entity(id) => format!("entity {}", id.0),
            StatementObject::Value(v) => format!("value {v:?}"),
            StatementObject::Memory(m) => format!("memory 0x{:032x}", m.raw()),
            StatementObject::Statement(s) => format!("statement {}", s.0),
        };
        writeln!(w, "  object      = {obj}")?;
        writeln!(w, "  confidence  = {:.4}", h.confidence)?;
        writeln!(w, "  evidence    = {:?}", h.evidence)?;
        writeln!(w, "  version     = {}", h.version)?;
        if h.tombstoned {
            writeln!(w, "  TOMBSTONED  reason={:?}", h.tombstone_reason)?;
        }
        if let Some(s) = h.superseded_by {
            writeln!(w, "  superseded_by = {}", s.0)?;
        }
        Ok(())
    }

    fn to_json_value(&self) -> Value {
        let h = &self.0;
        json!({
            "id": h.id.0.to_string(),
            "kind": format!("{:?}", h.kind),
            "predicate": h.predicate,
            "confidence": h.confidence,
            "version": h.version,
            "tombstoned": h.tombstoned,
        })
    }
}
