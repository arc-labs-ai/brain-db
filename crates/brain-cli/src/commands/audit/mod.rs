//! `brain-cli audit {query,export}` — spec §14/06 §8;
//! sub-task 10.11. Both deferred (no audit-log primitive yet).

pub mod export;
pub mod query;

use anyhow::{anyhow, Result};

use crate::cli::args::FamilyFlags;
use crate::cli::OutputFormat;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditAction {
    Query {
        since: Option<String>,
        until: Option<String>,
        agent: Option<String>,
    },
    Export {
        output_path: Option<String>,
    },
}

impl AuditAction {
    pub fn parse(args: &[String], flags: &FamilyFlags) -> Result<Self> {
        let action = args
            .first()
            .ok_or_else(|| anyhow!("audit requires a sub-action (query/export)"))?;
        match action.as_str() {
            "query" => Ok(Self::Query {
                since: flags.since.clone(),
                until: flags.until.clone(),
                agent: flags.agent.clone(),
            }),
            "export" => {
                // Spec syntax: `audit export --output PATH`. We
                // reuse `--value` as the destination because the
                // global `--output` already means render-format. v2
                // will disambiguate when the action is wired.
                Ok(Self::Export {
                    output_path: flags.value.clone(),
                })
            }
            other => Err(anyhow!("unknown audit sub-action `{other}`")),
        }
    }
}

pub fn run(server: &str, action: &AuditAction, _output: OutputFormat) -> Result<String> {
    match action {
        AuditAction::Query {
            since,
            until,
            agent,
        } => query::run(server, since.as_deref(), until.as_deref(), agent.as_deref()),
        AuditAction::Export { output_path } => export::run(server, output_path.as_deref()),
    }
}
