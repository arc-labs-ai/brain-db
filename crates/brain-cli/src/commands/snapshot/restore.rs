//! `brain-cli snapshot restore <id>` — stub.
//!
//! Spec §14/06 §5 calls restore "destructive — current data is
//! lost"; production restore needs the substrate stopped and is
//! a runbook step, not a one-liner. Deferred to v2 / Phase 11.

use crate::cli::OutputFormat;

pub fn run(_server: &str, id: u64, _output: OutputFormat) -> anyhow::Result<String> {
    Ok(format!(
        "snapshot restore <id={id}>: not yet supported in v1.\n\
         Spec §14/06 §5: restore is destructive and requires the substrate to be stopped.\n\
         The v1 workflow is: stop brain-server → swap files → restart. A scripted\n\
         online-restore landing in v2 is tracked separately.\n"
    ))
}
