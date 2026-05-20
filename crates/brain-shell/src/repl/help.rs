//! In-REPL help. Builds typed [`HelpTopLevel`] / [`HelpVerb`] /
//! [`HelpUnknown`] payloads for brain-explore to render — the data
//! lives here, the layout lives in brain-explore so the shell and CLI
//! pick up the same visual language.

use brain_explore::{HelpItem, HelpSection, HelpTopLevel, HelpUnknown, HelpVerb, Render};

/// Look up help for `verb`. Returns a boxed [`Render`] payload because
/// the three concrete types (top-level, per-verb, unknown) all need to
/// flow through the same dispatcher and a `Box<dyn Render>` is the
/// trait-object form that lets the caller pick a single path.
#[must_use]
pub fn lookup(verb: Option<&str>) -> Box<dyn Render> {
    match verb.map(str::to_ascii_lowercase).as_deref() {
        None | Some("") | Some("help") => Box::new(top_level()),
        Some("encode") => Box::new(help_encode()),
        Some("recall") => Box::new(help_recall()),
        Some("plan") => Box::new(help_plan()),
        Some("reason") => Box::new(help_reason()),
        Some("forget") => Box::new(help_forget()),
        Some("link") => Box::new(help_link()),
        Some("unlink") => Box::new(help_unlink()),
        Some("txn") => Box::new(help_txn()),
        Some("subscribe") => Box::new(help_subscribe()),
        Some("meta") | Some("\\") => Box::new(help_meta()),
        Some(other) => Box::new(HelpUnknown {
            verb: other.to_string(),
        }),
    }
}

// ── top-level ───────────────────────────────────────────────────────

fn top_level() -> HelpTopLevel {
    HelpTopLevel {
        sections: vec![
            HelpSection {
                title: "COGNITIVE VERBS".into(),
                note: None,
                items: vec![
                    item(
                        "encode",
                        "<TEXT> [--context N] [--kind K] [--salience F]",
                        "write a memory",
                    ),
                    item(
                        "recall",
                        "<QUERY> [--top-k N] [--include-text]",
                        "find similar memories",
                    ),
                    item("plan", "<FROM> <TO>", "plan a path"),
                    item("reason", "<OBS> [--depth N]", "derive inferences"),
                    item("forget", "<ID> [--mode soft|hard]", "tombstone a memory"),
                    item("link", "<SRC> <KIND> <TGT>", "add an explicit edge"),
                    item("unlink", "<SRC> <KIND> <TGT>", "remove an edge"),
                    item("txn", "begin | commit <ID> | abort <ID>", "transactions"),
                    item(
                        "subscribe",
                        "[--context N] [--kind K] [--collect N]",
                        "live event stream",
                    ),
                ],
            },
            HelpSection {
                title: "KNOWLEDGE BROWSING".into(),
                note: None,
                items: vec![
                    item("entity", "list | show <id|name> | neighbors <id>", ""),
                    item("statement", "list | show <id>", ""),
                    item("relation", "list", ""),
                    item("mention", "list --memory M | --entity E", ""),
                    item("extract", "status <memory_id> | backfill --memory ...", ""),
                ],
            },
            HelpSection {
                title: "META".into(),
                note: Some("(session-only by default; \\config set persists)".into()),
                items: vec![
                    item("quit | exit | \\q", "", "exit the shell"),
                    item("help [verb] | ? [verb] | \\?", "", "show help"),
                    item(
                        "\\set output",
                        "auto|table|wide|json|ndjson|yaml",
                        "output format",
                    ),
                    item("\\set context", "<N>", "session sticky --context"),
                    item("\\unset txn", "", "drop active transaction"),
                    item("\\timing", "on|off", "per-op wall time"),
                    item("\\connect", "<host:port>", "reconnect"),
                    item("\\info", "", "server / agent / session diagnostic"),
                ],
            },
            HelpSection {
                title: "PERSISTED".into(),
                note: Some("(~/.config/brain/config.toml)".into()),
                items: vec![item(
                    "\\config",
                    "list | get <key> | set <key> <value> | path | edit",
                    "",
                )],
            },
            HelpSection {
                title: "AGENTS".into(),
                note: None,
                items: vec![
                    item("\\agent", "", "current binding"),
                    item(
                        "\\agent",
                        "list | show [<name>] | use <name> | create <name>",
                        "",
                    ),
                    item("\\agent set-default", "<name>", "mark as factory default"),
                ],
            },
        ],
        footer: vec![
            "Tip: bare `brain` mints a fresh agent on first run.".into(),
            "Type `help <verb>` for per-verb usage.".into(),
        ],
    }
}

/// Two-column row helper. `flags` lands between the verb signature
/// and the description; an empty `flags` collapses to just the verb.
fn item(signature: &str, flags: &str, description: &str) -> HelpItem {
    let signature = if flags.is_empty() {
        signature.to_string()
    } else {
        format!("{signature} {flags}")
    };
    HelpItem {
        signature,
        description: description.to_string(),
    }
}

// ── per-verb cards ──────────────────────────────────────────────────

fn help_encode() -> HelpVerb {
    HelpVerb {
        name: "encode".into(),
        tagline: "write a memory".into(),
        usage: vec![
            "encode <TEXT> [--context N] [--kind episodic|semantic|consolidated]".into(),
            "       [--salience F] [--allow-duplicate] [--txn HEX]".into(),
        ],
        description: vec![
            "Stores text as a memory. Inherits the session's sticky --context and active transaction when those flags are omitted. ENCODE happens against the current agent (use `\\agent` to see the binding).".into(),
            "Deduplication is ON by default — encoding the same text twice in the same context returns the existing memory rather than creating a duplicate. Pass --allow-duplicate to force a fresh write (use this for episodic memory where the same content is a genuinely distinct event).".into(),
        ],
        see_also: vec!["recall".into(), "forget".into(), "link".into()],
    }
}

fn help_recall() -> HelpVerb {
    HelpVerb {
        name: "recall".into(),
        tagline: "find similar memories".into(),
        usage: vec![
            "recall <QUERY> [--top-k N] [--confidence F]".into(),
            "       [--filter-context N]... [--filter-kind K]... [--txn HEX]".into(),
        ],
        description: vec![
            "Retrieve memories whose embedding is similar to the query text. The returned ids are remembered in the session for tab-completion, so the next `forget` / `link` can refer to them by short id.".into(),
        ],
        see_also: vec!["encode".into(), "reason".into()],
    }
}

fn help_plan() -> HelpVerb {
    HelpVerb {
        name: "plan".into(),
        tagline: "plan a path".into(),
        usage: vec!["plan <FROM> <TO> [--max-steps N] [--max-wall-time-ms N]".into()],
        description: vec![
            "Plan a path between two textual states. Returns an ordered list of intermediate memories that bridge the gap.".into(),
        ],
        see_also: vec!["reason".into(), "recall".into()],
    }
}

fn help_reason() -> HelpVerb {
    HelpVerb {
        name: "reason".into(),
        tagline: "derive inferences".into(),
        usage: vec![
            "reason <OBSERVATION> [--depth N] [--confidence F] [--max-inferences N]".into(),
        ],
        description: vec![
            "Reason about a textual observation; returns a list of inference steps with the chain of supporting memories.".into(),
        ],
        see_also: vec!["recall".into(), "plan".into()],
    }
}

fn help_forget() -> HelpVerb {
    HelpVerb {
        name: "forget".into(),
        tagline: "tombstone a memory".into(),
        usage: vec!["forget <ID> [--mode soft|hard]".into()],
        description: vec![
            "Soft tombstones reclaim the slot after a grace period (default 7 days) — recoverable in case the operator changes their mind.".into(),
            "Hard erases zero the slot immediately. Use --mode hard only when content must be unrecoverable (right-to-be-forgotten / secret material).".into(),
        ],
        see_also: vec!["encode".into(), "recall".into()],
    }
}

fn help_link() -> HelpVerb {
    HelpVerb {
        name: "link".into(),
        tagline: "add an explicit edge".into(),
        usage: vec!["link <SRC> <KIND> <TGT> [--weight F] [--txn HEX]".into()],
        description: vec![
            "Add a typed edge between two memories. KIND is one of: caused, followed-by, derived-from, similar-to, contradicts, supports, references, part-of.".into(),
        ],
        see_also: vec!["unlink".into(), "recall".into()],
    }
}

fn help_unlink() -> HelpVerb {
    HelpVerb {
        name: "unlink".into(),
        tagline: "remove an edge".into(),
        usage: vec!["unlink <SRC> <KIND> <TGT> [--txn HEX]".into()],
        description: vec![
            "Remove a typed edge between two memories. Idempotent: removing a non-existent edge succeeds without error.".into(),
        ],
        see_also: vec!["link".into()],
    }
}

fn help_txn() -> HelpVerb {
    HelpVerb {
        name: "txn".into(),
        tagline: "transactions".into(),
        usage: vec![
            "txn begin                     open a transaction (sticks to the session)".into(),
            "txn commit <ID>               commit by id".into(),
            "txn abort <ID>                abort by id".into(),
        ],
        description: vec![
            "Within an active txn, subsequent encode/forget/link/unlink calls inherit the txn id unless --txn is passed explicitly. `\\unset txn` drops the session's active txn without affecting the server-side transaction.".into(),
        ],
        see_also: vec!["encode".into(), "forget".into()],
    }
}

fn help_subscribe() -> HelpVerb {
    HelpVerb {
        name: "subscribe".into(),
        tagline: "live event stream".into(),
        usage: vec!["subscribe [--context N]... [--kind K]... [--collect N]".into()],
        description: vec![
            "Without --collect, streams forever — events render as they arrive, Ctrl-C or SIGTERM cancels cleanly (server-side registry entry is removed). With --collect N, blocks until N events arrive then exits.".into(),
            "Filters within a kind/context are OR; across (kind AND context) is AND. --start-lsn / WAL replay is not supported in v1.".into(),
            "In the REPL, bare `subscribe` blocks the prompt — prefer running it in a second terminal so the writer (encode / forget) can fire events.".into(),
        ],
        see_also: vec!["encode".into(), "forget".into()],
    }
}

/// META aggregates a directory of meta commands. A single HelpVerb's
/// usage block keeps it scrollable as one card; the description owns
/// the categorised body (Session-only / Persisted / Agents) so a
/// reader sees the same shape they'd see in `\config`. Building a
/// nested HelpTopLevel here would visually conflict with the per-verb
/// card framing that `help meta` would otherwise inherit.
fn help_meta() -> HelpVerb {
    HelpVerb {
        name: "meta".into(),
        tagline: "meta commands reference".into(),
        usage: vec![
            "\\set output json|table        output format".into(),
            "\\set context <N>              sticky default --context".into(),
            "\\unset txn                    drop the active transaction".into(),
            "\\timing on|off                show per-op wall time".into(),
            "\\connect <host:port>          reconnect to a different server".into(),
            "\\info                         server / agent / connection / session diagnostic".into(),
            "\\config list|get|set|path|edit  manage ~/.config/brain/config.toml".into(),
            "\\agent                        current binding (id + source)".into(),
            "\\agent list|show|use|create|set-default  manage named agents".into(),
            "\\q                            exit (alias for quit)".into(),
        ],
        description: vec![
            "Session-only settings (the first block) live until quit. Persisted commands (`\\config set`, `\\agent use`, `\\agent set-default`) write to ~/.config/brain/config.toml and survive across sessions.".into(),
        ],
        see_also: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Downcast helper so tests can assert which concrete variant
    /// `lookup` returned. The Render trait is object-safe but doesn't
    /// expose `Any`; we go through the JSON envelope's `kind` field
    /// instead, which is part of the public contract.
    fn payload_kind(item: &dyn Render) -> String {
        use brain_explore::{RenderCtx, TermPolicy, Theme};
        let ctx = RenderCtx {
            policy: TermPolicy::plain(),
            theme: Theme::default(),
            format: brain_explore::OutputFormat::Json,
        };
        let v = item.render_json(&ctx);
        v["kind"].as_str().unwrap_or("").to_string()
    }

    #[test]
    fn lookup_none_returns_top_level() {
        let payload = lookup(None);
        assert_eq!(payload_kind(payload.as_ref()), "help-top-level");
    }

    #[test]
    fn lookup_known_verb_returns_help_verb() {
        let payload = lookup(Some("encode"));
        assert_eq!(payload_kind(payload.as_ref()), "help-verb");
    }

    #[test]
    fn lookup_unknown_returns_help_unknown_with_verb_echoed() {
        use brain_explore::{RenderCtx, TermPolicy, Theme};
        let payload = lookup(Some("wibble"));
        assert_eq!(payload_kind(payload.as_ref()), "help-unknown");
        let ctx = RenderCtx {
            policy: TermPolicy::plain(),
            theme: Theme::default(),
            format: brain_explore::OutputFormat::Json,
        };
        let v = payload.render_json(&ctx);
        assert_eq!(v["verb"], "wibble");
    }

    #[test]
    fn lookup_case_insensitive() {
        let upper = lookup(Some("ENCODE"));
        let lower = lookup(Some("encode"));
        assert_eq!(payload_kind(upper.as_ref()), payload_kind(lower.as_ref()));
        assert_eq!(payload_kind(upper.as_ref()), "help-verb");
    }

    #[test]
    fn each_known_verb_has_nonempty_usage_and_tagline() {
        // Regression guard: any verb fixture must have both a tagline
        // and at least one usage line, otherwise the per-verb card
        // renders empty sections.
        let verbs = [
            help_encode(),
            help_recall(),
            help_plan(),
            help_reason(),
            help_forget(),
            help_link(),
            help_unlink(),
            help_txn(),
            help_subscribe(),
            help_meta(),
        ];
        for v in &verbs {
            assert!(!v.tagline.is_empty(), "{} missing tagline", v.name);
            assert!(!v.usage.is_empty(), "{} missing usage", v.name);
        }
    }
}
