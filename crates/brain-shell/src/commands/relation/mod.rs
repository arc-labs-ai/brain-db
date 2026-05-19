//! `relation` browse commands.

pub mod list;

use brain_sdk_rust::{Client, ClientError};

use crate::parser::RelationCommand;
use crate::session::Session;

use super::Rendered;

pub async fn run(
    client: &Client,
    session: &mut Session,
    cmd: RelationCommand,
) -> Result<Rendered, ClientError> {
    match cmd {
        RelationCommand::List(args) => list::run(client, session, args).await,
    }
}

#[must_use]
pub fn op_name(cmd: &RelationCommand) -> &'static str {
    match cmd {
        RelationCommand::List(_) => "relation_list",
    }
}
