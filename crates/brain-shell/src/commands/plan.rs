//! `plan` verb.

use brain_protocol::request::{PlanBudget, PlanState};
use brain_sdk_rust::{Client, ClientError};

use crate::output::table::PlanSteps;
use crate::parser::PlanArgs;
use crate::session::Session;

use super::Rendered;

pub async fn run(
    client: &Client,
    _session: &mut Session,
    args: PlanArgs,
) -> Result<Rendered, ClientError> {
    let budget = PlanBudget {
        max_steps: args.max_steps,
        max_wall_time_ms: args.max_wall_time_ms,
        max_branches_explored: 256,
    };
    let outcome = client
        .plan(PlanState::ByText(args.from), PlanState::ByText(args.to))
        .budget(budget)
        .send()
        .await?;
    Ok(Box::new(PlanSteps {
        steps: outcome.steps,
        status: outcome.status,
    }))
}
