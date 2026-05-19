//! `clap` command tree shared by REPL and one-shot.

use std::net::SocketAddr;
use std::num::ParseIntError;
use std::str::FromStr;

use brain_core::MemoryId;
use brain_protocol::request::{EdgeKindWire, ForgetMode, MemoryKindWire};
use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

/// Default wire-protocol endpoint (matches `config/dev.toml`).
pub const DEFAULT_SERVER: &str = "127.0.0.1:9090";

/// Top-level `brain` CLI tree. Both argv and REPL lines parse
/// through this.
#[derive(Debug, Parser)]
#[command(
    name = "brain",
    version,
    about = "Interactive shell for the Brain cognitive substrate.",
    disable_help_subcommand = true,
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalOpts,

    #[command(subcommand)]
    pub subcommand: Option<Command>,
}

/// Connection + I/O knobs that apply to every subcommand.
#[derive(Debug, Args, Clone)]
pub struct GlobalOpts {
    /// Wire-protocol endpoint (`host:port`).
    #[arg(long, global = true, default_value = DEFAULT_SERVER, env = "BRAIN_SERVER")]
    pub server: String,

    /// Named agent (looked up in ~/.config/brain/config.toml). Use
    /// `brain agent list` to see configured names.
    // Env-var (BRAIN_AGENT) handling lives in cli::agent_id::resolve.
    #[arg(long, global = true)]
    pub agent: Option<String>,

    /// Raw agent UUID. Bypasses the named-agent lookup entirely.
    // Env-var (BRAIN_AGENT_ID) handling lives in cli::agent_id::resolve.
    #[arg(long, global = true)]
    pub agent_id: Option<String>,

    /// Output format. Defaults: table in REPL, json when stdout is not a TTY.
    #[arg(long, global = true, value_enum)]
    pub output: Option<OutputFormatArg>,

    /// Per-op timeout in seconds.
    #[arg(long, global = true, default_value_t = 30u64)]
    pub timeout: u64,

    /// Reserved for v2 auth. Parsed and ignored in v1.
    #[arg(long, global = true)]
    pub token: Option<String>,
}

/// Output-format selector shared by `--output` and `\set output`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormatArg {
    Table,
    Json,
}

/// All shell subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Encode a memory (text → vector + slot).
    Encode(EncodeArgs),
    /// Recall similar memories by cue text.
    Recall(RecallArgs),
    /// Plan a path between two states.
    Plan(PlanArgs),
    /// Reason about an observation.
    Reason(ReasonArgs),
    /// Forget a memory (soft tombstone or hard erase).
    Forget(ForgetArgs),
    /// Add an edge between two memories.
    Link(LinkArgs),
    /// Remove an edge between two memories.
    Unlink(UnlinkArgs),
    /// Transaction control (begin / commit / abort).
    #[command(subcommand)]
    Txn(TxnCommand),
    /// Subscribe to change events.
    Subscribe(SubscribeArgs),
    /// Inspect / mutate persisted shell settings.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Manage named agents (list / show / create / rename / delete / import).
    #[command(subcommand)]
    Agent(AgentCommand),
    /// Drop into the interactive REPL (default when no subcommand given).
    Shell,
    /// Emit a shell-completion script.
    GenerateCompletion(GenerateCompletionArgs),
}

/// `brain config <…>` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum ConfigCommand {
    /// Show the effective settings (file + defaults), one per line.
    List,
    /// Print a single value (script-friendly, no decoration).
    Get { key: String },
    /// Validate, write the file, echo the change.
    Set { key: String, value: String },
    /// Print the config file path.
    Path,
    /// Open the config file in $EDITOR ($VISUAL → $EDITOR → vi).
    Edit,
}

/// `brain agent <…>` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum AgentCommand {
    /// Table of name / id / created_at / note; `*` marks the agent
    /// the current invocation would bind to.
    List,
    /// Full record for a single agent. Omit the name to see what
    /// the next connect would use.
    Show { name: Option<String> },
    /// Mint a fresh UUIDv7, write the entry, echo the new id.
    Create {
        name: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// Rename an existing entry. Errors if the new name exists.
    Rename { old: String, new: String },
    /// Remove an entry. Refuses to delete the agent the current
    /// invocation is bound to.
    Delete { name: String },
    /// Adopt an externally-supplied UUID under a local name (for
    /// agents shared by teammates).
    Import {
        name: String,
        id: String,
        #[arg(long)]
        note: Option<String>,
    },
}

/// `MemoryId` parser that accepts three input forms:
///
/// * **Short form** — `s{shard}/m{slot}/v{version}`, e.g. `s2/m1/v1`.
///   This is the table-display form (see `output::table::fmt_short_id`)
///   so users can paste straight from a recall result.
/// * **Long hex** — `0x` + 32 hex chars, the canonical wire-shaped id.
/// * **Decimal** — a bare `u128` literal, for scripts that build ids
///   numerically.
///
/// Keeping all three accepted means the documented UX ("paste any id
/// you see anywhere") actually works, instead of forcing users to
/// convert short ids back to hex by hand.
#[derive(Debug, Clone, Copy)]
pub struct MemoryIdArg(pub MemoryId);

impl FromStr for MemoryIdArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();

        // Short form first: a leading `s` followed by a `/` is
        // unambiguous (decimal can't start with `s`, hex starts with
        // `0x`).
        if let Some(stripped) = trimmed.strip_prefix('s').or_else(|| trimmed.strip_prefix('S')) {
            if stripped.contains('/') {
                return parse_short_form(stripped).map(MemoryIdArg);
            }
        }

        let raw: u128 = if let Some(rest) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
            u128::from_str_radix(rest, 16).map_err(parse_int_err("hex"))?
        } else {
            trimmed.parse::<u128>().map_err(parse_int_err("decimal"))?
        };
        Ok(MemoryIdArg(MemoryId::from_raw(raw)))
    }
}

/// Parse the body of a short-form id — everything after the leading
/// `s`. Expects `{shard}/m{slot}/v{version}` with the `m` / `v`
/// prefixes mandatory so a stray `1/2/3` doesn't silently coerce into
/// something it isn't.
fn parse_short_form(body: &str) -> Result<MemoryId, String> {
    let mut parts = body.split('/');
    let shard_str = parts.next().ok_or_else(|| short_form_err("missing shard"))?;
    let slot_str = parts.next().ok_or_else(|| short_form_err("missing slot"))?;
    let version_str = parts.next().ok_or_else(|| short_form_err("missing version"))?;
    if parts.next().is_some() {
        return Err(short_form_err("too many components"));
    }

    let slot_str = slot_str
        .strip_prefix('m')
        .or_else(|| slot_str.strip_prefix('M'))
        .ok_or_else(|| short_form_err("slot must start with 'm'"))?;
    let version_str = version_str
        .strip_prefix('v')
        .or_else(|| version_str.strip_prefix('V'))
        .ok_or_else(|| short_form_err("version must start with 'v'"))?;

    let shard: u16 = shard_str.parse().map_err(|e| short_form_err(&format!("shard: {e}")))?;
    let slot: u64 = slot_str.parse().map_err(|e| short_form_err(&format!("slot: {e}")))?;
    let version: u32 = version_str.parse().map_err(|e| short_form_err(&format!("version: {e}")))?;

    Ok(MemoryId::pack(shard, slot, version))
}

fn short_form_err(msg: &str) -> String {
    format!("invalid short-form memory id (expected `s<shard>/m<slot>/v<version>`): {msg}")
}

fn parse_int_err(kind: &'static str) -> impl Fn(ParseIntError) -> String {
    move |e| format!("invalid {kind} memory id: {e}")
}

/// `MemoryKindWire` clap shim — `clap::ValueEnum` doesn't reach into
/// brain-protocol so we re-derive locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KindArg {
    Episodic,
    Semantic,
    Consolidated,
}

impl KindArg {
    #[must_use]
    pub fn into_wire(self) -> MemoryKindWire {
        match self {
            KindArg::Episodic => MemoryKindWire::Episodic,
            KindArg::Semantic => MemoryKindWire::Semantic,
            KindArg::Consolidated => MemoryKindWire::Consolidated,
        }
    }
}

/// `EdgeKindWire` clap shim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EdgeKindArg {
    Caused,
    FollowedBy,
    DerivedFrom,
    SimilarTo,
    Contradicts,
    Supports,
    References,
    PartOf,
}

impl EdgeKindArg {
    #[must_use]
    pub fn into_wire(self) -> EdgeKindWire {
        match self {
            EdgeKindArg::Caused => EdgeKindWire::Caused,
            EdgeKindArg::FollowedBy => EdgeKindWire::FollowedBy,
            EdgeKindArg::DerivedFrom => EdgeKindWire::DerivedFrom,
            EdgeKindArg::SimilarTo => EdgeKindWire::SimilarTo,
            EdgeKindArg::Contradicts => EdgeKindWire::Contradicts,
            EdgeKindArg::Supports => EdgeKindWire::Supports,
            EdgeKindArg::References => EdgeKindWire::References,
            EdgeKindArg::PartOf => EdgeKindWire::PartOf,
        }
    }
}

/// `ForgetMode` clap shim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ForgetModeArg {
    Soft,
    Hard,
}

impl ForgetModeArg {
    #[must_use]
    pub fn into_wire(self) -> ForgetMode {
        match self {
            ForgetModeArg::Soft => ForgetMode::Soft,
            ForgetModeArg::Hard => ForgetMode::Hard,
        }
    }
}

// ─── per-subcommand argument structs ────────────────────────────

#[derive(Debug, Args, Clone)]
pub struct EncodeArgs {
    /// Memory text.
    pub text: String,
    /// Context id (default 0).
    #[arg(long)]
    pub context: Option<u64>,
    /// Memory kind.
    #[arg(long, value_enum)]
    pub kind: Option<KindArg>,
    /// Salience hint in `[0.0, 1.0]`.
    #[arg(long)]
    pub salience: Option<f32>,
    /// Ask the server to deduplicate by fingerprint.
    #[arg(long)]
    pub deduplicate: bool,
    /// Bind to an active transaction (hex bytes).
    #[arg(long)]
    pub txn: Option<String>,
}

#[derive(Debug, Args, Clone)]
pub struct RecallArgs {
    /// Cue text.
    pub query: String,
    /// Result cap.
    #[arg(long, default_value_t = 10u32)]
    pub top_k: u32,
    /// Confidence threshold `[0.0, 1.0]`.
    #[arg(long, default_value_t = 0.0f32)]
    pub confidence: f32,
    /// Repeatable: keep only these context ids.
    #[arg(long = "filter-context")]
    pub filter_context: Vec<u64>,
    /// Repeatable: keep only these memory kinds.
    #[arg(long = "filter-kind", value_enum)]
    pub filter_kind: Vec<KindArg>,
    /// Populate the `text` column in the result table — the substrate
    /// adds one batched read against the metadata `texts` table.
    /// Off by default; recall returns ids and scores only.
    #[arg(long = "include-text", default_value_t = false)]
    pub include_text: bool,
    /// Bind to an active transaction (hex bytes).
    #[arg(long)]
    pub txn: Option<String>,
}

#[derive(Debug, Args, Clone)]
pub struct PlanArgs {
    /// Start state (text describing the start).
    pub from: String,
    /// Goal state (text describing the goal).
    pub to: String,
    /// Max plan steps.
    #[arg(long, default_value_t = 10u32)]
    pub max_steps: u32,
    /// Wall-time budget in milliseconds.
    #[arg(long, default_value_t = 5_000u32)]
    pub max_wall_time_ms: u32,
}

#[derive(Debug, Args, Clone)]
pub struct ReasonArgs {
    /// Observation text.
    pub observation: String,
    /// Reasoning depth.
    #[arg(long, default_value_t = 3u32)]
    pub depth: u32,
    /// Confidence threshold.
    #[arg(long, default_value_t = 0.0f32)]
    pub confidence: f32,
    /// Max inferences to return.
    #[arg(long, default_value_t = 16u32)]
    pub max_inferences: u32,
}

#[derive(Debug, Args, Clone)]
pub struct ForgetArgs {
    /// Memory id (hex `0x…` or decimal).
    pub id: MemoryIdArg,
    /// Soft tombstone vs hard erase.
    #[arg(long, value_enum, default_value_t = ForgetModeArg::Soft)]
    pub mode: ForgetModeArg,
}

#[derive(Debug, Args, Clone)]
pub struct LinkArgs {
    /// Source memory id.
    pub src: MemoryIdArg,
    /// Edge kind.
    #[arg(value_enum)]
    pub kind: EdgeKindArg,
    /// Target memory id.
    pub tgt: MemoryIdArg,
    /// Edge weight in `[0.0, 1.0]`.
    #[arg(long, default_value_t = 1.0f32)]
    pub weight: f32,
    /// Bind to an active transaction (hex bytes).
    #[arg(long)]
    pub txn: Option<String>,
}

#[derive(Debug, Args, Clone)]
pub struct UnlinkArgs {
    /// Source memory id.
    pub src: MemoryIdArg,
    /// Edge kind.
    #[arg(value_enum)]
    pub kind: EdgeKindArg,
    /// Target memory id.
    pub tgt: MemoryIdArg,
    /// Bind to an active transaction (hex bytes).
    #[arg(long)]
    pub txn: Option<String>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum TxnCommand {
    /// Open a new transaction.
    Begin,
    /// Commit a transaction by id (hex bytes).
    Commit { id: String },
    /// Abort a transaction by id (hex bytes).
    Abort { id: String },
}

/// Compatibility re-export so callers can pattern-match without
/// importing the variant tree.
#[derive(Debug, Args, Clone)]
pub struct TxnArgs {
    #[command(subcommand)]
    pub cmd: TxnCommand,
}

#[derive(Debug, Args, Clone)]
pub struct SubscribeArgs {
    /// Repeatable: subscribe only to these context ids.
    #[arg(long = "context")]
    pub context: Vec<u64>,
    /// Repeatable: subscribe only to these memory kinds.
    #[arg(long = "kind", value_enum)]
    pub kind: Vec<KindArg>,
    /// Resume from this LSN.
    #[arg(long)]
    pub start_lsn: Option<u64>,
    /// Collect exactly N events then exit. Required in v1 — live
    /// streaming with Ctrl-C cancellation lands post-v1.
    #[arg(long)]
    pub collect: Option<usize>,
}

#[derive(Debug, Args, Clone)]
pub struct GenerateCompletionArgs {
    /// Target shell (bash / zsh / fish / powershell / elvish).
    pub shell: Shell,
}

/// Parse a hex-encoded 16-byte transaction id (32 hex chars, with
/// or without a `0x` prefix).
pub fn parse_txn_id(s: &str) -> Result<[u8; 16], String> {
    let trimmed = s.trim();
    let hex = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    if hex.len() != 32 {
        return Err(format!(
            "txn id must be 32 hex characters (got {})",
            hex.len()
        ));
    }
    let mut out = [0u8; 16];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).map_err(|e| format!("invalid utf8: {e}"))?;
        out[i] = u8::from_str_radix(pair, 16).map_err(|e| format!("invalid hex: {e}"))?;
    }
    Ok(out)
}

/// Format a 16-byte transaction id as `0x…` hex.
#[must_use]
pub fn format_txn_id(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(34);
    s.push_str("0x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Parse the global `--server` value into a [`SocketAddr`].
pub fn parse_server(value: &str) -> Result<SocketAddr, String> {
    value
        .parse::<SocketAddr>()
        .map_err(|e| format!("invalid --server '{value}': {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once(&"brain").chain(args.iter()))
            .expect("parse should succeed")
    }

    #[test]
    fn encode_one_shot_parses() {
        let cli = parse(&["encode", "hello world", "--context", "7", "--salience", "0.8"]);
        match cli.subcommand {
            Some(Command::Encode(args)) => {
                assert_eq!(args.text, "hello world");
                assert_eq!(args.context, Some(7));
                assert_eq!(args.salience, Some(0.8));
            }
            other => panic!("expected Encode, got {other:?}"),
        }
    }

    #[test]
    fn recall_repeatable_filters() {
        let cli = parse(&[
            "recall",
            "auth",
            "--top-k",
            "5",
            "--filter-context",
            "1",
            "--filter-context",
            "2",
            "--filter-kind",
            "episodic",
            "--filter-kind",
            "semantic",
        ]);
        match cli.subcommand {
            Some(Command::Recall(args)) => {
                assert_eq!(args.top_k, 5);
                assert_eq!(args.filter_context, vec![1, 2]);
                assert_eq!(args.filter_kind, vec![KindArg::Episodic, KindArg::Semantic]);
            }
            other => panic!("expected Recall, got {other:?}"),
        }
    }

    #[test]
    fn link_takes_positional_edge_kind() {
        let cli = parse(&[
            "link",
            "0x10001000100000000",
            "supports",
            "0x20002000200000000",
            "--weight",
            "0.9",
        ]);
        match cli.subcommand {
            Some(Command::Link(args)) => {
                assert_eq!(args.src.0.raw(), 0x10001000100000000u128);
                assert_eq!(args.tgt.0.raw(), 0x20002000200000000u128);
                assert_eq!(args.kind, EdgeKindArg::Supports);
                assert!((args.weight - 0.9).abs() < 1e-6);
            }
            other => panic!("expected Link, got {other:?}"),
        }
    }

    #[test]
    fn forget_decimal_id() {
        let cli = parse(&["forget", "42", "--mode", "hard"]);
        match cli.subcommand {
            Some(Command::Forget(args)) => {
                assert_eq!(args.id.0.raw(), 42);
                assert_eq!(args.mode, ForgetModeArg::Hard);
            }
            other => panic!("expected Forget, got {other:?}"),
        }
    }

    #[test]
    fn txn_begin_subcommand() {
        let cli = parse(&["txn", "begin"]);
        match cli.subcommand {
            Some(Command::Txn(TxnCommand::Begin)) => {}
            other => panic!("expected Txn(Begin), got {other:?}"),
        }
    }

    #[test]
    fn no_subcommand_is_repl() {
        let cli = parse(&[]);
        assert!(cli.subcommand.is_none());
    }

    #[test]
    fn memory_id_hex_lower_and_upper() {
        let lo: MemoryIdArg = "0xabcd".parse().expect("hex parse");
        let up: MemoryIdArg = "0XABCD".parse().expect("hex parse");
        assert_eq!(lo.0.raw(), 0xabcd);
        assert_eq!(up.0.raw(), 0xabcd);
    }

    #[test]
    fn memory_id_decimal() {
        let d: MemoryIdArg = "42".parse().expect("decimal parse");
        assert_eq!(d.0.raw(), 42);
    }

    #[test]
    fn memory_id_short_form_round_trip() {
        let id = MemoryId::pack(2, 1, 1);
        let parsed: MemoryIdArg = "s2/m1/v1".parse().expect("short form parse");
        assert_eq!(parsed.0, id);
        assert_eq!(parsed.0.shard(), 2);
        assert_eq!(parsed.0.slot(), 1);
        assert_eq!(parsed.0.version(), 1);
    }

    #[test]
    fn memory_id_short_form_uppercase() {
        let parsed: MemoryIdArg = "S7/M42/V9".parse().expect("uppercase short form");
        assert_eq!(parsed.0.shard(), 7);
        assert_eq!(parsed.0.slot(), 42);
        assert_eq!(parsed.0.version(), 9);
    }

    #[test]
    fn memory_id_short_form_large_values() {
        let parsed: MemoryIdArg = "s65535/m1099511627775/v4294967295"
            .parse()
            .expect("max values");
        assert_eq!(parsed.0.shard(), u16::MAX);
        assert_eq!(parsed.0.slot(), 1_099_511_627_775); // 2^40 - 1, well under MAX_SLOT_INDEX
        assert_eq!(parsed.0.version(), u32::MAX);
    }

    #[test]
    fn memory_id_short_form_missing_version_rejected() {
        let err = "s2/m1".parse::<MemoryIdArg>().unwrap_err();
        assert!(err.contains("missing version"), "got: {err}");
    }

    #[test]
    fn memory_id_short_form_missing_slot_prefix_rejected() {
        let err = "s2/1/v1".parse::<MemoryIdArg>().unwrap_err();
        assert!(err.contains("slot must start with 'm'"), "got: {err}");
    }

    #[test]
    fn memory_id_short_form_missing_version_prefix_rejected() {
        let err = "s2/m1/1".parse::<MemoryIdArg>().unwrap_err();
        assert!(err.contains("version must start with 'v'"), "got: {err}");
    }

    #[test]
    fn memory_id_short_form_too_many_components_rejected() {
        let err = "s2/m1/v1/extra".parse::<MemoryIdArg>().unwrap_err();
        assert!(err.contains("too many components"), "got: {err}");
    }

    #[test]
    fn memory_id_short_form_non_numeric_shard_rejected() {
        let err = "sfoo/m1/v1".parse::<MemoryIdArg>().unwrap_err();
        assert!(err.contains("shard"), "got: {err}");
    }

    #[test]
    fn memory_id_garbage_rejected() {
        assert!("not-an-id".parse::<MemoryIdArg>().is_err());
        assert!("".parse::<MemoryIdArg>().is_err());
    }

    #[test]
    fn parse_txn_id_with_and_without_prefix() {
        let raw = "00112233445566778899aabbccddeeff";
        let a = parse_txn_id(raw).expect("parse");
        let b = parse_txn_id(&format!("0x{raw}")).expect("parse");
        assert_eq!(a, b);
        assert_eq!(a[0], 0x00);
        assert_eq!(a[15], 0xff);
    }

    #[test]
    fn parse_txn_id_wrong_length() {
        let e = parse_txn_id("deadbeef").unwrap_err();
        assert!(e.contains("32 hex characters"));
    }

    #[test]
    fn parse_server_ipv4() {
        let s = parse_server("127.0.0.1:9090").expect("addr");
        assert_eq!(s.to_string(), "127.0.0.1:9090");
    }
}
