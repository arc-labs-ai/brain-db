//! `statement` browse commands.

pub mod list;
pub mod show;

use brain_sdk_rust::{Client, ClientError};

use crate::parser::StatementCommand;
use crate::session::Session;

use super::Rendered;

pub async fn run(
    client: &Client,
    session: &mut Session,
    cmd: StatementCommand,
) -> Result<Rendered, ClientError> {
    match cmd {
        StatementCommand::List(args) => list::run(client, session, args).await,
        StatementCommand::Show(args) => show::run(client, session, args).await,
    }
}

#[must_use]
pub fn op_name(cmd: &StatementCommand) -> &'static str {
    match cmd {
        StatementCommand::List(_) => "statement_list",
        StatementCommand::Show(_) => "statement_show",
    }
}
