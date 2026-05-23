//! LINK / UNLINK response renderers.

use std::io::{self, Write};

use brain_protocol::envelope::response::{LinkResponse, UnlinkResponse};
use serde_json::{json, Value};

use crate::render::{fmt_edge_kind, fmt_id};
use crate::theme::Token;
use crate::{Render, RenderCtx};

/// Renderer wrapper for `LINK_RESP`.
pub struct LinkRendered(pub LinkResponse);

/// Renderer wrapper for `UNLINK_RESP`.
pub struct UnlinkRendered(pub UnlinkResponse);

impl Render for LinkRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let r = &self.0;
        let ok = ctx.theme.paint(Token::Success, "ok", ctx.policy);
        let kind = ctx
            .theme
            .paint(Token::Predicate, fmt_edge_kind(r.kind), ctx.policy);
        writeln!(
            w,
            "{ok}  {} --[{kind}]--> {}  weight={:.4}  already_existed={}",
            fmt_id(r.source),
            fmt_id(r.target),
            r.weight,
            r.already_existed,
        )
    }

    fn render_json(&self, _ctx: &RenderCtx) -> Value {
        let r = &self.0;
        json!({
            "source": fmt_id(r.source),
            "target": fmt_id(r.target),
            "kind": fmt_edge_kind(r.kind),
            "weight": r.weight,
            "created_at_unix_nanos": r.created_at_unix_nanos,
            "already_existed": r.already_existed,
        })
    }
}

impl Render for UnlinkRendered {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let r = &self.0;
        let ok = ctx.theme.paint(Token::Success, "ok", ctx.policy);
        let kind = ctx
            .theme
            .paint(Token::Predicate, fmt_edge_kind(r.kind), ctx.policy);
        writeln!(
            w,
            "{ok}  {} --[{kind}]--> {}  removed={}",
            fmt_id(r.source),
            fmt_id(r.target),
            r.removed,
        )
    }

    fn render_json(&self, _ctx: &RenderCtx) -> Value {
        let r = &self.0;
        json!({
            "source": fmt_id(r.source),
            "target": fmt_id(r.target),
            "kind": fmt_edge_kind(r.kind),
            "removed": r.removed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::OutputFormat;
    use crate::theme::Theme;
    use crate::TermPolicy;
    use brain_protocol::envelope::request::EdgeKindWire;

    fn ctx() -> RenderCtx {
        RenderCtx {
            policy: TermPolicy::plain(),
            theme: Theme::default(),
            format: OutputFormat::Table,
        }
    }

    #[test]
    fn link_table_shows_arrow_and_weight() {
        let r = LinkRendered(LinkResponse {
            source: 0x1,
            target: 0x2,
            kind: EdgeKindWire::Caused,
            weight: 0.5,
            created_at_unix_nanos: 0,
            already_existed: false,
        });
        let mut buf = Vec::new();
        r.render_table(&ctx(), &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Caused"));
        assert!(s.contains("weight=0.5000"));
        assert!(s.contains("already_existed=false"));
    }

    #[test]
    fn unlink_table_shows_removed_flag() {
        let r = UnlinkRendered(UnlinkResponse {
            source: 0x1,
            target: 0x2,
            kind: EdgeKindWire::FollowedBy,
            removed: true,
        });
        let mut buf = Vec::new();
        r.render_table(&ctx(), &mut buf).unwrap();
        assert!(String::from_utf8(buf).unwrap().contains("removed=true"));
    }

    #[test]
    fn link_json_carries_kind_name() {
        let r = LinkRendered(LinkResponse {
            source: 0x1,
            target: 0x2,
            kind: EdgeKindWire::Caused,
            weight: 0.5,
            created_at_unix_nanos: 0,
            already_existed: false,
        });
        let v = r.render_json(&ctx());
        assert_eq!(v["kind"], "Caused");
    }
}
