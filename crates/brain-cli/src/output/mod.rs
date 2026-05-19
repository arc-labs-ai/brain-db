//! Admin-domain renderers + the dispatch helper.
//!
//! Each renderer impls [`brain_explore::Render`] so the admin CLI renders
//! identically to brain-shell: same colors, same OSC 8 behavior, same
//! `--output` matrix. Admin shapes live here (and not in brain-explore)
//! because they're consumed by exactly one binary; lifting them into the
//! shared library would force brain-explore to depend on admin SDK
//! surfaces that no other consumer needs.

pub mod render;

use std::io::{self, Write};

use brain_explore::{dispatch, OutputFormat, Render, RenderCtx, TermPolicy, Theme};

/// Run a renderer through brain-explore's [`dispatch`] and capture the
/// bytes as a `String`. Lets command functions return a rendered
/// `String` (preserving the old `Result<String>` test surface) while
/// the heavy lifting lives in brain-explore.
///
/// Uses [`TermPolicy::plain`] so output is deterministic across
/// terminals — the live process picks up real capabilities by building
/// a different policy in `main`.
pub fn dispatch_to_string(item: &dyn Render, format: OutputFormat) -> anyhow::Result<String> {
    let ctx = RenderCtx {
        policy: TermPolicy::plain(),
        theme: Theme::default(),
        format,
    };
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    dispatch(item, &ctx, &mut buf)?;
    Ok(String::from_utf8(buf)?)
}

/// Live dispatch into the supplied writer, using the resolved
/// [`TermPolicy`] from `main`. Used by the binary entry point so an
/// interactive terminal gets color + OSC 8 while pipes get ndjson.
pub fn dispatch_with_policy(
    item: &dyn Render,
    policy: TermPolicy,
    format: OutputFormat,
    w: &mut dyn Write,
) -> io::Result<()> {
    let ctx = RenderCtx {
        policy,
        theme: Theme::default(),
        format,
    };
    dispatch(item, &ctx, w)
}
