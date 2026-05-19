//! `subscribe` verb — two modes.
//!
//! - `subscribe --collect <N>` — batch wait. Returns after N events
//!   have arrived. The collected list is rendered by
//!   [`SubscriptionEventList`]. Useful in tests + scripts.
//! - `subscribe` (no `--collect`) — real-time stream. Events render
//!   one at a time as they arrive; the loop exits on Ctrl-C or
//!   SIGTERM, sending `UnsubscribeRequest` to the server first so
//!   the registry entry doesn't leak.
//!
//! `--start-lsn N` makes the server replay any historical events
//! with `lsn >= N` from the WAL before joining the live tail. The
//! cutover is invisible to the client (no gap, no dupes). The
//! server rejects values below the oldest available LSN with
//! `SubscriptionLsnTooOld`.

use std::io::{self, Write};

use brain_explore::{
    dispatch, Render, RenderCtx, SubscriptionEventList, SubscriptionEventRendered,
};
use brain_protocol::response::SubscriptionEvent;
use brain_sdk_rust::{Client, ClientError};
use futures_lite::StreamExt;
use serde_json::Value;

use crate::commands::render_ctx;
use crate::parser::{OutputFormatArg, SubscribeArgs};
use crate::session::Session;

use super::Rendered;

/// Treat anything that isn't a JSON / yaml / jsonpath family as
/// human-table output for the purposes of streaming banners.
#[must_use]
fn is_human_output(o: &OutputFormatArg) -> bool {
    matches!(
        o,
        OutputFormatArg::Auto | OutputFormatArg::Table | OutputFormatArg::Wide
    )
}

pub async fn run(
    client: &Client,
    session: &mut Session,
    args: SubscribeArgs,
) -> Result<Rendered, ClientError> {
    let mut b = client.subscribe();
    if let Some(lsn) = args.start_lsn {
        b = b.start_lsn(lsn);
    }
    if !args.context.is_empty() {
        b = b.contexts(args.context);
    }
    if !args.kind.is_empty() {
        let kinds = args.kind.into_iter().map(|k| k.into_wire()).collect();
        b = b.kinds(kinds);
    }

    match args.collect {
        Some(n) => {
            // Batch path. SDK collect already drops the stream on
            // completion; the new FrameStream Drop impl marks the
            // guard failed if EOS wasn't observed, so even an early
            // server-side close won't poison the pool.
            let events = b.collect(n).await?;
            Ok(Box::new(SubscriptionEventList(events)))
        }
        None => {
            // The streaming path renders events as they arrive — by
            // the time we return there's nothing left to dispatch. The
            // sentinel below makes the outer dispatch loop a no-op.
            stream_until_signal(client, b, session).await?;
            Ok(Box::new(AlreadyRendered))
        }
    }
}

/// Real-time loop. Renders events as they arrive; cleans up on
/// SIGINT / SIGTERM. Returns once the stream ends, the loop is
/// signalled, or the stream errors.
async fn stream_until_signal(
    client: &Client,
    builder: brain_sdk_rust::SubscribeBuilder<'_>,
    session: &Session,
) -> Result<(), ClientError> {
    let mut stream = builder.send_stream().await?;
    let stream_id = stream.stream_id();

    let output = session.output.clone();
    if is_human_output(&output) {
        let _ = writeln!(io::stderr(), "subscribed — Ctrl-C to stop");
        let _ = io::stderr().flush();
    }

    // Build the render context once. The streaming loop reuses it for
    // every event so OSC 8 / NO_COLOR / width handling stays consistent.
    // Force ndjson when the user picked JSON / NDJSON / YAML — pretty
    // JSON or YAML buffer poorly across event boundaries; ndjson is
    // the right shape for any structured stream.
    let stream_format = match output.clone() {
        OutputFormatArg::Json | OutputFormatArg::Ndjson | OutputFormatArg::Yaml => {
            OutputFormatArg::Ndjson
        }
        OutputFormatArg::JsonPath(_) => OutputFormatArg::Ndjson,
        other => other,
    };
    let ctx = render_ctx(
        stream_format,
        crate::parser::ColorMode::Auto,
        crate::parser::HyperlinkMode::Auto,
    );

    // Persistent signal receivers — installed ONCE outside the
    // loop. Using `tokio::signal::ctrl_c()` per-iteration drops
    // its internal registration between iterations and loses
    // signals that arrive in the gap (observed: users had to
    // press Ctrl-C four times to break out).
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|e| ClientError::Internal(format!("SIGINT handler: {e}")))?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| ClientError::Internal(format!("SIGTERM handler: {e}")))?;

    let mut count: u64 = 0;
    let mut stream_err: Option<ClientError> = None;
    let mut server_closed = false;
    let mut signalled = false;

    loop {
        tokio::select! {
            biased;
            _ = sigint.recv() => {
                signalled = true;
                break;
            }
            _ = sigterm.recv() => {
                signalled = true;
                break;
            }
            ev = stream.next() => match ev {
                Some(Ok(event)) => {
                    if let Err(e) = render_stream_event(&event, &ctx) {
                        // stdout closed (broken pipe): exit quietly.
                        if e.kind() == io::ErrorKind::BrokenPipe {
                            break;
                        }
                        return Err(ClientError::Internal(format!("render: {e}")));
                    }
                    count += 1;
                }
                Some(Err(e)) => {
                    stream_err = Some(e);
                    break;
                }
                None => {
                    server_closed = true;
                    break;
                }
            },
        }
    }

    // Tell the user RIGHT NOW that we heard the signal — the
    // server-side cleanup below can take a moment (unsubscribe RPC
    // + drop of in-flight frames), and the worst UX is staring at
    // a dead prompt wondering whether Ctrl-C was registered.
    if signalled && is_human_output(&output) {
        let _ = writeln!(io::stderr(), "\nclosing stream…");
        let _ = io::stderr().flush();
    }

    // Best-effort unsubscribe so the server doesn't keep the
    // registry entry alive. Capped at 2s so a hung server can't
    // pin us — a second Ctrl-C from the user shouldn't be needed.
    // Skip entirely if the stream errored or the server closed —
    // the registry has already cleaned up in either case.
    if !server_closed && stream_err.is_none() {
        // Race the unsubscribe against a second SIGINT — if the
        // user mashes Ctrl-C a second time we want to bail
        // immediately rather than complete the RPC.
        let unsub = client.unsubscribe(stream_id);
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(2));
        tokio::select! {
            _ = unsub => {}
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
            _ = timeout => {}
        }
    }
    // Drop the FrameStream explicitly so its Drop impl runs before
    // we print the closing banner. The Drop marks the pool guard
    // failed unless EOS was observed — prevents pool poisoning.
    drop(stream);

    if is_human_output(&output) {
        let footer = if server_closed {
            format!("(stream closed by server; {count} events)")
        } else if stream_err.is_some() {
            format!("(stream error; {count} events delivered)")
        } else {
            format!("(unsubscribed; {count} events)")
        };
        let _ = writeln!(io::stderr(), "{footer}");
        let _ = io::stderr().flush();
    }

    if let Some(e) = stream_err {
        return Err(e);
    }
    Ok(())
}

/// One event → one line on stdout, flushed.
///
/// Goes through brain-explore's dispatch so streamed events get the
/// same color / OSC 8 / format matrix as one-shot output. NDJSON is
/// the right shape across event boundaries (the caller has already
/// normalised JSON / YAML into NDJSON before calling here).
fn render_stream_event(ev: &SubscriptionEvent, ctx: &RenderCtx) -> io::Result<()> {
    let mut out = io::stdout().lock();
    let item = SubscriptionEventRendered(ev.clone());
    dispatch(&item, ctx, &mut out)?;
    out.flush()
}

/// Sentinel returned by the streaming path so the dispatch layer's
/// outer `dispatch()` call is a no-op (the events were already printed).
struct AlreadyRendered;

impl Render for AlreadyRendered {
    fn render_table(&self, _ctx: &RenderCtx, _w: &mut dyn Write) -> io::Result<()> {
        Ok(())
    }
    fn render_json(&self, _ctx: &RenderCtx) -> Value {
        Value::Null
    }
}
