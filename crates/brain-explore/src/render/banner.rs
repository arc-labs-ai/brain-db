//! REPL connect-banner card.
//!
//! Replaces the previous three-line print sequence (`brain shell —
//! connected …` + first-run note + help hint) with a single Render
//! impl that matches the card style used by the encode renderer:
//! framed top + bottom, dot brand icon + version + server on the
//! title line, columnar agent block below, footer hint.
//!
//! The banner consciously drops the multi-row ASCII logo idea
//! (terminal characters can't render the hex SVG faithfully at any
//! size) in favour of a single ◉ glyph that nods to the SVG's
//! center-with-aperture detail.

use std::io::{self, Write};

use serde_json::{json, Value};

use crate::render::fmt_uuid;
use crate::theme::Token;
use crate::{Render, RenderCtx};

/// Source the resolved agent came from, surfaced in the banner's
/// agent block. The shell maps its own AgentIdSource into one of
/// these variants — keeps brain-explore independent of the
/// AgentIdSource enum's internals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BannerAgentSource {
    /// `--agent <name>` flag — name is the named lookup key.
    NamedFlag(String),
    /// `--agent-id <uuid>` flag — no config-side name.
    IdFlag,
    /// `BRAIN_AGENT=<name>` env var.
    NamedEnv(String),
    /// `BRAIN_AGENT_ID=<uuid>` env var.
    IdEnv,
    /// Picked from config because `active = true` was set.
    ActiveFromConfig {
        name: String,
        file_display: String,
    },
    /// Picked from config because `default = true` and no active.
    DefaultFromConfig {
        name: String,
        file_display: String,
    },
    /// First-run auto-mint — the shell just created and persisted
    /// this agent. `file_display` is the path the new config was
    /// written to so the user knows where their state lives.
    AutoMinted {
        name: String,
        file_display: String,
    },
    /// In-memory ephemeral (no HOME, no config). Rare.
    Ephemeral,
}

/// Welcome banner rendered once at REPL start. All values are owned
/// strings so the shell can build the struct before any I/O happens
/// and pass it to `dispatch` like any other render payload.
pub struct WelcomeBanner {
    /// Display name like `"brain-shell"` or `"brain"`. Shown right
    /// after the brand icon.
    pub product_name: String,
    /// Cargo package version string.
    pub version: String,
    /// Server address the SDK is bound to, formatted as `host:port`.
    pub server_addr: String,
    /// Agent name when one was resolved (NamedFlag, NamedEnv,
    /// ActiveFromConfig, DefaultFromConfig, AutoMinted). None for
    /// raw-id sources and Ephemeral.
    pub agent_name: Option<String>,
    /// Full agent UUID bytes — rendered canonical dashed form.
    pub agent_id: [u8; 16],
    /// Where the agent came from. Drives the muted source annotation
    /// under the UUID.
    pub agent_source: BannerAgentSource,
}

impl Render for WelcomeBanner {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let policy = ctx.policy;
        let theme = &ctx.theme;
        let lbl = |s: &str| theme.paint(Token::Label, s, policy).into_owned();
        let val = |s: &str| theme.paint(Token::Value, s, policy).into_owned();
        let muted = |s: &str| theme.paint(Token::Muted, s, policy).into_owned();
        let accent = |s: &str| theme.paint(Token::Accent, s, policy).into_owned();

        // ── Top rule ──────────────────────────────────────────────
        let width = policy.width.min(80).max(40);
        let rule: String = "─".repeat(width);
        writeln!(w, "{}", muted(&rule))?;
        writeln!(w)?;

        // ── Title line ────────────────────────────────────────────
        // ◉ brand icon + product name + version + · + connection.
        // The icon is the only nod to the SVG logo; everything else
        // is text. Mature CLIs (psql, redis-cli, gh) don't ASCII-art
        // their brands — the SVG lives in docs where it renders
        // correctly.
        let icon = accent("◉");
        let product = val(&self.product_name);
        let ver = muted(&format!("v{}", self.version));
        let sep = muted("·");
        let conn_label = muted("connected to");
        let server = val(&self.server_addr);
        writeln!(
            w,
            "  {icon} {product}  {ver}  {sep}  {conn_label} {server}"
        )?;
        writeln!(w)?;

        // ── Agent block ──────────────────────────────────────────
        // Three sub-rows under one `agent` label column:
        //   line 1: name (or "(raw id)" for IdFlag / IdEnv, or
        //           "(ephemeral)" for Ephemeral)
        //   line 2: canonical UUID
        //   line 3: muted source annotation
        let agent_label = lbl(&format!("{:<10}", "agent"));
        let agent_display_name = match (&self.agent_name, &self.agent_source) {
            (Some(name), _) => val(name),
            (None, BannerAgentSource::IdFlag) => muted("(raw id from --agent-id)"),
            (None, BannerAgentSource::IdEnv) => muted("(raw id from BRAIN_AGENT_ID)"),
            (None, BannerAgentSource::Ephemeral) => muted("(ephemeral)"),
            (None, _) => muted("(unknown)"),
        };
        writeln!(w, "  {agent_label}  {agent_display_name}")?;

        // UUID row — empty label, value under the agent column.
        let blank_label: String = " ".repeat(10);
        let uuid = val(&fmt_uuid(&self.agent_id));
        writeln!(w, "  {blank_label}  {uuid}")?;

        // Source annotation row — muted, sits under the UUID.
        let source_text = source_annotation(&self.agent_source);
        writeln!(w, "  {blank_label}  {}", muted(&source_text))?;

        writeln!(w)?;

        // ── Footer hint ──────────────────────────────────────────
        let hint = muted("Type `help` for commands, `quit` to exit.");
        writeln!(w, "  {hint}")?;

        // ── Bottom rule ──────────────────────────────────────────
        writeln!(w, "{}", muted(&rule))?;
        Ok(())
    }

    fn render_json(&self, _ctx: &RenderCtx) -> Value {
        json!({
            "product_name": self.product_name,
            "version": self.version,
            "server_addr": self.server_addr,
            "agent": {
                "name": self.agent_name,
                "id": fmt_uuid(&self.agent_id),
                "source": match &self.agent_source {
                    BannerAgentSource::NamedFlag(_) => "named-flag",
                    BannerAgentSource::IdFlag => "id-flag",
                    BannerAgentSource::NamedEnv(_) => "named-env",
                    BannerAgentSource::IdEnv => "id-env",
                    BannerAgentSource::ActiveFromConfig { .. } => "config-active",
                    BannerAgentSource::DefaultFromConfig { .. } => "config-default",
                    BannerAgentSource::AutoMinted { .. } => "auto-minted",
                    BannerAgentSource::Ephemeral => "ephemeral",
                },
            },
        })
    }
}

/// Human-readable single-line annotation for the agent's source.
/// Render-internal helper; sits under the UUID in the banner.
fn source_annotation(source: &BannerAgentSource) -> String {
    match source {
        BannerAgentSource::NamedFlag(name) => format!("--agent {name}"),
        BannerAgentSource::IdFlag => "--agent-id <uuid>".to_string(),
        BannerAgentSource::NamedEnv(name) => format!("BRAIN_AGENT={name}"),
        BannerAgentSource::IdEnv => "BRAIN_AGENT_ID=<uuid>".to_string(),
        BannerAgentSource::ActiveFromConfig { name, .. } => {
            format!("config: active = {name}")
        }
        BannerAgentSource::DefaultFromConfig { name, .. } => {
            format!("config: default = {name}")
        }
        BannerAgentSource::AutoMinted {
            name: _,
            file_display,
        } => {
            // First-run case absorbs the "first run" note that used
            // to print on its own line. Folding it into the agent
            // block keeps the banner to a single visual unit. The
            // name itself is already in the UUID label row above.
            format!("auto-minted on first run · stored at {file_display}")
        }
        BannerAgentSource::Ephemeral => {
            "ephemeral (no config file available)".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::OutputFormat;
    use crate::theme::Theme;
    use crate::TermPolicy;

    fn render(banner: &WelcomeBanner, format: OutputFormat) -> String {
        let ctx = RenderCtx {
            policy: TermPolicy::plain(),
            theme: Theme::default(),
            format,
        };
        let mut buf = Vec::new();
        crate::dispatch(banner, &ctx, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn sample() -> WelcomeBanner {
        WelcomeBanner {
            product_name: "brain-shell".into(),
            version: "1.0.0".into(),
            server_addr: "127.0.0.1:9090".into(),
            agent_name: Some("agent-019e433e".into()),
            agent_id: [
                0x01, 0x9e, 0x43, 0x3e, 0x92, 0x72, 0x7e, 0x70, 0xb0, 0x71, 0x4a, 0x4f, 0xd6, 0x13,
                0x5d, 0x1e,
            ],
            agent_source: BannerAgentSource::AutoMinted {
                name: "agent-019e433e".into(),
                file_display: "/root/.config/brain/config.toml".into(),
            },
        }
    }

    #[test]
    fn render_table_includes_brand_icon_product_and_server() {
        let out = render(&sample(), OutputFormat::Table);
        assert!(out.contains("◉"), "missing brand icon: {out}");
        assert!(out.contains("brain-shell"), "missing product name: {out}");
        assert!(out.contains("v1.0.0"), "missing version: {out}");
        assert!(
            out.contains("connected to 127.0.0.1:9090"),
            "missing server: {out}"
        );
    }

    #[test]
    fn render_table_shows_agent_name_and_full_uuid() {
        let out = render(&sample(), OutputFormat::Table);
        assert!(out.contains("agent-019e433e"), "missing agent name: {out}");
        // Canonical dashed UUID — same form fmt_uuid produces.
        assert!(
            out.contains("019e433e-9272-7e70-b071-4a4fd6135d1e"),
            "missing canonical UUID: {out}"
        );
    }

    #[test]
    fn render_table_first_run_annotation_includes_config_path() {
        let out = render(&sample(), OutputFormat::Table);
        assert!(
            out.contains("auto-minted on first run"),
            "missing first-run annotation: {out}"
        );
        assert!(
            out.contains("/root/.config/brain/config.toml"),
            "missing config path: {out}"
        );
    }

    #[test]
    fn render_table_active_source_uses_config_label() {
        let mut banner = sample();
        banner.agent_source = BannerAgentSource::ActiveFromConfig {
            name: "work".into(),
            file_display: "/root/.config/brain/config.toml".into(),
        };
        banner.agent_name = Some("work".into());
        let out = render(&banner, OutputFormat::Table);
        assert!(
            out.contains("config: active = work"),
            "missing active-source label: {out}"
        );
        // Should NOT mention auto-mint when source is config:active.
        assert!(
            !out.contains("auto-minted"),
            "must not leak auto-mint label: {out}"
        );
    }

    #[test]
    fn render_table_id_flag_uses_raw_id_label() {
        let mut banner = sample();
        banner.agent_source = BannerAgentSource::IdFlag;
        banner.agent_name = None;
        let out = render(&banner, OutputFormat::Table);
        assert!(out.contains("(raw id from --agent-id)"));
        assert!(out.contains("--agent-id <uuid>"));
    }

    #[test]
    fn render_table_ephemeral_is_marked_clearly() {
        let mut banner = sample();
        banner.agent_source = BannerAgentSource::Ephemeral;
        banner.agent_name = None;
        let out = render(&banner, OutputFormat::Table);
        assert!(out.contains("(ephemeral)"));
        assert!(out.contains("no config file available"));
    }

    #[test]
    fn render_table_has_top_and_bottom_rules() {
        let out = render(&sample(), OutputFormat::Table);
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines.first().unwrap_or(&"").contains("─"),
            "missing top rule"
        );
        assert!(
            lines.last().unwrap_or(&"").contains("─"),
            "missing bottom rule"
        );
    }

    #[test]
    fn render_table_includes_help_hint() {
        let out = render(&sample(), OutputFormat::Table);
        assert!(
            out.contains("Type `help` for commands"),
            "missing help hint: {out}"
        );
    }

    #[test]
    fn render_json_envelope_shape() {
        let out = render(&sample(), OutputFormat::Json);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["product_name"], "brain-shell");
        assert_eq!(v["agent"]["source"], "auto-minted");
        assert_eq!(v["agent"]["name"], "agent-019e433e");
    }
}
