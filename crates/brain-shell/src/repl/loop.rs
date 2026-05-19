//! REPL event loop.

use std::time::{Duration, Instant};

use brain_core::AgentId;
use brain_sdk_rust::{Client, ClientError};
use clap::Parser;
use rustyline::error::ReadlineError;

use crate::cli::agent_id::AgentIdSource;
use crate::cli::config::Config;
use crate::commands;
use crate::connection;
use crate::output::{write_rendered, OutputFormatArg};
use crate::parser::tokenize::tokenize_line;
use crate::parser::{format_txn_id, parse_server, Cli, Command, TxnCommand};
use crate::repl::editor;
use crate::repl::help;
use crate::session::Session;

/// Run the REPL until the user exits. Returns `Ok(())` on clean
/// shutdown.
pub async fn run(
    mut session: Session,
    mut client: Client,
    agent_id: AgentId,
    agent_source: AgentIdSource,
) -> anyhow::Result<()> {
    let (mut ed, history_path) = editor::build()?;

    println!(
        "brain shell — connected to {} as {}{}.",
        session.server,
        agent_id.0,
        source_suffix(&agent_source),
    );
    if let Some(note) = source_note(&agent_source) {
        println!("{note}");
    }
    println!("Type `help` for commands, `quit` to exit.");

    loop {
        let prompt = session.prompt();
        let line = match ed.readline(&prompt) {
            Ok(l) => l,
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => {
                println!();
                break;
            }
            Err(e) => {
                eprintln!("readline error: {e}");
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(action) = parse_meta(trimmed) {
            if handle_meta(action, &mut session, &mut client, agent_id, &agent_source).await {
                break;
            }
            continue;
        }

        let tokens = match tokenize_line(trimmed) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("parse error: {e}");
                continue;
            }
        };
        if tokens.is_empty() {
            continue;
        }
        let argv: Vec<String> = std::iter::once("brain".to_string()).chain(tokens).collect();

        match Cli::try_parse_from(&argv) {
            Ok(cli) => match cli.subcommand {
                None | Some(Command::Shell) => {
                    eprintln!("(already in the shell)");
                }
                Some(Command::GenerateCompletion(_)) => {
                    eprintln!("generate-completion is only available as a one-shot subcommand");
                }
                Some(cmd) => {
                    run_one(&client, &mut session, cmd).await;
                }
            },
            Err(e) => {
                eprint!("{e}");
            }
        }
    }

    editor::save(&mut ed, &history_path);
    Ok(())
}

async fn run_one(client: &Client, session: &mut Session, cmd: Command) {
    let started = Instant::now();
    let inherited_active_txn = inherits_active_txn(&cmd);
    let result: Result<(String, Box<dyn crate::output::Render>), ClientError> = match cmd {
        Command::Encode(a) => commands::encode::run(client, session, a).await.map(|r| ("encode".to_string(), r)),
        Command::Recall(a) => commands::recall::run(client, session, a).await.map(|r| ("recall".to_string(), r)),
        Command::Forget(a) => commands::forget::run(client, session, a).await.map(|r| ("forget".to_string(), r)),
        Command::Link(a) => commands::link::run(client, session, a).await.map(|r| ("link".to_string(), r)),
        Command::Unlink(a) => commands::unlink::run(client, session, a).await.map(|r| ("unlink".to_string(), r)),
        Command::Plan(a) => commands::plan::run(client, session, a).await.map(|r| ("plan".to_string(), r)),
        Command::Reason(a) => commands::reason::run(client, session, a).await.map(|r| ("reason".to_string(), r)),
        Command::Subscribe(a) => commands::subscribe::run(client, session, a).await.map(|r| ("subscribe".to_string(), r)),
        Command::Txn(t) => {
            let op = match &t {
                TxnCommand::Begin => "txn_begin",
                TxnCommand::Commit { .. } => "txn_commit",
                TxnCommand::Abort { .. } => "txn_abort",
            };
            commands::txn::run(client, session, t).await.map(|r| (op.to_string(), r))
        }
        Command::Config(_) | Command::Agent(_) => {
            // `\config …` / `\agent …` are handled by parse_meta; the
            // clap path only fires if someone typed the bare verbs
            // inside the REPL. Tell them to use the meta form.
            eprintln!(
                "use `\\config <subcommand>` or `\\agent <subcommand>` inside the REPL"
            );
            return;
        }
        Command::Shell | Command::GenerateCompletion(_) => unreachable!("filtered above"),
    };
    let elapsed = started.elapsed();
    let elapsed_ms = if session.timing { Some(elapsed.as_millis()) } else { None };

    match result {
        Ok((op, body)) => {
            let mut stdout = std::io::stdout();
            if let Err(e) = write_rendered(&mut stdout, &op, body.as_ref(), session.output, elapsed_ms)
            {
                eprintln!("output error: {e}");
            }
        }
        Err(e) => {
            if inherited_active_txn
                && session.active_txn.is_some()
                && commands::is_txn_terminal(&e)
            {
                let stale = session.active_txn.take();
                if let Some(bytes) = stale {
                    eprintln!(
                        "note: server reported the active transaction is no longer usable; \
                         session no longer attached to txn {}",
                        format_txn_id(&bytes),
                    );
                }
            }
            eprintln!("error: {e}");
        }
    }
}

fn inherits_active_txn(cmd: &Command) -> bool {
    match cmd {
        Command::Encode(a) => a.txn.is_none(),
        Command::Recall(a) => a.txn.is_none(),
        Command::Link(a) => a.txn.is_none(),
        Command::Unlink(a) => a.txn.is_none(),
        Command::Forget(_) | Command::Plan(_) | Command::Reason(_) => true,
        Command::Subscribe(_)
        | Command::Txn(_)
        | Command::Config(_)
        | Command::Agent(_)
        | Command::Shell
        | Command::GenerateCompletion(_) => false,
    }
}

// ─── meta commands ──────────────────────────────────────────────

#[derive(Debug)]
enum Meta {
    Quit,
    Help(Option<String>),
    SetOutput(OutputFormatArg),
    SetContext(u64),
    UnsetTxn,
    Timing(bool),
    Connect(String),
    Agent,
    AgentSub(AgentSub),
    ConfigSub(ConfigSub),
    Unknown(String),
}

#[derive(Debug)]
enum AgentSub {
    List,
    Show(Option<String>),
    // `name` is reserved for future live-rebind support; today the
    // handler refuses with a "quit + relaunch" message and doesn't
    // need the value.
    Use(#[allow(dead_code)] String),
    Create { name: String, note: Option<String> },
}

#[derive(Debug)]
enum ConfigSub {
    List,
    Get(String),
    Set { key: String, value: String },
    Path,
    Edit,
}

fn parse_meta(line: &str) -> Option<Meta> {
    let lower = line.trim();
    if lower == "quit" || lower == "exit" || lower == "\\q" {
        return Some(Meta::Quit);
    }
    if lower == "?" || lower == "\\?" || lower == "help" {
        return Some(Meta::Help(None));
    }
    if let Some(rest) = lower
        .strip_prefix("help ")
        .or_else(|| lower.strip_prefix("? "))
        .or_else(|| lower.strip_prefix("\\? "))
    {
        return Some(Meta::Help(Some(rest.trim().to_string())));
    }

    if let Some(rest) = lower.strip_prefix("\\set ") {
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if parts.len() == 2 && parts[0] == "output" {
            let v = parts[1].to_ascii_lowercase();
            return match v.as_str() {
                "json" => Some(Meta::SetOutput(OutputFormatArg::Json)),
                "table" => Some(Meta::SetOutput(OutputFormatArg::Table)),
                _ => Some(Meta::Unknown(v)),
            };
        }
        if parts.len() == 2 && parts[0] == "context" {
            if let Ok(n) = parts[1].parse::<u64>() {
                return Some(Meta::SetContext(n));
            }
        }
        return Some(Meta::Unknown(rest.to_string()));
    }

    if lower == "\\unset txn" {
        return Some(Meta::UnsetTxn);
    }

    // `\agent` and `\agent <subcommand …>`
    if lower == "\\agent" {
        return Some(Meta::Agent);
    }
    if let Some(rest) = lower.strip_prefix("\\agent ") {
        return Some(parse_agent_sub(rest));
    }

    // `\config <subcommand …>`
    if let Some(rest) = lower.strip_prefix("\\config ") {
        return Some(parse_config_sub(rest));
    }
    if lower == "\\config" {
        return Some(Meta::Unknown("\\config requires a subcommand".into()));
    }

    if let Some(rest) = lower.strip_prefix("\\timing ") {
        return match rest.trim() {
            "on" => Some(Meta::Timing(true)),
            "off" => Some(Meta::Timing(false)),
            other => Some(Meta::Unknown(other.to_string())),
        };
    }

    if let Some(rest) = lower.strip_prefix("\\connect ") {
        return Some(Meta::Connect(rest.trim().to_string()));
    }

    if lower.starts_with('\\') {
        return Some(Meta::Unknown(lower.to_string()));
    }

    None
}

fn parse_agent_sub(rest: &str) -> Meta {
    let parts: Vec<&str> = rest.split_whitespace().collect();
    match parts.as_slice() {
        ["list"] => Meta::AgentSub(AgentSub::List),
        ["show"] => Meta::AgentSub(AgentSub::Show(None)),
        ["show", name] => Meta::AgentSub(AgentSub::Show(Some((*name).to_string()))),
        ["use", name] => Meta::AgentSub(AgentSub::Use((*name).to_string())),
        ["create", name] => Meta::AgentSub(AgentSub::Create {
            name: (*name).to_string(),
            note: None,
        }),
        ["create", name, "--note", note @ ..] => Meta::AgentSub(AgentSub::Create {
            name: (*name).to_string(),
            note: Some(note.join(" ")),
        }),
        _ => Meta::Unknown(format!("\\agent {}", rest)),
    }
}

fn parse_config_sub(rest: &str) -> Meta {
    let parts: Vec<&str> = rest.split_whitespace().collect();
    match parts.as_slice() {
        ["list"] => Meta::ConfigSub(ConfigSub::List),
        ["path"] => Meta::ConfigSub(ConfigSub::Path),
        ["edit"] => Meta::ConfigSub(ConfigSub::Edit),
        ["get", key] => Meta::ConfigSub(ConfigSub::Get((*key).to_string())),
        ["set", key, value] => Meta::ConfigSub(ConfigSub::Set {
            key: (*key).to_string(),
            value: (*value).to_string(),
        }),
        _ => Meta::Unknown(format!("\\config {}", rest)),
    }
}

/// Returns `true` when the loop should exit.
async fn handle_meta(
    meta: Meta,
    session: &mut Session,
    client: &mut Client,
    agent_id: AgentId,
    agent_source: &AgentIdSource,
) -> bool {
    match meta {
        Meta::Quit => true,
        Meta::Help(v) => {
            print!("{}", help::lookup(v.as_deref()));
            false
        }
        Meta::SetOutput(o) => {
            session.output = o;
            println!("output = {:?}", o);
            false
        }
        Meta::SetContext(n) => {
            session.sticky_context = Some(n);
            println!("sticky context = {n}");
            false
        }
        Meta::UnsetTxn => {
            session.active_txn = None;
            println!("active txn cleared");
            false
        }
        Meta::Timing(on) => {
            session.timing = on;
            println!("timing = {on}");
            false
        }
        Meta::Connect(addr_str) => {
            match parse_server(&addr_str) {
                Ok(addr) => match connection::connect(addr, agent_id, Duration::from_secs(30)).await {
                    Ok(new_client) => {
                        *client = new_client;
                        session.server = addr;
                        println!("connected to {addr}");
                    }
                    Err(e) => eprintln!("connect failed: {e}"),
                },
                Err(e) => eprintln!("{e}"),
            }
            false
        }
        Meta::Agent => {
            print_agent_info(session.output, agent_id, agent_source);
            false
        }
        Meta::AgentSub(sub) => {
            handle_agent_sub(sub, session, agent_source);
            false
        }
        Meta::ConfigSub(sub) => {
            handle_config_sub(sub, session);
            false
        }
        Meta::Unknown(s) => {
            eprintln!("unknown meta command: {s}");
            false
        }
    }
}

fn handle_agent_sub(sub: AgentSub, session: &Session, agent_source: &AgentIdSource) {
    let bound_name = source_name(agent_source);
    let (mut config, _note) = match Config::load_or_default() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return;
        }
    };
    match sub {
        AgentSub::List => {
            if config.agents().is_empty() {
                println!("(no named agents — `\\agent create <name>` to add one)");
                return;
            }
            println!("{:<2} {:<16} {:<36} {:<20} NOTE", "", "NAME", "ID", "CREATED");
            for (name, entry) in config.agents() {
                let marker = if Some(name.as_str()) == bound_name.as_deref() { "*" } else { " " };
                println!(
                    "{marker:<2} {:<16} {:<36} {:<20} {}",
                    name, entry.id, entry.created_at, entry.note,
                );
            }
        }
        AgentSub::Show(name) => {
            let resolved = name.or(bound_name);
            match resolved {
                Some(n) => match config.get_agent(&n) {
                    Ok(e) => {
                        println!("name       = {n}");
                        println!("id         = {}", e.id);
                        println!("created_at = {}", e.created_at);
                        if !e.note.is_empty() {
                            println!("note       = {}", e.note);
                        }
                    }
                    Err(e) => eprintln!("error: {e}"),
                },
                None => {
                    println!("(no named agent — this invocation is ephemeral)");
                }
            }
        }
        AgentSub::Use(_) => {
            // Rebind is intentionally tricky (it requires a fresh
            // connection with the new agent id), and the user has
            // session state (active_txn, recent_ids) that ought to
            // be reset. Refuse here with a clean message pointing
            // at `quit` + relaunch. Live rebinding is a future
            // refinement, not load-bearing for this PR.
            if session.active_txn.is_some() {
                eprintln!(
                    "error: active transaction prevents agent rebind — \
                     commit or abort first"
                );
                return;
            }
            eprintln!(
                "live agent rebind is not yet supported — quit and relaunch \
                 with `brain --agent <name>`"
            );
        }
        AgentSub::Create { name, note } => {
            match config.create_agent(&name, note.as_deref().unwrap_or("")) {
                Ok(e) => {
                    let id = e.id.clone();
                    if let Err(err) = config.save() {
                        eprintln!("error: {err}");
                        return;
                    }
                    println!("created agent '{name}' ({id})");
                }
                Err(e) => eprintln!("error: {e}"),
            }
        }
    }
}

fn handle_config_sub(sub: ConfigSub, session: &mut Session) {
    let (mut config, _note) = match Config::load_or_default() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return;
        }
    };
    match sub {
        ConfigSub::List => {
            for (k, v) in config.list() {
                println!("{k:<15} {v}");
            }
        }
        ConfigSub::Get(key) => match config.get(&key) {
            Ok(v) => println!("{v}"),
            Err(e) => eprintln!("error: {e}"),
        },
        ConfigSub::Set { key, value } => match config.set(&key, &value) {
            Ok(()) => match config.save() {
                Ok(()) => {
                    // Mirror into the live session so the change
                    // takes effect immediately. `server` is the
                    // exception — `\connect` is the live verb for
                    // that, so we just persist without disturbing
                    // the open connection.
                    apply_setting_to_session(&key, &value, session);
                    println!("{key} = {value}");
                }
                Err(e) => eprintln!("error: {e}"),
            },
            Err(e) => eprintln!("error: {e}"),
        },
        ConfigSub::Path => println!("{}", config.path.display()),
        ConfigSub::Edit => {
            let editor = std::env::var("VISUAL")
                .or_else(|_| std::env::var("EDITOR"))
                .unwrap_or_else(|_| "vi".to_string());
            if !config.path.exists() {
                if let Err(e) = config.save() {
                    eprintln!("error: {e}");
                    return;
                }
            }
            let status = std::process::Command::new(editor).arg(&config.path).status();
            match status {
                Ok(s) if s.success() => {}
                Ok(s) => eprintln!("editor exited with status {}", s.code().unwrap_or(-1)),
                Err(e) => eprintln!("error launching editor: {e}"),
            }
        }
    }
}

fn apply_setting_to_session(key: &str, value: &str, session: &mut Session) {
    match key {
        "output" => {
            session.output = match value {
                "json" => OutputFormatArg::Json,
                _ => OutputFormatArg::Table,
            };
        }
        "timing" => {
            session.timing = matches!(value, "true" | "on" | "1");
        }
        "sticky_context" => {
            if let Ok(n) = value.parse::<u64>() {
                session.sticky_context = Some(n);
            }
        }
        _ => {} // `server` and unknown keys: no live-session mirror.
    }
}

// ─── source-aware banner + \agent helpers ───────────────────────

fn source_suffix(s: &AgentIdSource) -> &'static str {
    match s {
        AgentIdSource::NamedFlag { .. } => " (via --agent)",
        AgentIdSource::IdFlag => " (via --agent-id)",
        AgentIdSource::NamedEnv { .. } => " (via BRAIN_AGENT)",
        AgentIdSource::IdEnv => " (via BRAIN_AGENT_ID)",
        AgentIdSource::Ephemeral => " (ephemeral)",
    }
}

fn source_note(s: &AgentIdSource) -> Option<String> {
    match s {
        AgentIdSource::Ephemeral => Some(
            "note: ephemeral session — memories you encode are visible only \
             until you quit. Use `brain agent create <name>` then \
             `brain --agent <name>` for persistence."
                .to_string(),
        ),
        _ => None,
    }
}

fn source_label(s: &AgentIdSource) -> String {
    match s {
        AgentIdSource::NamedFlag { name, file } => {
            format!("flag --agent {name} ({})", file.display())
        }
        AgentIdSource::IdFlag => "flag --agent-id".into(),
        AgentIdSource::NamedEnv { name, file } => {
            format!("env BRAIN_AGENT={name} ({})", file.display())
        }
        AgentIdSource::IdEnv => "env BRAIN_AGENT_ID".into(),
        AgentIdSource::Ephemeral => "ephemeral (no --agent / BRAIN_AGENT set)".into(),
    }
}

/// Name (if any) of the agent this session is currently bound to.
/// Used for the `*` marker in `\agent list` and for `\agent show`
/// fallback.
fn source_name(s: &AgentIdSource) -> Option<String> {
    match s {
        AgentIdSource::NamedFlag { name, .. } | AgentIdSource::NamedEnv { name, .. } => {
            Some(name.clone())
        }
        _ => None,
    }
}

/// Render `\agent` (bare) — current binding summary.
fn print_agent_info(output: OutputFormatArg, agent_id: AgentId, source: &AgentIdSource) {
    match output {
        OutputFormatArg::Table => {
            println!("agent_id = {}", agent_id.0);
            println!("source   = {}", source_label(source));
            if let Some(name) = source_name(source) {
                println!("name     = {name}");
            }
        }
        OutputFormatArg::Json => {
            let kind = match source {
                AgentIdSource::NamedFlag { .. } => "named-flag",
                AgentIdSource::IdFlag => "id-flag",
                AgentIdSource::NamedEnv { .. } => "named-env",
                AgentIdSource::IdEnv => "id-env",
                AgentIdSource::Ephemeral => "ephemeral",
            };
            let name = source_name(source);
            let body = serde_json::json!({
                "op": "agent",
                "result": {
                    "agent_id": agent_id.0.to_string(),
                    "source": kind,
                    "name": name,
                },
            });
            println!("{body}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ----- parse_meta dispatch -----------------------------------

    #[test]
    fn parse_meta_recognises_backslash_agent_bare() {
        assert!(matches!(parse_meta("\\agent"), Some(Meta::Agent)));
        assert!(matches!(parse_meta("  \\agent  "), Some(Meta::Agent)));
    }

    #[test]
    fn parse_meta_recognises_agent_subcommands() {
        assert!(matches!(
            parse_meta("\\agent list"),
            Some(Meta::AgentSub(AgentSub::List))
        ));
        match parse_meta("\\agent show foo") {
            Some(Meta::AgentSub(AgentSub::Show(Some(n)))) => assert_eq!(n, "foo"),
            other => panic!("got {other:?}"),
        }
        match parse_meta("\\agent show") {
            Some(Meta::AgentSub(AgentSub::Show(None))) => {}
            other => panic!("got {other:?}"),
        }
        match parse_meta("\\agent use work") {
            Some(Meta::AgentSub(AgentSub::Use(n))) => assert_eq!(n, "work"),
            other => panic!("got {other:?}"),
        }
        match parse_meta("\\agent create work") {
            Some(Meta::AgentSub(AgentSub::Create { name, note })) => {
                assert_eq!(name, "work");
                assert!(note.is_none());
            }
            other => panic!("got {other:?}"),
        }
        match parse_meta("\\agent create work --note prod notebook") {
            Some(Meta::AgentSub(AgentSub::Create { name, note })) => {
                assert_eq!(name, "work");
                assert_eq!(note.as_deref(), Some("prod notebook"));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn parse_meta_recognises_config_subcommands() {
        assert!(matches!(
            parse_meta("\\config list"),
            Some(Meta::ConfigSub(ConfigSub::List))
        ));
        assert!(matches!(
            parse_meta("\\config path"),
            Some(Meta::ConfigSub(ConfigSub::Path))
        ));
        match parse_meta("\\config get output") {
            Some(Meta::ConfigSub(ConfigSub::Get(k))) => assert_eq!(k, "output"),
            other => panic!("got {other:?}"),
        }
        match parse_meta("\\config set output json") {
            Some(Meta::ConfigSub(ConfigSub::Set { key, value })) => {
                assert_eq!(key, "output");
                assert_eq!(value, "json");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn parse_meta_bare_config_is_unknown() {
        match parse_meta("\\config") {
            Some(Meta::Unknown(s)) => assert!(s.contains("subcommand")),
            other => panic!("got {other:?}"),
        }
    }

    // ----- source helpers ----------------------------------------

    #[test]
    fn source_suffix_describes_each_variant() {
        assert_eq!(source_suffix(&AgentIdSource::IdFlag), " (via --agent-id)");
        assert_eq!(source_suffix(&AgentIdSource::IdEnv), " (via BRAIN_AGENT_ID)");
        assert_eq!(source_suffix(&AgentIdSource::Ephemeral), " (ephemeral)");
        let p = PathBuf::from("/x/y");
        assert_eq!(
            source_suffix(&AgentIdSource::NamedFlag {
                name: "work".into(),
                file: p.clone(),
            }),
            " (via --agent)"
        );
        assert_eq!(
            source_suffix(&AgentIdSource::NamedEnv {
                name: "work".into(),
                file: p
            }),
            " (via BRAIN_AGENT)"
        );
    }

    #[test]
    fn source_note_fires_only_for_ephemeral() {
        assert!(source_note(&AgentIdSource::IdFlag).is_none());
        assert!(source_note(&AgentIdSource::IdEnv).is_none());
        let p = PathBuf::from("/x/y");
        assert!(source_note(&AgentIdSource::NamedFlag {
            name: "w".into(),
            file: p.clone(),
        })
        .is_none());
        assert!(source_note(&AgentIdSource::Ephemeral)
            .unwrap()
            .contains("ephemeral"));
    }

    #[test]
    fn source_name_returns_named_variants_only() {
        let p = PathBuf::from("/x/y");
        assert_eq!(
            source_name(&AgentIdSource::NamedFlag {
                name: "work".into(),
                file: p.clone(),
            }),
            Some("work".to_string())
        );
        assert_eq!(
            source_name(&AgentIdSource::NamedEnv {
                name: "demo".into(),
                file: p,
            }),
            Some("demo".to_string())
        );
        assert!(source_name(&AgentIdSource::IdFlag).is_none());
        assert!(source_name(&AgentIdSource::IdEnv).is_none());
        assert!(source_name(&AgentIdSource::Ephemeral).is_none());
    }
}
