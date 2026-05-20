//! Top-level argv dispatch — picks between one-shot exec, REPL,
//! and helper subcommands (config / agent / generate-completion).

use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{CommandFactory, Parser};

use crate::cli::agent;
use crate::cli::config::{Config, MigrationNote};
use crate::commands;
use crate::commands::render_ctx;
use crate::connection;
use crate::parser::{
    parse_server, AgentCommand, Cli, Command, ConfigCommand, OutputFormatArg, TxnCommand,
};
use crate::repl;
use crate::session::Session;

/// Entry point used by both `main` and integration tests.
pub async fn dispatch_argv(argv: Vec<String>) -> ExitCode {
    let cli = match Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            };
        }
    };

    // ── connectionless subcommands ─────────────────────────────────
    if let Some(Command::GenerateCompletion(args)) = &cli.subcommand {
        let mut cmd = Cli::command();
        clap_complete::generate(args.shell, &mut cmd, "brain", &mut std::io::stdout());
        return ExitCode::SUCCESS;
    }
    if let Some(Command::Config(c)) = cli.subcommand.clone() {
        return run_config(c);
    }
    if let Some(Command::Agent(a)) = cli.subcommand.clone() {
        return run_agent(
            a,
            cli.global.agent.as_deref(),
            cli.global.agent_id.as_deref(),
        );
    }

    // ── agent + settings resolution ────────────────────────────────
    let resolved = match agent::resolve(cli.global.agent.as_deref(), cli.global.agent_id.as_deref())
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    if let Some(note) = &resolved.migration {
        eprint_migration_note(note);
    }
    let agent_id = resolved.agent_id;
    let agent_source = resolved.source;

    // Load settings (defaults if file missing). Migration is
    // already-applied by the resolver above, so we don't surface it
    // again here.
    let settings_file = match Config::load_or_default() {
        Ok((c, _)) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let settings = settings_file.settings().clone();

    // Effective server: per-invocation flag > persisted setting > built-in default.
    let server_str = settings
        .server
        .clone()
        .unwrap_or_else(|| cli.global.server.clone());
    // If --server was explicitly passed (clap saw a non-default), the
    // flag wins. Detect that by comparing against the default sentinel
    // — `Cli::DEFAULT_SERVER` lives in command.rs; we re-derive by
    // checking the user typed `--server`. Clap doesn't tell us
    // directly, so the simple rule is "flag value if differs from
    // persisted, else persisted." Tied: doesn't matter.
    let effective_server = if cli.global.server != crate::parser::command::DEFAULT_SERVER {
        cli.global.server.clone()
    } else {
        server_str
    };
    let addr = match parse_server(&effective_server) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    let timeout = Duration::from_secs(cli.global.timeout);
    let is_repl = matches!(cli.subcommand, None | Some(Command::Shell));

    // Output format: per-invocation flag > persisted setting > `Auto`.
    // `Auto` resolves to table when stdout is a TTY, ndjson otherwise —
    // the resolution happens at render time so it picks up redirects.
    let output_format = cli
        .global
        .output
        .clone()
        .unwrap_or_else(|| match settings.output {
            Some(crate::cli::config::OutputPref::Json) => OutputFormatArg::Json,
            Some(crate::cli::config::OutputPref::Table) => OutputFormatArg::Table,
            None => {
                if is_repl {
                    OutputFormatArg::Table
                } else {
                    OutputFormatArg::Auto
                }
            }
        });

    let client = match connection::connect(addr, agent_id, timeout).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("connect failed: {e}");
            return ExitCode::from(1);
        }
    };

    if is_repl {
        let session = Session::from_settings(addr, output_format, &settings);
        if let Err(e) = repl::run_loop(session, client, agent_id, agent_source).await {
            eprintln!("repl error: {e}");
            return ExitCode::from(1);
        }
        return ExitCode::SUCCESS;
    }

    let mut session = Session::from_settings(addr, output_format, &settings);
    let cmd = cli.subcommand.expect("invariant: is_repl handled above");

    let started = Instant::now();
    let res: Result<(String, Box<dyn brain_explore::Render>), brain_sdk_rust::ClientError> =
        match cmd {
            Command::Encode(a) => commands::encode::run(&client, &mut session, a)
                .await
                .map(|r| ("encode".to_string(), r)),
            Command::Recall(a) => commands::recall::run(&client, &mut session, a)
                .await
                .map(|r| ("recall".to_string(), r)),
            Command::Forget(a) => commands::forget::run(&client, &mut session, a)
                .await
                .map(|r| ("forget".to_string(), r)),
            Command::Link(a) => commands::link::run(&client, &mut session, a)
                .await
                .map(|r| ("link".to_string(), r)),
            Command::Unlink(a) => commands::unlink::run(&client, &mut session, a)
                .await
                .map(|r| ("unlink".to_string(), r)),
            Command::Plan(a) => commands::plan::run(&client, &mut session, a)
                .await
                .map(|r| ("plan".to_string(), r)),
            Command::Reason(a) => commands::reason::run(&client, &mut session, a)
                .await
                .map(|r| ("reason".to_string(), r)),
            Command::Subscribe(a) => commands::subscribe::run(&client, &mut session, a)
                .await
                .map(|r| ("subscribe".to_string(), r)),
            Command::Txn(t) => {
                let op = match &t {
                    TxnCommand::Begin => "txn_begin",
                    TxnCommand::Commit { .. } => "txn_commit",
                    TxnCommand::Abort { .. } => "txn_abort",
                };
                commands::txn::run(&client, &mut session, t)
                    .await
                    .map(|r| (op.to_string(), r))
            }
            Command::Entity(sub) => {
                let op = commands::entity::op_name(&sub).to_string();
                commands::entity::run(&client, &mut session, sub)
                    .await
                    .map(|r| (op, r))
            }
            Command::Statement(sub) => {
                let op = commands::statement::op_name(&sub).to_string();
                commands::statement::run(&client, &mut session, sub)
                    .await
                    .map(|r| (op, r))
            }
            Command::Relation(sub) => {
                let op = commands::relation::op_name(&sub).to_string();
                commands::relation::run(&client, &mut session, sub)
                    .await
                    .map(|r| (op, r))
            }
            Command::Mention(sub) => {
                let op = commands::mention::op_name(&sub).to_string();
                commands::mention::run(&client, &mut session, sub)
                    .await
                    .map(|r| (op, r))
            }
            Command::Extract(sub) => {
                let op = commands::extract::op_name(&sub).to_string();
                commands::extract::run(&client, &mut session, sub)
                    .await
                    .map(|r| (op, r))
            }
            Command::Config(_)
            | Command::Agent(_)
            | Command::Shell
            | Command::GenerateCompletion(_) => {
                unreachable!("filtered above")
            }
        };
    let elapsed = started.elapsed();
    let elapsed_ms = if session.timing {
        Some(elapsed.as_millis())
    } else {
        None
    };

    match res {
        Ok((_op, body)) => {
            let mut stdout = std::io::stdout();
            let ctx = render_ctx(
                session.output.clone(),
                cli.global.color,
                cli.global.hyperlinks,
            );
            if let Err(e) = brain_explore::dispatch(body.as_ref(), &ctx, &mut stdout) {
                eprintln!("output error: {e}");
                return ExitCode::from(1);
            }
            // Per-op timing footer — only meaningful for human formats; the
            // structured outputs would break under a stray trailer line.
            if let Some(ms) = elapsed_ms {
                if matches!(
                    session.output,
                    OutputFormatArg::Auto | OutputFormatArg::Table | OutputFormatArg::Wide
                ) {
                    use std::io::Write as _;
                    let _ = writeln!(stdout, "({ms} ms)");
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

// ---------------------------------------------------------------------------
// brain config <subcommand>
// ---------------------------------------------------------------------------

fn run_config(cmd: ConfigCommand) -> ExitCode {
    let (mut config, note) = match Config::load_or_default() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    if let Some(n) = &note {
        eprint_migration_note(n);
    }
    match cmd {
        ConfigCommand::List => {
            for (k, v) in config.list() {
                println!("{k:<15} {v}");
            }
            ExitCode::SUCCESS
        }
        ConfigCommand::Get { key } => match config.get(&key) {
            Ok(v) => {
                println!("{v}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        },
        ConfigCommand::Set { key, value } => match config.set(&key, &value) {
            Ok(()) => match config.save() {
                Ok(()) => {
                    println!("{key} = {value}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::from(1)
                }
            },
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        },
        ConfigCommand::Path => {
            println!("{}", config.path.display());
            ExitCode::SUCCESS
        }
        ConfigCommand::Edit => {
            let editor = std::env::var("VISUAL")
                .or_else(|_| std::env::var("EDITOR"))
                .unwrap_or_else(|_| "vi".to_string());
            // Ensure the file exists so $EDITOR has something to open
            // (mirror behaviour of `git config --edit` on first run).
            if !config.path.exists() {
                if let Err(e) = config.save() {
                    eprintln!("error: {e}");
                    return ExitCode::from(1);
                }
            }
            let status = std::process::Command::new(editor)
                .arg(&config.path)
                .status();
            match status {
                Ok(s) if s.success() => ExitCode::SUCCESS,
                Ok(s) => ExitCode::from(s.code().unwrap_or(1) as u8),
                Err(e) => {
                    eprintln!("error launching editor: {e}");
                    ExitCode::from(1)
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// brain agent <subcommand>
// ---------------------------------------------------------------------------

fn run_agent(cmd: AgentCommand, agent_flag: Option<&str>, agent_id_flag: Option<&str>) -> ExitCode {
    let (mut config, note) = match Config::load_or_default() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    if let Some(n) = &note {
        eprint_migration_note(n);
    }
    match cmd {
        AgentCommand::List => {
            let bound_name = resolved_bound_name(agent_flag, agent_id_flag);
            if config.agents().is_empty() {
                println!("(no named agents — `brain agent create <name>` to add one)");
                return ExitCode::SUCCESS;
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
                    name, entry.id, entry.created_at, entry.note
                );
            }
            ExitCode::SUCCESS
        }
        AgentCommand::Show { name } => {
            let resolved_name = name.or_else(|| resolved_bound_name(agent_flag, agent_id_flag));
            match resolved_name {
                Some(n) => match config.get_agent(&n) {
                    Ok(e) => {
                        println!("name       = {n}");
                        println!("id         = {}", e.id);
                        println!("created_at = {}", e.created_at);
                        if !e.note.is_empty() {
                            println!("note       = {}", e.note);
                        }
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("error: {e}");
                        ExitCode::from(2)
                    }
                },
                None => {
                    println!("(no named agent — this invocation would mint an ephemeral one)");
                    ExitCode::SUCCESS
                }
            }
        }
        AgentCommand::Create { name, note } => {
            match config.create_agent(&name, note.as_deref().unwrap_or("")) {
                Ok(entry) => {
                    let id = entry.id.clone();
                    match config.save() {
                        Ok(()) => {
                            println!("created agent '{name}' ({id})");
                            ExitCode::SUCCESS
                        }
                        Err(e) => {
                            eprintln!("error: {e}");
                            ExitCode::from(1)
                        }
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::from(2)
                }
            }
        }
        AgentCommand::Rename { old, new } => match config.rename_agent(&old, &new) {
            Ok(()) => match config.save() {
                Ok(()) => {
                    println!("renamed '{old}' → '{new}'");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::from(1)
                }
            },
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        },
        AgentCommand::Delete { name } => {
            let bound_name = resolved_bound_name(agent_flag, agent_id_flag);
            if bound_name.as_deref() == Some(name.as_str()) {
                eprintln!(
                    "error: refusing to delete '{name}' — the current invocation is bound to it"
                );
                return ExitCode::from(2);
            }
            match config.delete_agent(&name) {
                Ok(_) => match config.save() {
                    Ok(()) => {
                        println!("deleted agent '{name}'");
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("error: {e}");
                        ExitCode::from(1)
                    }
                },
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::from(2)
                }
            }
        }
        AgentCommand::Import { name, id, note } => {
            match config.import_agent(&name, &id, note.as_deref().unwrap_or("")) {
                Ok(_) => match config.save() {
                    Ok(()) => {
                        println!("imported agent '{name}' ({id})");
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("error: {e}");
                        ExitCode::from(1)
                    }
                },
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::from(2)
                }
            }
        }
    }
}

/// Name of the agent this invocation would bind to — for the `*`
/// marker in `agent list`. Returns `None` when the binding is
/// ephemeral or by raw id (no name to compare against).
fn resolved_bound_name(agent_flag: Option<&str>, agent_id_flag: Option<&str>) -> Option<String> {
    if let Some(n) = agent_flag {
        return Some(n.to_owned());
    }
    if agent_id_flag.is_some() {
        return None;
    }
    if let Ok(s) = std::env::var(agent::ENV_VAR_NAME) {
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

fn eprint_migration_note(note: &MigrationNote) {
    eprintln!(
        "note: migrated legacy config.toml to named-agent schema (entry '{name}'); backup at {path}",
        name = note.migrated_name,
        path = note.backup_path.display(),
    );
}
