//! Diagnostic card for `\info` (REPL) / `brain info` (one-shot).
//!
//! Four sections: Server (cached handshake), Agent (resolved id +
//! source + config entry details), Connection (SDK state), Session
//! (shell-side preferences + REPL state).
//!
//! Renders cleanly when the server isn't connected — the card is
//! designed to answer both "what's my current state?" and "why did
//! that just fail?" without requiring a live connection.

use std::io::{self, Write};

use serde_json::{json, Value};

use crate::render::fmt_uuid;
use crate::theme::Token;
use crate::{Render, RenderCtx};

/// Width of the leading label column inside each section. Sized so
/// the longest label (`sticky_context`, `wire_version`, `connected_at`)
/// still leaves a comfortable gap before the value column.
const LABEL_WIDTH: usize = 16;

/// Full diagnostic snapshot. Constructed by brain-shell's
/// `commands::info::collect`; the renderer here owns only the layout.
pub struct InfoCard {
    pub server: ServerInfo,
    pub agent: AgentInfo,
    pub connection: ConnectionInfo,
    pub session: SessionInfo,
}

/// Server identity + capabilities, from the cached handshake.
/// `welcome = None` means the connection hasn't completed (or is
/// reconnecting) — the renderer surfaces "(not connected)" so the
/// operator can tell connection failure from a degenerate response.
#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    /// `host:port` the shell dialed (always known, even when
    /// disconnected).
    pub address: String,
    pub welcome: Option<ServerWelcomeFields>,
}

/// Subset of `WelcomePayload` + `AuthOkPayload` that's useful to
/// surface. Carries the negotiated wire version, server identity,
/// the bound shard, and the server's wall clock for skew detection.
#[derive(Debug, Clone)]
pub struct ServerWelcomeFields {
    pub server_id: String,
    pub wire_version: u8,
    pub server_time_unix_nanos: u64,
    pub bound_shard: u16,
    pub streaming: bool,
    pub compression_zstd: bool,
    pub server_push: bool,
}

/// Resolved agent. brain-shell already does the resolution at session
/// start; we just reflect what it picked.
#[derive(Debug, Clone)]
pub struct AgentInfo {
    /// `Some` for named sources (config, env, flag with a name);
    /// `None` for raw-id paths (`--agent-id` / `BRAIN_AGENT_ID`) and
    /// ephemeral binds where there is no name to display.
    pub name: Option<String>,
    pub agent_id: [u8; 16],
    /// Human-readable source label, e.g. `config: active` or
    /// `--agent flag`. Drives the connect banner too; we mirror its
    /// wording here so both surfaces agree.
    pub source_label: String,
    /// `true` when this agent is the file's `default = true`.
    /// Surfaces because the factory-fallback identity is worth
    /// noting in diagnostics — losing your default is a common
    /// "why did the next session pick a different agent?" gotcha.
    pub default: bool,
    pub note: String,
    pub created_at: Option<String>,
}

/// SDK connection state. Two values matter: whether the handshake
/// completed, and (best-effort) when. The shell already knows the
/// address from `session.server`, so we don't duplicate it here.
#[derive(Debug, Clone, Default)]
pub struct ConnectionInfo {
    pub authenticated: bool,
    pub connected_at_unix_nanos: Option<u64>,
}

/// Shell-side session preferences + REPL state. All in-memory and
/// non-blocking to collect; the values surface here so an operator
/// debugging "why did that command behave that way?" can verify the
/// sticky context / active txn / timing flag in one place.
#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    pub output: String,
    pub sticky_context: Option<u64>,
    pub active_txn: Option<String>,
    pub timing: bool,
}

impl Render for InfoCard {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let policy = ctx.policy;
        let theme = &ctx.theme;

        // ── Server ────────────────────────────────────────────────
        writeln!(w, "{}", theme.paint(Token::Accent, "Server", policy))?;
        write_row(w, theme, policy, "address", &self.server.address)?;
        match &self.server.welcome {
            Some(welcome) => {
                write_row(w, theme, policy, "server_id", &welcome.server_id)?;
                write_row(
                    w,
                    theme,
                    policy,
                    "wire_version",
                    &welcome.wire_version.to_string(),
                )?;
                let server_time = format!("{} unix-nanos", welcome.server_time_unix_nanos);
                write_row(w, theme, policy, "server_time", &server_time)?;
                write_row(
                    w,
                    theme,
                    policy,
                    "bound_shard",
                    &welcome.bound_shard.to_string(),
                )?;
                let mut caps: Vec<&str> = Vec::new();
                if welcome.streaming {
                    caps.push("streaming");
                }
                if welcome.compression_zstd {
                    caps.push("zstd");
                }
                if welcome.server_push {
                    caps.push("push");
                }
                let caps_str = if caps.is_empty() {
                    "(none)".to_string()
                } else {
                    caps.join(", ")
                };
                write_row(w, theme, policy, "capabilities", &caps_str)?;
            }
            None => {
                let muted = theme.paint(Token::Muted, "(not connected)", policy);
                writeln!(w, "  {muted}")?;
            }
        }
        writeln!(w)?;

        // ── Agent ─────────────────────────────────────────────────
        writeln!(w, "{}", theme.paint(Token::Accent, "Agent", policy))?;
        if let Some(name) = &self.agent.name {
            write_row(w, theme, policy, "name", name)?;
        }
        write_row(w, theme, policy, "id", &fmt_uuid(&self.agent.agent_id))?;
        write_row(w, theme, policy, "source", &self.agent.source_label)?;
        write_row(
            w,
            theme,
            policy,
            "default",
            if self.agent.default { "yes" } else { "no" },
        )?;
        if !self.agent.note.is_empty() {
            write_row(w, theme, policy, "note", &self.agent.note)?;
        }
        if let Some(ts) = &self.agent.created_at {
            write_row(w, theme, policy, "created_at", ts)?;
        }
        writeln!(w)?;

        // ── Connection ────────────────────────────────────────────
        writeln!(w, "{}", theme.paint(Token::Accent, "Connection", policy))?;
        if self.connection.authenticated {
            write_row(w, theme, policy, "state", "authenticated")?;
        } else {
            let padded = pad_label("state");
            let label = theme.paint(Token::Label, &padded, policy);
            let muted = theme.paint(Token::Muted, "(not connected)", policy);
            writeln!(w, "  {label}  {muted}")?;
        }
        if let Some(ts) = self.connection.connected_at_unix_nanos {
            let formatted = format!("{ts} unix-nanos");
            write_row(w, theme, policy, "connected_at", &formatted)?;
        }
        writeln!(w)?;

        // ── Session ───────────────────────────────────────────────
        writeln!(w, "{}", theme.paint(Token::Accent, "Session", policy))?;
        write_row(w, theme, policy, "output", &self.session.output)?;
        let ctx_str = self
            .session
            .sticky_context
            .map(|c| c.to_string())
            .unwrap_or_else(|| "(none)".to_string());
        write_row(w, theme, policy, "sticky_context", &ctx_str)?;
        let txn_str = self
            .session
            .active_txn
            .clone()
            .unwrap_or_else(|| "none".to_string());
        write_row(w, theme, policy, "active_txn", &txn_str)?;
        write_row(
            w,
            theme,
            policy,
            "timing",
            if self.session.timing { "on" } else { "off" },
        )?;

        Ok(())
    }

    fn render_json(&self, _ctx: &RenderCtx) -> Value {
        json!({
            "server": {
                "address": self.server.address,
                "welcome": self.server.welcome.as_ref().map(|w| json!({
                    "server_id": w.server_id,
                    "wire_version": w.wire_version,
                    "server_time_unix_nanos": w.server_time_unix_nanos,
                    "bound_shard": w.bound_shard,
                    "capabilities": {
                        "streaming": w.streaming,
                        "compression_zstd": w.compression_zstd,
                        "server_push": w.server_push,
                    },
                })),
            },
            "agent": {
                "name": self.agent.name,
                "id": fmt_uuid(&self.agent.agent_id),
                "source": self.agent.source_label,
                "default": self.agent.default,
                "note": self.agent.note,
                "created_at": self.agent.created_at,
            },
            "connection": {
                "authenticated": self.connection.authenticated,
                "connected_at_unix_nanos": self.connection.connected_at_unix_nanos,
            },
            "session": {
                "output": self.session.output,
                "sticky_context": self.session.sticky_context,
                "active_txn": self.session.active_txn,
                "timing": self.session.timing,
            },
        })
    }
}

/// Format the left label column. Pulling the padding into a helper
/// keeps the renderer body readable and ensures every section uses
/// the same column width without each call site repeating the
/// `{:<LABEL_WIDTH$}` format spec.
fn pad_label(label: &str) -> String {
    format!("{label:<LABEL_WIDTH$}")
}

/// Write one `  label  value` row. The label gets the `Label` token,
/// the value gets `Value`; padding happens before paint so the ANSI
/// escapes don't throw off the column alignment.
fn write_row(
    w: &mut dyn Write,
    theme: &crate::theme::Theme,
    policy: crate::TermPolicy,
    label: &str,
    value: &str,
) -> io::Result<()> {
    let padded = pad_label(label);
    let label_painted = theme.paint(Token::Label, &padded, policy);
    let value_painted = theme.paint(Token::Value, value, policy);
    writeln!(w, "  {label_painted}  {value_painted}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::OutputFormat;
    use crate::theme::Theme;
    use crate::TermPolicy;

    fn render(card: &InfoCard, format: OutputFormat) -> String {
        let ctx = RenderCtx {
            policy: TermPolicy::plain(),
            theme: Theme::default(),
            format,
        };
        let mut buf = Vec::new();
        crate::dispatch(card, &ctx, &mut buf).expect("render");
        String::from_utf8(buf).expect("utf8")
    }

    fn baseline() -> InfoCard {
        InfoCard {
            server: ServerInfo {
                address: "127.0.0.1:9090".into(),
                welcome: None,
            },
            agent: AgentInfo {
                name: Some("agent-01927a8b".into()),
                agent_id: [
                    0x01, 0x92, 0x7a, 0x8b, 0x4c, 0x2f, 0x70, 0x00, 0x80, 0x00, 0xde, 0xad, 0xbe,
                    0xef, 0xfe, 0xed,
                ],
                source_label: "config: active".into(),
                default: false,
                note: String::new(),
                created_at: Some("2026-05-20T02:00:00Z".into()),
            },
            connection: ConnectionInfo {
                authenticated: false,
                connected_at_unix_nanos: None,
            },
            session: SessionInfo {
                output: "wide".into(),
                sticky_context: Some(7),
                active_txn: None,
                timing: false,
            },
        }
    }

    #[test]
    fn render_table_shows_all_four_sections() {
        let out = render(&baseline(), OutputFormat::Table);
        assert!(out.contains("Server"));
        assert!(out.contains("Agent"));
        assert!(out.contains("Connection"));
        assert!(out.contains("Session"));
    }

    #[test]
    fn render_table_disconnected_state_is_clean() {
        let out = render(&baseline(), OutputFormat::Table);
        assert!(
            out.contains("(not connected)"),
            "disconnected server must say so: {out}"
        );
    }

    #[test]
    fn render_table_authenticated_state_shows_welcome_fields() {
        let mut card = baseline();
        card.server.welcome = Some(ServerWelcomeFields {
            server_id: "brain-server/1.0.0".into(),
            wire_version: 2,
            server_time_unix_nanos: 1_700_000_000_000_000_000,
            bound_shard: 2,
            streaming: true,
            compression_zstd: false,
            server_push: false,
        });
        card.connection.authenticated = true;
        let out = render(&card, OutputFormat::Table);
        assert!(out.contains("brain-server/1.0.0"));
        assert!(out.contains("wire_version"));
        assert!(out.contains("bound_shard"));
        assert!(out.contains("streaming"));
        assert!(out.contains("authenticated"));
    }

    #[test]
    fn render_table_full_agent_uuid_appears() {
        let out = render(&baseline(), OutputFormat::Table);
        assert!(
            out.contains("01927a8b-4c2f-7000-8000-deadbeeffeed"),
            "agent uuid must render in canonical dashed form: {out}"
        );
    }

    #[test]
    fn render_json_envelope_shape() {
        let out = render(&baseline(), OutputFormat::Json);
        let v: Value = serde_json::from_str(&out).expect("parse json");
        assert!(v["server"].is_object());
        assert!(v["agent"].is_object());
        assert!(v["connection"].is_object());
        assert!(v["session"].is_object());
        // Disconnected → welcome is null. JSON consumers branch on
        // this to decide whether the rest of the server block is
        // meaningful.
        assert!(v["server"]["welcome"].is_null());
        assert_eq!(v["agent"]["id"], "01927a8b-4c2f-7000-8000-deadbeeffeed");
    }
}
