//! `brain-cli extract` — extractor admin surface.
//!
//! Currently exposes a single sub-action — `extract --backfill` —
//! which re-enqueues existing memories through the three-tier
//! extractor pipeline. Used after enabling the extractor worker on a
//! populated shard or after a fresh schema upload.
//!
//! ## Surface
//!
//! ```text
//! brain-cli extract --backfill --memory-id <u128>
//! brain-cli extract --backfill --since <unix_nanos>
//! brain-cli extract --backfill --all
//! ```
//!
//! Exactly one of `--memory-id`, `--since`, `--all` must be present
//! alongside `--backfill`. The CLI POSTs to
//! `/v1/extract/backfill?<selector>` on the admin port and renders
//! the server's `{enqueued, skipped, shards}` reply.

pub mod backfill;

use anyhow::{anyhow, Result};

use crate::cli::args::FamilyFlags;
use crate::cli::OutputFormat;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractAction {
    Backfill(BackfillKind),
}

/// Mirrors `brain_protocol::requests::admin::BackfillSelector` but
/// without the protocol-crate dep — the CLI only renders + transports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfillKind {
    Memory(u128),
    Since(u64),
    All,
}

impl ExtractAction {
    /// Parse positional + family-flag arguments for the `extract`
    /// subcommand. The argv vector is everything after `extract` on
    /// the command line.
    pub fn parse(args: &[String], family: &FamilyFlags) -> Result<Self> {
        let mut backfill = false;
        let mut memory: Option<u128> = None;
        let mut since: Option<u64> = None;
        let mut all = false;

        // Consume sub-action positional + the action-specific flags
        // we don't already pull through `FamilyFlags`.
        let mut i = 0;
        while i < args.len() {
            let a = args[i].as_str();
            match a {
                "--backfill" => backfill = true,
                "--memory-id" => {
                    i += 1;
                    let v = args
                        .get(i)
                        .ok_or_else(|| anyhow!("--memory-id expects a value"))?;
                    memory = Some(
                        v.parse::<u128>()
                            .map_err(|e| anyhow!("invalid --memory-id `{v}`: {e}"))?,
                    );
                }
                "--all" => all = true,
                other => return Err(anyhow!("unknown extract flag `{other}`")),
            }
            i += 1;
        }

        // `--since` rides on the existing family-flag bag because
        // `audit` already declares it; reuse the parsed string.
        if let Some(s) = family.since.as_deref() {
            since = Some(
                s.parse::<u64>()
                    .map_err(|e| anyhow!("invalid --since `{s}`: {e}"))?,
            );
        }

        if !backfill {
            return Err(anyhow!(
                "extract requires --backfill (no other sub-actions yet)",
            ));
        }
        let present = [memory.is_some(), since.is_some(), all]
            .into_iter()
            .filter(|b| *b)
            .count();
        if present == 0 {
            return Err(anyhow!(
                "extract --backfill requires exactly one of --memory-id <id>, --since <unix_nanos>, --all",
            ));
        }
        if present > 1 {
            return Err(anyhow!(
                "extract --backfill: --memory-id, --since, --all are mutually exclusive",
            ));
        }

        let kind = if let Some(m) = memory {
            BackfillKind::Memory(m)
        } else if let Some(s) = since {
            BackfillKind::Since(s)
        } else {
            BackfillKind::All
        };
        Ok(Self::Backfill(kind))
    }
}

pub fn run(server: &str, action: &ExtractAction, output: OutputFormat) -> Result<String> {
    match action {
        ExtractAction::Backfill(kind) => backfill::run(server, *kind, output),
    }
}
