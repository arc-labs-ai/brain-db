//! `extract` commands — extraction audit + backfill admin ops.

pub mod backfill;
pub mod status;

use brain_sdk_rust::{Client, ClientError};

use crate::parser::ExtractCommand;
use crate::session::Session;

use super::Rendered;

pub async fn run(
    client: &Client,
    session: &mut Session,
    cmd: ExtractCommand,
) -> Result<Rendered, ClientError> {
    match cmd {
        ExtractCommand::Status(args) => status::run(client, session, args).await,
        ExtractCommand::Backfill(args) => backfill::run(client, session, args).await,
    }
}

#[must_use]
pub fn op_name(cmd: &ExtractCommand) -> &'static str {
    match cmd {
        ExtractCommand::Status(_) => "extract_status",
        ExtractCommand::Backfill(_) => "extract_backfill",
    }
}
