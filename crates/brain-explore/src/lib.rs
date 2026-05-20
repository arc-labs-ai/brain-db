//! brain-explore — terminal UI/UX layer for Brain.
//!
//! Used by both `brain-shell` (the user-facing REPL/CLI) and `brain-cli`
//! (the admin/operator tool) so both binaries render with identical look,
//! flags, and policy. Owning color, hyperlink, pager, and table conventions
//! in one place is the only way to keep two CLIs from drifting apart over
//! time.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub mod format;
pub mod render;
pub mod table;
pub mod term;
pub mod theme;
#[cfg(feature = "tui")]
pub mod tui;
pub mod util;

pub use format::{dispatch, OutputFormat, Render, RenderCtx};
pub use render::{
    AdHocTable, AgentInfo, AuditCard, BannerAgentSource, ConnectionInfo, EncodeRendered,
    EntityCard, GraphEnrichment, GraphNode, GraphTree, HelpFlagRow, HelpItem, HelpReference,
    HelpSection, HelpTopLevel, HelpUnknown, HelpVerb, InfoCard, LinkRendered, MemorySummary,
    ObjectRef, PlanSteps, ReasonSteps, RecallResults, RecallWithGraph, RelationCard,
    RelationSummary, RenderableError, ServerInfo, ServerWelcomeFields, SessionInfo, StatementCard,
    StatementSummary, SubscriptionEventList, SubscriptionEventRendered, TxnAbortRendered,
    TxnBeginRendered, TxnCommitRendered, UnlinkRendered, WelcomeBanner,
};
pub use term::policy::TermPolicy;
pub use theme::{Theme, Token};
