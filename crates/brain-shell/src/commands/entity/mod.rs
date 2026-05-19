//! `entity` browse commands. Backed by the wire entity-list / entity-get
//! ops via `brain-sdk-rust::knowledge::EntityClient`.

pub mod list;
pub mod neighbors;
pub mod show;

use brain_sdk_rust::{Client, ClientError};

use crate::parser::EntityCommand;
use crate::session::Session;

use super::Rendered;

pub async fn run(
    client: &Client,
    session: &mut Session,
    cmd: EntityCommand,
) -> Result<Rendered, ClientError> {
    match cmd {
        EntityCommand::List(args) => list::run(client, session, args).await,
        EntityCommand::Show(args) => show::run(client, session, args).await,
        EntityCommand::Neighbors(args) => neighbors::run(client, session, args).await,
    }
}

/// Op name for the dispatcher's JSON envelope.
#[must_use]
pub fn op_name(cmd: &EntityCommand) -> &'static str {
    match cmd {
        EntityCommand::List(_) => "entity_list",
        EntityCommand::Show(_) => "entity_show",
        EntityCommand::Neighbors(_) => "entity_neighbors",
    }
}
