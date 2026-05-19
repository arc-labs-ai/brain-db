//! `entity show <id|name>` — stacked-card view of a single entity.
//!
//! Pipeline: resolve name → `EntityHandle<Person>` → fan out to
//! `statement list --subject` and `relation list --from/--to` to fill
//! the card. The "Mentioned in" section requires a `mention list`-style
//! wire op — gated below until that lands.

use brain_sdk_rust::{Client, ClientError, Person};
use uuid::Uuid;

use brain_explore::EntityCard;

use crate::commands::Rendered;
use crate::parser::EntityShowArgs;
use crate::session::Session;

pub async fn run(
    client: &Client,
    _session: &mut Session,
    args: EntityShowArgs,
) -> Result<Rendered, ClientError> {
    let handle = if let Ok(uuid) = Uuid::parse_str(args.id_or_name.trim()) {
        let id = brain_sdk_rust::EntityId(uuid);
        match client.entity::<Person>().get(id).await? {
            Some(h) => h,
            None => {
                return Err(ClientError::Internal(format!(
                    "entity not found: {}",
                    args.id_or_name
                )));
            }
        }
    } else {
        tracing::warn!(
            target: "brain_shell",
            "entity show by name: depends on EntityResolveReq wiring; \
             ambiguous results not yet surfaced.",
        );
        let resolution = client
            .entity::<Person>()
            .resolve(args.id_or_name.clone())
            .send()
            .await?;
        let id = resolution.entity_id().ok_or_else(|| {
            ClientError::Internal(format!(
                "entity not resolved (NotFound or Ambiguous): {}",
                args.id_or_name
            ))
        })?;
        client.entity::<Person>().get(id).await?.ok_or_else(|| {
            ClientError::Internal("resolver found id but get returned None".into())
        })?
    };

    // Statements / relations / mentions need additional wire ops the
    // shell doesn't have today (statement-list-by-subject, relation-list-by-entity,
    // mention-list-by-entity). Scaffold the card with empty sections so
    // the renderer surface is exercised; a follow-up populates them.
    tracing::warn!(
        target: "brain_shell",
        "entity show: statements / mentions / relations sections empty \
         until statement-list-by-subject + mention-list-by-entity wire \
         ops are wired through the shell."
    );

    let card = EntityCard {
        id: handle.id.0.to_string(),
        canonical_name: handle.canonical_name,
        type_qname: "Person".to_string(),
        aliases: handle.aliases,
        statements: Vec::new(),
        mentioned_in: Vec::new(),
        relations_out: Vec::new(),
        relations_in: Vec::new(),
    };
    Ok(Box::new(card))
}
