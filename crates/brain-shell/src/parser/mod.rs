//! Shared argv parser. The same `clap` tree drives both one-shot
//! argv and per-line REPL input.

pub mod command;
pub mod help_intent;
pub mod tokenize;

pub use help_intent::{detect_help_intent, HelpIntent};

pub use command::{
    format_txn_id, parse_server, parse_txn_id, AgentCommand, Cli, ColorMode, Command,
    ConfigCommand, EdgeKindArg, EdgeSpec, EncodeArgs, EntityCommand, EntityListArgs,
    EntityNeighborsArgs, EntityShowArgs, ExtractBackfillArgs, ExtractCommand, ExtractStatusArgs,
    ForgetArgs, ForgetModeArg, GenerateCompletionArgs, GlobalOpts, HyperlinkMode, KindArg,
    LinkArgs, MemoryIdArg, MentionCommand, MentionListArgs, OutputFormatArg, PlanArgs, ReasonArgs,
    RecallArgs, RelationCommand, RelationListArgs, StatementCommand, StatementListArgs,
    StatementShowArgs, SubscribeArgs, TxnArgs, TxnCommand, UnlinkArgs,
};
pub use tokenize::{tokenize_line, TokenizeError};
