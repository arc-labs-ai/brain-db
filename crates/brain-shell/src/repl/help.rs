//! In-REPL help. Builds typed [`HelpTopLevel`] / [`HelpVerb`] /
//! [`HelpUnknown`] payloads for brain-explore to render — the data
//! lives here, the layout lives in brain-explore so the shell and CLI
//! pick up the same visual language.

use brain_explore::{
    HelpFlagRow, HelpItem, HelpReference, HelpSection, HelpTopLevel, HelpUnknown, HelpVerb, Render,
};

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

/// Construct a [`HelpFlagRow`]. Tiny shim that keeps each fixture's
/// flag table readable as a `[(sig, desc), …]` literal instead of an
/// inline struct constructor on every row.
fn row(sig: &str, desc: &str) -> HelpFlagRow {
    HelpFlagRow {
        signature: sig.to_string(),
        description: desc.to_string(),
    }
}

/// Build the per-verb markdown reference path. Centralises the
/// `docs/reference/shell/commands/<verb>.md` convention so a future
/// docs reorg only edits one place.
fn doc_for(verb: &str) -> String {
    format!("docs/reference/shell/commands/{verb}.md")
}

fn help_encode() -> HelpVerb {
    HelpVerb {
        name: "encode".into(),
        tagline: "write a memory".into(),
        usage: vec![
            "encode <TEXT> [flags]".into(),
            "encode --from-file <PATH> [flags]".into(),
            "encode --from-stdin [flags]".into(),
        ],
        flags: vec![
            row("--context N", "u64; default 0; sticky via \\set context"),
            row(
                "--kind K",
                "episodic | semantic | consolidated (default: episodic)",
            ),
            row("--salience F", "[0.0, 1.0]; default 0.5"),
            row(
                "--allow-duplicate",
                "force fresh write; dedup is ON by default",
            ),
            row("--edge KIND:ID", "add edge at create time; repeatable"),
            row("--request-id UUID", "idempotency key (24h cache)"),
            row(
                "--wait-for-extraction",
                "block until knowledge layer extracts",
            ),
            row("--txn HEX", "attach to an open transaction"),
        ],
        sources: vec![
            row("<TEXT>", "inline string (the default)"),
            row("--from-file P", "read from file (use - for stdin)"),
            row("--from-stdin", "shorthand for --from-file -"),
            row("--vector CSV", "gated — panics today; see Notes"),
        ],
        description: vec![
            "Dedup is on by default — encoding the same text twice in the same (agent, context) returns the existing memory. Pass --allow-duplicate for episodic events where the same content is a genuinely distinct event.".into(),
            "Inherits the session's sticky --context (set via \\set context N) and active txn (begun via `txn begin`) unless the corresponding flag overrides. The card shows the caller's agent in wide output (`-o wide`).".into(),
            "--vector and --from-file *.jsonl are parsed today but not wired server-side — they panic / return a stub error. The other flags work as documented.".into(),
        ],
        example: Some(r#"encode "Alice merged the auth-rewrite branch" --context 7"#.into()),
        see_also: vec![
            "recall".into(),
            "forget".into(),
            "link".into(),
            "subscribe".into(),
        ],
        reference: Some(HelpReference {
            clap_command: "encode --help".into(),
            doc_path: Some(doc_for("encode")),
        }),
    }
}

fn help_recall() -> HelpVerb {
    HelpVerb {
        name: "recall".into(),
        tagline: "find similar memories".into(),
        usage: vec!["recall <QUERY> [flags]".into()],
        flags: vec![
            row("--top-k N", "result cap; default 10; server clamps to cap"),
            row("--confidence F", "[0.0, 1.0] similarity threshold; default 0.0"),
            row(
                "--filter-context N",
                "keep results from this context; repeatable, up to 16",
            ),
            row(
                "--filter-kind K",
                "episodic | semantic | consolidated; repeatable",
            ),
            row("--include-text", "populate the text column (off by default)"),
            row(
                "--include-graph",
                "request per-hit graph enrichment (gated)",
            ),
            row(
                "--txn HEX",
                "read inside an open transaction; auto in REPL",
            ),
        ],
        sources: vec![],
        description: vec![
            "Vector-similarity search. Embeds the query text, runs a top-K HNSW lookup in the active context's index, returns ranked MemoryResults. Substrate path compares the threshold to similarity_score; hybrid (knowledge-layer) path compares it to the RRF-fused score.".into(),
            "When the top-K scores cluster within Δ<0.001, the table footer warns that ranking may not be meaningful — typically the embedder isn't loaded (test mode) or the query genuinely doesn't discriminate. Don't trust the order in that case.".into(),
            "Returned ids are remembered in the session for tab-completion — the next `forget` or `link` can refer to them by short id.".into(),
        ],
        example: Some(r#"recall "auth rewrite" --top-k 5 --include-text --filter-context 7"#.into()),
        see_also: vec![
            "encode".into(),
            "reason".into(),
            "subscribe".into(),
        ],
        reference: Some(HelpReference {
            clap_command: "recall --help".into(),
            doc_path: Some(doc_for("recall")),
        }),
    }
}

fn help_plan() -> HelpVerb {
    HelpVerb {
        name: "plan".into(),
        tagline: "plan a path".into(),
        usage: vec!["plan <FROM> <TO> [flags]".into()],
        flags: vec![
            row("--max-steps N", "result cap; default 10"),
            row(
                "--max-wall-time-ms N",
                "wall-time budget in ms; default 5000",
            ),
        ],
        sources: vec![],
        description: vec![
            "Plan a path between two textual states. Returns an ordered list of intermediate memories — each step labelled with a confidence and a heuristic est_to_goal — that bridge the gap.".into(),
            "The footer surfaces a `status=` line loudly when the result is not `GoalReached` (e.g. `MaxStepsHit`, `BudgetExhausted`) so partial results aren't mistaken for complete ones.".into(),
        ],
        example: Some(r#"plan "kickoff" "shipped" --max-steps 5"#.into()),
        see_also: vec!["reason".into(), "recall".into()],
        reference: Some(HelpReference {
            clap_command: "plan --help".into(),
            doc_path: Some(doc_for("plan")),
        }),
    }
}

fn help_reason() -> HelpVerb {
    HelpVerb {
        name: "reason".into(),
        tagline: "derive inferences".into(),
        usage: vec!["reason <OBSERVATION> [flags]".into()],
        flags: vec![
            row("--depth N", "reasoning depth; default 3"),
            row(
                "--confidence F",
                "[0.0, 1.0] confidence threshold; default 0.0",
            ),
            row("--max-inferences N", "result cap; default 16"),
        ],
        sources: vec![],
        description: vec![
            "Reason about a textual observation; returns a list of inference steps, each carrying a confidence and the chain of supporting memories that produced it.".into(),
            "Depth bounds how many derivation hops the engine will chase; raising it surfaces longer chains at the cost of latency and noise.".into(),
        ],
        example: Some(r#"reason "the build broke" --depth 4 --confidence 0.5"#.into()),
        see_also: vec!["recall".into(), "plan".into()],
        reference: Some(HelpReference {
            clap_command: "reason --help".into(),
            doc_path: Some(doc_for("reason")),
        }),
    }
}

fn help_forget() -> HelpVerb {
    HelpVerb {
        name: "forget".into(),
        tagline: "tombstone a memory".into(),
        usage: vec!["forget <ID> [flags]".into()],
        flags: vec![
            row(
                "--mode M",
                "soft (default) | hard — hard zeroes the slot immediately",
            ),
            row("--txn HEX", "tombstone inside an open transaction"),
        ],
        sources: vec![],
        description: vec![
            "Soft tombstones reclaim the slot after a grace period (default 7 days) — recoverable in case the operator changes their mind. The fingerprint is evicted in the same write transaction as the tombstone so a re-encode of the same content is a dedup miss, not a hit.".into(),
            "Hard erases zero the slot immediately. Use --mode hard only when content must be unrecoverable (right-to-be-forgotten / secret material) — the operation is not reversible.".into(),
            "Forgetting a non-existent or already-tombstoned id returns success (idempotent) — outcome=MemoryNotFound or outcome=AlreadyTombstoned.".into(),
        ],
        example: Some("forget s2/m17/v1 --mode hard".into()),
        see_also: vec!["encode".into(), "recall".into(), "subscribe".into()],
        reference: Some(HelpReference {
            clap_command: "forget --help".into(),
            doc_path: Some(doc_for("forget")),
        }),
    }
}

fn help_link() -> HelpVerb {
    HelpVerb {
        name: "link".into(),
        tagline: "add an explicit edge".into(),
        usage: vec!["link <SRC> <KIND> <TGT> [flags]".into()],
        flags: vec![
            row(
                "<KIND>",
                "caused | followed-by | derived-from | similar-to | contradicts | supports | references | part-of",
            ),
            row("--weight F", "[0.0, 1.0]; default 1.0"),
            row("--txn HEX", "link inside an open transaction"),
        ],
        sources: vec![],
        description: vec![
            "Add a typed edge between two memories. Source and target ids accept any of the three MemoryId input forms — short (s2/m17/v1), long hex (0x…), decimal u128 — including pasting from a recall table directly.".into(),
            "Kind names accept both hyphen and underscore variants (followed-by ≡ followed_by); co_occurs is normalised to similar_to. Inline edges added at encode-time via `--edge KIND:ID` fix the weight at 1.0 — use `link` after the fact to vary it.".into(),
        ],
        example: Some("link s2/m1/v1 caused s2/m2/v1 --weight 0.8".into()),
        see_also: vec!["unlink".into(), "encode".into(), "recall".into()],
        reference: Some(HelpReference {
            clap_command: "link --help".into(),
            doc_path: Some(doc_for("link")),
        }),
    }
}

fn help_unlink() -> HelpVerb {
    HelpVerb {
        name: "unlink".into(),
        tagline: "remove an edge".into(),
        usage: vec!["unlink <SRC> <KIND> <TGT> [flags]".into()],
        flags: vec![
            row("<KIND>", "same whitelist as `link`"),
            row("--txn HEX", "unlink inside an open transaction"),
        ],
        sources: vec![],
        description: vec![
            "Remove a typed edge between two memories. Idempotent — removing a non-existent edge succeeds without error so retries from a flaky client are safe.".into(),
            "Source / target ids accept short, long-hex, or decimal forms (same as `link`).".into(),
        ],
        example: Some("unlink s2/m1/v1 caused s2/m2/v1".into()),
        see_also: vec!["link".into(), "recall".into()],
        reference: Some(HelpReference {
            clap_command: "unlink --help".into(),
            doc_path: Some(doc_for("unlink")),
        }),
    }
}

fn help_txn() -> HelpVerb {
    HelpVerb {
        name: "txn".into(),
        tagline: "multi-op atomicity".into(),
        usage: vec![
            "txn begin                     open a transaction (sticks to the session)".into(),
            "txn commit <ID>               commit by id".into(),
            "txn abort <ID>                abort by id".into(),
        ],
        flags: vec![],
        sources: vec![],
        description: vec![
            "Within an active txn, subsequent encode/forget/link/unlink calls inherit the txn id unless --txn is passed explicitly. The prompt switches to `brain*>` while a session txn is active so it's visible at a glance.".into(),
            "`\\unset txn` drops the session's local handle on the txn without sending anything to the server — useful when you want to issue a one-off outside-the-txn read. The server-side transaction stays open until commit/abort.".into(),
            "Recall reads inside a txn see the txn's pending writes — so encode + recall in one transaction is atomically self-consistent.".into(),
        ],
        example: Some("txn begin   →   encode \"...\"   →   recall \"...\"   →   txn commit <id>".into()),
        see_also: vec!["encode".into(), "recall".into(), "forget".into()],
        reference: Some(HelpReference {
            clap_command: "txn --help".into(),
            doc_path: Some(doc_for("txn")),
        }),
    }
}

fn help_subscribe() -> HelpVerb {
    HelpVerb {
        name: "subscribe".into(),
        tagline: "live event stream".into(),
        usage: vec!["subscribe [flags]".into()],
        flags: vec![
            row(
                "--context N",
                "filter to this context id; repeatable, up to 16",
            ),
            row(
                "--kind K",
                "episodic | semantic | consolidated; repeatable",
            ),
            row(
                "--start-lsn N",
                "replay history from this LSN before joining the live tail (0 = oldest)",
            ),
            row(
                "--collect N",
                "batch: wait for exactly N events then exit",
            ),
        ],
        sources: vec![],
        description: vec![
            "Two modes. Streaming (default) tails forever — events flush per-line so `subscribe | jq` sees them as they arrive; Ctrl-C / SIGTERM unsubscribes cleanly. Batch (--collect N) waits for exactly N events and exits with the collected list.".into(),
            "Structured output auto-downgrades to ndjson while streaming so pretty JSON / YAML don't buffer poorly across event boundaries. Table output stays as table.".into(),
            "Filters within a kind/context list are OR; across (kind AND context) is AND. --start-lsn below the oldest retained LSN returns SubscriptionLsnTooOld with the actual oldest in the message.".into(),
            "In the REPL, bare `subscribe` blocks the prompt — prefer running it in a second terminal so the writer (encode / forget) can fire events.".into(),
        ],
        example: Some("subscribe --context 7 --start-lsn 0 --collect 50".into()),
        see_also: vec!["encode".into(), "forget".into(), "recall".into()],
        reference: Some(HelpReference {
            clap_command: "subscribe --help".into(),
            doc_path: Some(doc_for("subscribe")),
        }),
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
        flags: vec![],
        sources: vec![],
        description: vec![
            "Session-only settings (the first block) live until quit. Persisted commands (`\\config set`, `\\agent use`, `\\agent set-default`) write to ~/.config/brain/config.toml and survive across sessions.".into(),
            "Per-meta-command deep dives live under docs/reference/shell/meta/ — one page per meta verb (agent, config, set, unset, info, timing, connect, help).".into(),
        ],
        example: Some("\\set output ndjson   →   \\set context 7   →   encode \"...\"".into()),
        see_also: vec![],
        reference: Some(HelpReference {
            clap_command: "brain --help".into(),
            doc_path: Some("docs/reference/shell/meta/".into()),
        }),
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

    /// Render the verb's card to a plain-policy table and return the
    /// raw string so per-verb regression tests can grep for flag names.
    fn render_card_table(verb: &HelpVerb) -> String {
        use brain_explore::{RenderCtx, TermPolicy, Theme};
        let ctx = RenderCtx {
            policy: TermPolicy::plain(),
            theme: Theme::default(),
            format: brain_explore::OutputFormat::Table,
        };
        let mut buf = Vec::new();
        brain_explore::dispatch(verb, &ctx, &mut buf).expect("render");
        String::from_utf8(buf).expect("utf8")
    }

    #[test]
    fn help_encode_lists_every_documented_flag() {
        // Regression guard: if a flag from the reference doc isn't in
        // the card, the card lies to the user about what flags exist.
        let card = render_card_table(&help_encode());
        for flag in [
            "--context",
            "--kind",
            "--salience",
            "--allow-duplicate",
            "--edge",
            "--request-id",
            "--wait-for-extraction",
            "--txn",
        ] {
            assert!(card.contains(flag), "missing flag {flag} in:\n{card}");
        }
        // Sources block must list all four source forms.
        for source in ["<TEXT>", "--from-file", "--from-stdin", "--vector"] {
            assert!(
                card.contains(source),
                "missing source {source} in:\n{card}"
            );
        }
        // Reference block must surface clap + markdown deep-dive.
        assert!(card.contains("encode --help"), "missing clap reference");
        assert!(
            card.contains("docs/reference/shell/commands/encode.md"),
            "missing markdown reference"
        );
        // Example line must be present.
        assert!(card.contains("Example"), "missing Example label");
    }

    #[test]
    fn help_recall_lists_every_documented_flag() {
        let card = render_card_table(&help_recall());
        for flag in [
            "--top-k",
            "--confidence",
            "--filter-context",
            "--filter-kind",
            "--include-text",
            "--include-graph",
            "--txn",
        ] {
            assert!(card.contains(flag), "missing flag {flag} in:\n{card}");
        }
        assert!(card.contains("recall --help"));
        assert!(card.contains("docs/reference/shell/commands/recall.md"));
    }

    #[test]
    fn help_forget_lists_every_documented_flag() {
        let card = render_card_table(&help_forget());
        assert!(card.contains("--mode"));
        assert!(card.contains("--txn"));
        assert!(card.contains("forget --help"));
        assert!(card.contains("docs/reference/shell/commands/forget.md"));
    }

    #[test]
    fn help_link_lists_kind_whitelist() {
        let card = render_card_table(&help_link());
        assert!(card.contains("--weight"));
        assert!(card.contains("--txn"));
        // Kind whitelist must list every accepted kind so a reader
        // sees what `<KIND>` can be without leaving the card.
        for kind in [
            "caused",
            "followed-by",
            "derived-from",
            "similar-to",
            "contradicts",
            "supports",
            "references",
            "part-of",
        ] {
            assert!(card.contains(kind), "missing kind {kind} in:\n{card}");
        }
        assert!(card.contains("link --help"));
    }

    #[test]
    fn help_unlink_documents_kind_and_txn() {
        let card = render_card_table(&help_unlink());
        assert!(card.contains("<KIND>"));
        assert!(card.contains("--txn"));
        assert!(card.contains("unlink --help"));
    }

    #[test]
    fn help_plan_lists_every_documented_flag() {
        let card = render_card_table(&help_plan());
        assert!(card.contains("--max-steps"));
        assert!(card.contains("--max-wall-time-ms"));
        assert!(card.contains("plan --help"));
        // Status footer note is what stops users mistaking partial
        // results for complete ones — must be in Notes.
        assert!(card.contains("GoalReached"));
    }

    #[test]
    fn help_reason_lists_every_documented_flag() {
        let card = render_card_table(&help_reason());
        assert!(card.contains("--depth"));
        assert!(card.contains("--confidence"));
        assert!(card.contains("--max-inferences"));
        assert!(card.contains("reason --help"));
    }

    #[test]
    fn help_txn_lists_subcommands_in_usage() {
        // `txn` has sub-commands instead of flags — Flags block stays
        // empty by design; Usage carries all three forms.
        let card = render_card_table(&help_txn());
        assert!(card.contains("txn begin"));
        assert!(card.contains("txn commit"));
        assert!(card.contains("txn abort"));
        assert!(card.contains("txn --help"));
    }

    #[test]
    fn help_meta_points_at_per_command_docs() {
        let card = render_card_table(&help_meta());
        // Reference must include the per-meta-command docs dir so a
        // reader knows where to find the deep dive for each meta verb.
        assert!(card.contains("docs/reference/shell/meta/"));
        // Usage block keeps the directory of meta commands.
        for cmd in ["\\set", "\\agent", "\\config", "\\info"] {
            assert!(card.contains(cmd), "missing meta cmd {cmd} in:\n{card}");
        }
    }

    #[test]
    fn help_subscribe_lists_every_documented_flag() {
        let card = render_card_table(&help_subscribe());
        assert!(card.contains("--context"));
        assert!(card.contains("--kind"));
        assert!(card.contains("--start-lsn"));
        assert!(card.contains("--collect"));
        assert!(card.contains("subscribe --help"));
        // Notes must mention ndjson auto-downgrade so a scripting
        // user doesn't get burned by buffered JSON.
        assert!(card.contains("ndjson"));
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
