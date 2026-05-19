//! `extract backfill --memory <id> | --since <ts> | --all` — admin op
//! that re-runs extraction for memories that never produced knowledge.
//!
//! Wire op required: `ExtractionBackfillReq` — currently exists only
//! as a CLI admin verb in `brain-cli` (per the plan), not as a wire
//! frame the SDK can dispatch.

use brain_sdk_rust::{Client, ClientError};

use crate::commands::Rendered;
use crate::parser::ExtractBackfillArgs;
use crate::session::Session;

pub async fn run(
    _client: &Client,
    _session: &mut Session,
    args: ExtractBackfillArgs,
) -> Result<Rendered, ClientError> {
    let scope = if let Some(m) = &args.memory {
        format!("memory 0x{:x}", m.0.raw())
    } else if let Some(ts) = args.since {
        format!("since unix_nanos={ts}")
    } else if args.all {
        "all unaudited memories".to_string()
    } else {
        return Err(ClientError::Internal(
            "extract backfill requires --memory, --since, or --all".into(),
        ));
    };

    tracing::warn!(
        target: "brain_shell",
        "extract backfill ({scope}): requires ExtractionBackfillReq wire op + \
         SDK builder; not wired yet.",
    );
    todo!("wire op required: ExtractionBackfillReq for `extract backfill ({scope})`.")
}
