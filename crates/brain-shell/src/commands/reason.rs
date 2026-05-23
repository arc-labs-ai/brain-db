//! `reason` verb.

use brain_protocol::envelope::request::ObservationInput;
use brain_sdk_rust::{Client, ClientError};

use crate::parser::ReasonArgs;
use crate::session::Session;
use brain_explore::ReasonSteps;

use super::Rendered;

pub async fn run(
    client: &Client,
    _session: &mut Session,
    args: ReasonArgs,
) -> Result<Rendered, ClientError> {
    let inferences = client
        .reason(ObservationInput::ByText(args.observation))
        .depth(args.depth)
        .confidence_threshold(args.confidence)
        .max_inferences(args.max_inferences)
        .send()
        .await?;
    Ok(Box::new(ReasonSteps(inferences)))
}
