//! Shared argv parser. The same `clap` tree drives both one-shot
//! argv and per-line REPL input.

pub mod command;
pub mod tokenize;

pub use command::{
    format_txn_id, parse_server, parse_txn_id, AgentCommand, Cli, Command, ConfigCommand,
    EdgeKindArg, EncodeArgs, ForgetArgs, ForgetModeArg, GenerateCompletionArgs, GlobalOpts,
    KindArg, LinkArgs, MemoryIdArg, OutputFormatArg, PlanArgs, ReasonArgs, RecallArgs,
    SubscribeArgs, TxnArgs, TxnCommand, UnlinkArgs,
};
pub use tokenize::{tokenize_line, TokenizeError};
