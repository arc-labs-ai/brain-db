//! `brain-cli config {get,reload,set}` — spec §14/06 §7;
//! sub-task 10.11. Only `get` is backed today.

pub mod get;
pub mod reload;
pub mod set;

use anyhow::{anyhow, Result};

use crate::cli::args::FamilyFlags;
use crate::cli::OutputFormat;
use crate::commands::worker::common;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigAction {
    Get { key: Option<String> },
    Reload,
    Set { key: String, value: String },
}

impl ConfigAction {
    pub fn parse(args: &[String], flags: &FamilyFlags) -> Result<Self> {
        let action = args
            .first()
            .ok_or_else(|| anyhow!("config requires a sub-action (get/reload/set)"))?;
        match action.as_str() {
            "get" => Ok(Self::Get {
                key: flags.key.clone(),
            }),
            "reload" => Ok(Self::Reload),
            "set" => {
                let key = flags
                    .key
                    .clone()
                    .ok_or_else(|| anyhow!("`config set` requires --key <dotted.path>"))?;
                let value = flags
                    .value
                    .clone()
                    .ok_or_else(|| anyhow!("`config set` requires --value <v>"))?;
                Ok(Self::Set { key, value })
            }
            other => Err(anyhow!("unknown config sub-action `{other}`")),
        }
    }
}

pub fn run(server: &str, action: &ConfigAction, output: OutputFormat) -> Result<String> {
    match action {
        ConfigAction::Get { key } => get::run(server, key.as_deref(), output),
        ConfigAction::Reload => reload::run(server),
        ConfigAction::Set { key, value } => set::run(server, key, value),
    }
}

// Re-export the shared 501 surfacer so siblings have a stable path.
pub(crate) fn surface_501(resp: &crate::http::HttpResponse, path: &str) -> Result<String> {
    common::surface_status(resp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str], flags: FamilyFlags) -> Result<ConfigAction> {
        ConfigAction::parse(
            &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &flags,
        )
    }

    #[test]
    fn parses_get_without_key() {
        assert_eq!(
            parse(&["get"], FamilyFlags::default()).unwrap(),
            ConfigAction::Get { key: None }
        );
    }

    #[test]
    fn parses_get_with_key() {
        let f = FamilyFlags {
            key: Some("server.listen_addr".into()),
            ..Default::default()
        };
        assert_eq!(
            parse(&["get"], f).unwrap(),
            ConfigAction::Get {
                key: Some("server.listen_addr".into()),
            }
        );
    }

    #[test]
    fn set_requires_key_and_value() {
        assert!(parse(&["set"], FamilyFlags::default()).is_err());
        let f = FamilyFlags {
            key: Some("x".into()),
            ..Default::default()
        };
        assert!(parse(&["set"], f).is_err());
    }
}
