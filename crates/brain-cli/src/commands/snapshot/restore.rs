//! `brain-cli snapshot restore <id>` — stub.
//!
//! Restore is destructive — current data is lost — so production restore
//! needs the substrate stopped and is a runbook step, not a one-liner.
//! Deferred until the v2 online-restore design ships.

use crate::cli::OutputFormat;
use crate::output::{dispatch_to_string, render::snapshot::SnapshotRestoreStubRendered};

pub fn run(_server: &str, id: u64, output: OutputFormat) -> anyhow::Result<String> {
    dispatch_to_string(&SnapshotRestoreStubRendered(id), output)
}
