//! REPL event loop.

use std::time::{Duration, Instant};

use brain_core::AgentId;
use brain_sdk_rust::{Client, ClientError};
use clap::Parser;
use rustyline::error::ReadlineError;

use crate::cli::agent::AgentIdSource;
use crate::cli::config::Config;
use crate::commands;
use crate::commands::render_ctx;
use crate::connection;
use crate::parser::tokenize::tokenize_line;
use crate::parser::{format_txn_id, parse_server, Cli, Command, OutputFormatArg, TxnCommand};
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
    let (mut ed, history_path) = editor::build(session.recent_ids.clone())?;

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
                Some(Command::Info) => {
                    // Mirror the `\info` meta — the bare verb is here so
                    // users who type `info` (no backslash) get the same
                    // diagnostic instead of a clap parse error.
                    let card =
                        commands::info::collect(&client, &session, agent_id, &agent_source).await;
                    let ctx = render_ctx(
                        cli.global
                            .output
                            .clone()
                            .unwrap_or_else(|| session.output.clone()),
                        cli.global.color,
                        cli.global.hyperlinks,
                    );
                    let mut stdout = std::io::stdout();
                    if let Err(e) = brain_explore::dispatch(&card, &ctx, &mut stdout) {
                        eprintln!("output error: {e}");
                    }
                }
                Some(cmd) => {
                    run_one(&client, &mut session, cmd, &cli.global).await;
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

async fn run_one(
    client: &Client,
    session: &mut Session,
    cmd: Command,
    globals: &crate::parser::GlobalOpts,
) {
    let started = Instant::now();
    let inherited_active_txn = inherits_active_txn(&cmd);
    let result: Result<(String, Box<dyn brain_explore::Render>), ClientError> = match cmd {
        Command::Encode(a) => commands::encode::run(client, session, a)
            .await
            .map(|r| ("encode".to_string(), r)),
        Command::Recall(a) => commands::recall::run(client, session, a)
            .await
            .map(|r| ("recall".to_string(), r)),
        Command::Forget(a) => commands::forget::run(client, session, a)
            .await
            .map(|r| ("forget".to_string(), r)),
        Command::Link(a) => commands::link::run(client, session, a)
            .await
            .map(|r| ("link".to_string(), r)),
        Command::Unlink(a) => commands::unlink::run(client, session, a)
            .await
            .map(|r| ("unlink".to_string(), r)),
        Command::Plan(a) => commands::plan::run(client, session, a)
            .await
            .map(|r| ("plan".to_string(), r)),
        Command::Reason(a) => commands::reason::run(client, session, a)
            .await
            .map(|r| ("reason".to_string(), r)),
        Command::Subscribe(a) => commands::subscribe::run(client, session, a)
            .await
            .map(|r| ("subscribe".to_string(), r)),
        Command::Txn(t) => {
            let op = match &t {
                TxnCommand::Begin => "txn_begin",
                TxnCommand::Commit { .. } => "txn_commit",
                TxnCommand::Abort { .. } => "txn_abort",
            };
            commands::txn::run(client, session, t)
                .await
                .map(|r| (op.to_string(), r))
        }
        Command::Entity(sub) => {
            let op = commands::entity::op_name(&sub).to_string();
            commands::entity::run(client, session, sub)
                .await
                .map(|r| (op, r))
        }
        Command::Statement(sub) => {
            let op = commands::statement::op_name(&sub).to_string();
            commands::statement::run(client, session, sub)
                .await
                .map(|r| (op, r))
        }
        Command::Relation(sub) => {
            let op = commands::relation::op_name(&sub).to_string();
            commands::relation::run(client, session, sub)
                .await
                .map(|r| (op, r))
        }
        Command::Mention(sub) => {
            let op = commands::mention::op_name(&sub).to_string();
            commands::mention::run(client, session, sub)
                .await
                .map(|r| (op, r))
        }
        Command::Extract(sub) => {
            let op = commands::extract::op_name(&sub).to_string();
            commands::extract::run(client, session, sub)
                .await
                .map(|r| (op, r))
        }
        Command::Config(_) | Command::Agent(_) => {
            // `\config …` / `\agent …` are handled by parse_meta; the
            // clap path only fires if someone typed the bare verbs
            // inside the REPL. Tell them to use the meta form.
            eprintln!("use `\\config <subcommand>` or `\\agent <subcommand>` inside the REPL");
            return;
        }
        Command::Shell | Command::Info | Command::GenerateCompletion(_) => {
            unreachable!("filtered above")
        }
    };
    let elapsed = started.elapsed();
    let elapsed_ms = if session.timing {
        Some(elapsed.as_millis())
    } else {
        None
    };

    match result {
        Ok((_op, body)) => {
            let mut stdout = std::io::stdout();
            // Per-line clap flags win over the session-resolved
            // default. `--output / -o` and `--color` / `--hyperlinks`
            // live on `GlobalOpts` (clap globals), so a line like
            // `encode "foo" -o wide` lands as `globals.output =
            // Some(Wide)` even though the rest of the REPL session
            // is at session.output. Falling back to session.output
            // when the per-line flag is absent preserves
            // `\output wide`-style session overrides.
            let output = globals
                .output
                .clone()
                .unwrap_or_else(|| session.output.clone());
            let ctx = render_ctx(output.clone(), globals.color, globals.hyperlinks);
            if let Err(e) = brain_explore::dispatch(body.as_ref(), &ctx, &mut stdout) {
                eprintln!("output error: {e}");
            }
            if let Some(ms) = elapsed_ms {
                // Footer follows the same per-line-override discipline
                // as the dispatch above — the timing tail is only
                // useful for human formats, and a per-line `-o json`
                // would otherwise corrupt structured output with the
                // stray "(N ms)" line.
                if matches!(
                    output,
                    OutputFormatArg::Auto | OutputFormatArg::Table | OutputFormatArg::Wide
                ) {
                    use std::io::Write as _;
                    let _ = writeln!(stdout, "({ms} ms)");
                }
            }
        }
        Err(e) => {
            if inherited_active_txn && session.active_txn.is_some() && commands::is_txn_terminal(&e)
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
        // Knowledge-layer browse + extract surfaces are read-only or
        // admin-flavored; none of them implicitly bind to the active
        // txn the way encode / link / unlink do.
        Command::Entity(_)
        | Command::Statement(_)
        | Command::Relation(_)
        | Command::Mention(_)
        | Command::Extract(_)
        | Command::Subscribe(_)
        | Command::Txn(_)
        | Command::Config(_)
        | Command::Agent(_)
        | Command::Shell
        | Command::Info
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
    Info,
    Unknown(String),
}

#[derive(Debug)]
enum AgentSub {
    List,
    Show(Option<String>),
    Use(String),
    Create { name: String, note: Option<String> },
    SetDefault(String),
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
                "auto" => Some(Meta::SetOutput(OutputFormatArg::Auto)),
                "json" => Some(Meta::SetOutput(OutputFormatArg::Json)),
                "table" => Some(Meta::SetOutput(OutputFormatArg::Table)),
                "wide" => Some(Meta::SetOutput(OutputFormatArg::Wide)),
                "ndjson" => Some(Meta::SetOutput(OutputFormatArg::Ndjson)),
                "yaml" => Some(Meta::SetOutput(OutputFormatArg::Yaml)),
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

    if lower == "\\info" {
        return Some(Meta::Info);
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
        ["set-default", name] => Meta::AgentSub(AgentSub::SetDefault((*name).to_string())),
        ["set-default"] => Meta::Unknown("\\agent set-default needs <name>".into()),
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
            let label = o.short_name();
            session.output = o;
            println!("output = {label}");
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
                Ok(addr) => {
                    // sticky_agent (set via `\agent use`) takes precedence over the
                    // process-bound agent_id so the rebind actually changes the
                    // wire identity on reconnect.
                    let effective_agent = session.sticky_agent.unwrap_or(agent_id);
                    match connection::connect(addr, effective_agent, Duration::from_secs(30)).await
                    {
                        Ok(new_client) => {
                            *client = new_client;
                            session.server = addr;
                            println!("connected to {addr} as {}", effective_agent.0);
                        }
                        Err(e) => eprintln!("connect failed: {e}"),
                    }
                }
                Err(e) => eprintln!("{e}"),
            }
            false
        }
        Meta::Agent => {
            print_agent_info(&session.output, agent_id, agent_source);
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
        Meta::Info => {
            let card = commands::info::collect(client, session, agent_id, agent_source).await;
            let ctx = render_ctx(
                session.output.clone(),
                crate::parser::ColorMode::Auto,
                crate::parser::HyperlinkMode::Auto,
            );
            let mut stdout = std::io::stdout();
            if let Err(e) = brain_explore::dispatch(&card, &ctx, &mut stdout) {
                eprintln!("output error: {e}");
            }
            false
        }
        Meta::Unknown(s) => {
            eprintln!("unknown meta command: {s}");
            false
        }
    }
}

fn handle_agent_sub(sub: AgentSub, session: &mut Session, agent_source: &AgentIdSource) {
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
            println!(
                "{:<2} {:<16} {:<36} {:<20} NOTE",
                "", "NAME", "ID", "CREATED"
            );
            for (name, entry) in config.agents() {
                let marker = if Some(name.as_str()) == bound_name.as_deref() {
                    "*"
                } else {
                    " "
                };
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
        AgentSub::Use(name) => {
            // Sticky-agent binding. Looks the name up in config, parses
            // the UUID, stashes it on the session. The reconnect is the
            // owner of the open Client and lives in the caller — here we
            // refuse if there's an open transaction (its handle is tied
            // to the current agent's connection), and otherwise stash
            // the new id so a follow-up `\connect` (or process restart)
            // picks it up.
            if session.active_txn.is_some() {
                eprintln!(
                    "error: active transaction prevents agent rebind — \
                     commit or abort first"
                );
                return;
            }
            let entry = match config.get_agent(&name) {
                Ok(e) => e.clone(),
                Err(e) => {
                    eprintln!("error: {e}");
                    return;
                }
            };
            match uuid::Uuid::parse_str(&entry.id) {
                Ok(uuid) => {
                    session.sticky_agent = Some(brain_core::AgentId(uuid));
                    // Persist the switch so the next bare `brain`
                    // session picks the same agent. Best-effort: if
                    // the file write fails we keep the in-memory
                    // sticky_agent so the live session still uses the
                    // new id; the user sees a note explaining the
                    // session-vs-disk divergence.
                    if let Err(e) = config.set_active(&name) {
                        eprintln!(
                            "note: could not persist active flag to config: {e}\n\
                             the switch is effective for this session only."
                        );
                    }
                    println!(
                        "sticky agent set to '{name}' ({uuid}); reconnect via `\\connect <host:port>` \
                         to bind the new id on the wire."
                    );
                }
                Err(e) => eprintln!("error: agent '{name}' has malformed uuid: {e}"),
            }
        }
        AgentSub::SetDefault(name) => match config.set_default(&name) {
            Ok(()) => println!("default agent → {name}"),
            Err(e) => eprintln!("error: {e}"),
        },
        AgentSub::Create { name, note } => {
            // Mirror the CLI's first-create-promotes invariant fix so
            // the in-shell create path also keeps the file valid.
            let promote = if config.agents().is_empty() {
                crate::cli::config::AgentPromotion::DefaultAndActive
            } else {
                crate::cli::config::AgentPromotion::None
            };
            match config.create_agent(&name, note.as_deref().unwrap_or(""), promote) {
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
            let status = std::process::Command::new(editor)
                .arg(&config.path)
                .status();
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
                "auto" => OutputFormatArg::Auto,
                "json" => OutputFormatArg::Json,
                "wide" => OutputFormatArg::Wide,
                "ndjson" => OutputFormatArg::Ndjson,
                "yaml" => OutputFormatArg::Yaml,
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

fn source_suffix(s: &AgentIdSource) -> String {
    match s {
        AgentIdSource::NamedFlag { .. } => " (via --agent)".into(),
        AgentIdSource::IdFlag => " (via --agent-id)".into(),
        AgentIdSource::NamedEnv { .. } => " (via BRAIN_AGENT)".into(),
        AgentIdSource::IdEnv => " (via BRAIN_AGENT_ID)".into(),
        AgentIdSource::ActiveFromConfig { name, .. } => {
            format!(" (config: active = {name})")
        }
        AgentIdSource::DefaultFromConfig { name, .. } => {
            format!(" (config: default = {name})")
        }
        AgentIdSource::AutoMinted { name, .. } => {
            format!(" (auto-minted as {name})")
        }
        AgentIdSource::Ephemeral => " (ephemeral)".into(),
    }
}

fn source_note(s: &AgentIdSource) -> Option<String> {
    match s {
        AgentIdSource::AutoMinted { name, file } => Some(format!(
            "note: first run — minted and persisted agent '{name}' at {}. \
             Use `\\agent create <name>` to add another, and \
             `\\agent use <name>` to switch.",
            file.display()
        )),
        AgentIdSource::Ephemeral => Some(
            "note: ephemeral session — no config file available so the \
             minted agent is in-memory only. Memories you encode are \
             visible only until you quit."
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
        AgentIdSource::ActiveFromConfig { name, file } => {
            format!("config active = {name} ({})", file.display())
        }
        AgentIdSource::DefaultFromConfig { name, file } => {
            format!("config default = {name} ({})", file.display())
        }
        AgentIdSource::AutoMinted { name, file } => {
            format!("auto-minted {name} ({})", file.display())
        }
        AgentIdSource::Ephemeral => "ephemeral (no config file path available)".into(),
    }
}

/// Name (if any) of the agent this session is currently bound to.
/// Used for the `*` marker in `\agent list` and for `\agent show`
/// fallback.
fn source_name(s: &AgentIdSource) -> Option<String> {
    match s {
        AgentIdSource::NamedFlag { name, .. }
        | AgentIdSource::NamedEnv { name, .. }
        | AgentIdSource::ActiveFromConfig { name, .. }
        | AgentIdSource::DefaultFromConfig { name, .. }
        | AgentIdSource::AutoMinted { name, .. } => Some(name.clone()),
        _ => None,
    }
}

/// Render `\agent` (bare) — current binding summary.
fn print_agent_info(output: &OutputFormatArg, agent_id: AgentId, source: &AgentIdSource) {
    let human = matches!(
        output,
        OutputFormatArg::Auto | OutputFormatArg::Table | OutputFormatArg::Wide
    );
    if human {
        println!("agent_id = {}", agent_id.0);
        println!("source   = {}", source_label(source));
        if let Some(name) = source_name(source) {
            println!("name     = {name}");
        }
        return;
    }
    let kind = match source {
        AgentIdSource::NamedFlag { .. } => "named-flag",
        AgentIdSource::IdFlag => "id-flag",
        AgentIdSource::NamedEnv { .. } => "named-env",
        AgentIdSource::IdEnv => "id-env",
        AgentIdSource::ActiveFromConfig { .. } => "active",
        AgentIdSource::DefaultFromConfig { .. } => "default",
        AgentIdSource::AutoMinted { .. } => "auto-minted",
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
        assert_eq!(
            source_suffix(&AgentIdSource::IdEnv),
            " (via BRAIN_AGENT_ID)"
        );
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
    fn source_note_fires_for_ephemeral_and_automint() {
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
        assert!(source_note(&AgentIdSource::AutoMinted {
            name: "agent-deadbeef".into(),
            file: p,
        })
        .unwrap()
        .contains("first run"));
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
