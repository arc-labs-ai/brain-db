//! In-REPL help text.

/// Return a help blurb for `verb` or the top-level list.
#[must_use]
pub fn lookup(verb: Option<&str>) -> String {
    match verb.map(str::to_ascii_lowercase).as_deref() {
        None | Some("") | Some("help") => top_level(),
        Some("encode") => ENCODE.to_string(),
        Some("recall") => RECALL.to_string(),
        Some("plan") => PLAN.to_string(),
        Some("reason") => REASON.to_string(),
        Some("forget") => FORGET.to_string(),
        Some("link") => LINK.to_string(),
        Some("unlink") => UNLINK.to_string(),
        Some("txn") => TXN.to_string(),
        Some("subscribe") => SUBSCRIBE.to_string(),
        Some("meta") | Some("\\") => META.to_string(),
        Some(other) => format!("no help for `{other}`. Try `help` for the list."),
    }
}

fn top_level() -> String {
    let mut s = String::new();
    s.push_str("Cognitive verbs:\n");
    s.push_str("  encode <TEXT> [--context N] [--kind ...] [--salience F] [--allow-duplicate]\n");
    s.push_str("         [--edge KIND:ID]... [--request-id UUID] [--from-file PATH]\n");
    s.push_str("         [--from-stdin] [--vector CSV] [--wait-for-extraction]\n");
    s.push_str("  recall <QUERY> [--top-k N] [--confidence F] [--filter-context N]...\n");
    s.push_str("         [--filter-kind K]... [--include-text] [--include-graph]\n");
    s.push_str("  plan <FROM> <TO> [--max-steps N] [--max-wall-time-ms N]\n");
    s.push_str("  reason <OBS> [--depth N] [--confidence F] [--max-inferences N]\n");
    s.push_str("  forget <ID> [--mode soft|hard]\n");
    s.push_str("  link <SRC> <KIND> <TGT> [--weight F]\n");
    s.push_str("  unlink <SRC> <KIND> <TGT>\n");
    s.push_str("  txn begin | txn commit <ID> | txn abort <ID>\n");
    s.push_str("  subscribe [--context N]... [--kind K]... [--collect N]\n");
    s.push_str("\nKnowledge browsing:\n");
    s.push_str("  entity list [--type T] [--limit N] [--prefix STR]\n");
    s.push_str("  entity show <id|name>\n");
    s.push_str("  entity neighbors <id> [--depth N]\n");
    s.push_str("  statement list [--subject E] [--predicate P] [--object E]\n");
    s.push_str("  statement show <id>\n");
    s.push_str("  relation list [--from E] [--to E] [--type T]\n");
    s.push_str("  mention list --memory M | --entity E\n");
    s.push_str("  extract status <memory_id>\n");
    s.push_str("  extract backfill --memory <id> | --since <ts> | --all\n");
    s.push_str("\nMeta (session-only by default — `\\config set` persists):\n");
    s.push_str("  quit | exit | \\q                 exit the shell\n");
    s.push_str("  help [verb] | ? [verb] | \\?       show help\n");
    s.push_str("  \\set output auto|table|wide|json|ndjson|yaml\n");
    s.push_str("  \\set context <N>                 session-only sticky --context\n");
    s.push_str("  \\unset txn                       drop the active transaction\n");
    s.push_str("  \\timing on|off                   per-op wall time\n");
    s.push_str("  \\connect <host:port>             reconnect to a different server\n");
    s.push_str(
        "  \\info                            server / agent / connection / session diagnostic\n",
    );
    s.push_str("\nPersisted (writes ~/.config/brain/config.toml):\n");
    s.push_str("  \\config list                     show effective settings\n");
    s.push_str("  \\config get <key>                read a single setting\n");
    s.push_str("  \\config set <key> <value>        persist + apply to session\n");
    s.push_str("  \\config path                     print the config file path\n");
    s.push_str("  \\config edit                     open the file in $EDITOR\n");
    s.push_str("\nAgents (named identities, see `brain agent --help`):\n");
    s.push_str("  \\agent                           current binding (id + source)\n");
    s.push_str("  \\agent list                      named agents in config.toml\n");
    s.push_str("  \\agent show [<name>]             full record\n");
    s.push_str("  \\agent use <name>                switch + persist as active\n");
    s.push_str("  \\agent create <name> [--note T]  mint a fresh agent\n");
    s.push_str("  \\agent set-default <name>        mark <name> as default (sticky)\n");
    s.push_str(
        "\nFirst run with no agent? brain mints `agent-<8hex>` and marks it\n\
         default + active. Use `\\agent create <name>` to add another, and\n\
         `\\agent use <name>` to switch (persists across sessions).\n",
    );
    s
}

const ENCODE: &str = "encode <TEXT> [--context N] [--kind episodic|semantic|consolidated]\n\
                     [--salience F] [--allow-duplicate] [--txn HEX]\n\
\n\
Store text as a memory. Inherits the session's sticky --context and\n\
active transaction when those flags are omitted. ENCODE happens against\n\
the current agent (use `\\agent` to see the binding).\n\
\n\
Deduplication is ON by default — encoding the same text twice in the\n\
same context returns the existing memory rather than creating a duplicate.\n\
Pass --allow-duplicate to force a fresh write (use this for episodic\n\
memory where the same content is a genuinely distinct event).";

const RECALL: &str = "recall <QUERY> [--top-k N] [--confidence F]\n\
                     [--filter-context N]... [--filter-kind K]... [--txn HEX]\n\
\n\
Retrieve similar memories. The returned ids are remembered in the\n\
session for tab-completion.";

const PLAN: &str = "plan <FROM> <TO> [--max-steps N] [--max-wall-time-ms N]\n\
\n\
Plan a path between two textual states.";

const REASON: &str = "reason <OBSERVATION> [--depth N] [--confidence F] [--max-inferences N]\n\
\n\
Reason about a textual observation; returns a list of inference steps.";

const FORGET: &str = "forget <ID> [--mode soft|hard]\n\
\n\
Soft tombstones reclaim the slot after a grace period (default 7 days).\n\
Hard erases zero the slot immediately (spec §09/06).";

const LINK: &str = "link <SRC> <KIND> <TGT> [--weight F] [--txn HEX]\n\
\n\
KIND is one of: caused, followed-by, derived-from, similar-to,\n\
contradicts, supports, references, part-of.";

const UNLINK: &str = "unlink <SRC> <KIND> <TGT> [--txn HEX]\n\
\n\
Idempotent: removing a non-existent edge succeeds.";

const TXN: &str = "txn begin                     open a transaction (sticks to the session)\n\
txn commit <ID>               commit by id\n\
txn abort <ID>                abort by id\n\
\n\
Within an active txn, subsequent encode/forget/link/unlink calls inherit\n\
the txn id unless --txn is passed explicitly.";

const SUBSCRIBE: &str = "subscribe [--context N]... [--kind K]... [--collect N]\n\
\n\
Without --collect, streams forever — events render as they arrive,\n\
Ctrl-C or SIGTERM cancels cleanly (server-side registry entry is\n\
removed). With --collect N, blocks until N events arrive then exits.\n\
\n\
Filters within a kind/context are OR; across (kind AND context) is AND.\n\
--start-lsn / WAL replay is not supported in v1.\n\
\n\
In the REPL, bare `subscribe` blocks the prompt — prefer running it\n\
in a second terminal so the writer (encode / forget) can fire events.";

const META: &str = "Meta commands (handled before clap parsing).\n\
\n\
Session-only (lost on quit):\n\
  \\set output json|table     output format\n\
  \\set context <N>           sticky default --context\n\
  \\unset txn                 drop the active transaction\n\
  \\timing on|off             show per-op wall time\n\
  \\connect <host:port>       reconnect to a different server\n\
  \\info                      server / agent / connection / session diagnostic\n\
\n\
Persisted (writes ~/.config/brain/config.toml):\n\
  \\config list               show effective settings\n\
  \\config get <key>          read a single setting\n\
  \\config set <key> <value>  persist + mirror into the session\n\
  \\config path               print the file path\n\
  \\config edit               open in $EDITOR (or vi)\n\
\n\
Agents:\n\
  \\agent                     current binding (id + source)\n\
  \\agent list                table of named agents\n\
  \\agent show [<name>]       full record\n\
  \\agent use <name>          switch + persist as active\n\
  \\agent create <name>       mint and persist a fresh agent\n\
  \\agent set-default <name>  mark as default (factory fallback)\n\
\n\
  \\q                         exit (alias for quit)\n";
