//! `mention` browse commands.

pub mod list;

use brain_sdk_rust::{Client, ClientError};

use crate::parser::MentionCommand;
use crate::session::Session;

use super::Rendered;

pub async fn run(
    client: &Client,
    session: &mut Session,
    cmd: MentionCommand,
) -> Result<Rendered, ClientError> {
    match cmd {
        MentionCommand::List(args) => list::run(client, session, args).await,
    }
}

#[must_use]
pub fn op_name(cmd: &MentionCommand) -> &'static str {
    match cmd {
        MentionCommand::List(_) => "mention_list",
    }
}
