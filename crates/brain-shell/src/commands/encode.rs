//! `encode` verb.
//!
//! Source modes (in precedence order — clap enforces mutual
//! exclusivity at parse time):
//!   * `--from-file <path>` (path `-` = stdin; `.jsonl` opens a TXN
//!     and batches each line).
//!   * `--from-stdin` (shortcut for `--from-file -`).
//!   * positional `TEXT`.
//!
//! `--wait` opens a subscribe stream right after the encode returns
//! and blocks until every pending background stage of the write
//! (extractor, auto_edge, temporal_edge, …) emits a `StageCompleted`
//! event for the new memory id, or the shell's internal stages
//! timeout elapses.

use std::io::Read;
use std::time::Duration;

use brain_core::{MemoryId, RequestId};
use brain_protocol::EventType;
use brain_sdk_rust::{Client, ClientError};
use futures_lite::StreamExt;
use uuid::Uuid;

use brain_explore::{AutoEdgeSummary, AutoEdgesDelta, EncodeRendered};

use crate::parser::{parse_txn_id, EncodeArgs};
use crate::session::Session;

use super::Rendered;

/// Send an `ENCODE`. Inherits the session's active txn + sticky
/// context when the caller didn't override them. Pushes the
/// resulting memory id onto the recent-id list.
pub async fn run(
    client: &Client,
    session: &mut Session,
    args: EncodeArgs,
) -> Result<Rendered, ClientError> {
    let text = resolve_source_text(&args)?;
    let request_id = parse_request_id(args.request_id.as_deref())?;

    let explicit_txn = match args.txn.as_deref() {
        Some(s) => Some(parse_txn_id(s).map_err(ClientError::Internal)?),
        None => None,
    };
    let txn = session.effective_txn(explicit_txn);
    let context_id = session.effective_context(args.context);

    // Deduplication is on by default — encoding the same text twice in
    // the same (agent, context) should return the existing memory, not
    // create a duplicate. `--allow-duplicate` is the explicit opt-out
    // for episodic memory where the same content really is a second
    // distinct event.
    let deduplicate = !args.allow_duplicate;

    // Print the wait-status line IMMEDIATELY when the operator
    // passed `--wait`, before any wire I/O. Silence between Enter
    // and the encode response reads as a freeze even when everything
    // is healthy; one printed line tells the user the shell is
    // alive and what it's about to do. Flush explicitly because
    // stderr is line-buffered when piped.
    if args.wait {
        use std::io::Write as _;
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(
            stderr,
            "→ encode + waiting for background stages (up to {} s)…",
            WAIT_STAGES_TIMEOUT_SECS,
        );
        let _ = stderr.flush();
    }

    // If the operator asked us to wait for background stages, open
    // the subscribe stream BEFORE sending the encode. Workers may
    // publish `StageCompleted` between when the encode response
    // returns and when we'd otherwise open the stream — a race that
    // makes `--wait` look "stuck" (we hit the timeout instead of
    // matching the event we missed). Holding an open stream from
    // before the write closes the window.
    let pre_subscribe_stream = if args.wait {
        match client.subscribe().send_stream().await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(
                    target: "brain_shell",
                    "encode --wait: subscribe failed ({e}); \
                     proceeding without a wait — the encode itself is unaffected."
                );
                None
            }
        }
    } else {
        None
    };

    let mut b = client
        .encode(text.clone())
        .context(context_id)
        .salience(args.salience.unwrap_or(0.5))
        .deduplicate(deduplicate);
    if let Some(k) = args.kind {
        b = b.kind(k.into_wire());
    }
    if let Some(t) = txn {
        b = b.txn(t);
    }
    if !args.edges.is_empty() {
        let edges = args.edges.iter().map(|e| e.into_request()).collect();
        b = b.edges(edges);
    }
    if let Some(rid) = request_id {
        b = b.request_id(rid);
    }
    let resp = b.send().await?;
    session.push_recent_id(MemoryId::from_raw(resp.memory_id));

    let stage_results = if args.wait {
        // `--wait` blocks until every pending background stage on
        // this write emits a `StageCompleted` event. Empty
        // `pending_stages` returns a zero-result delta immediately
        // so the operator still sees a "stages completed" section
        // in the card, making clear the flag was a no-op (no
        // workers wired, dedup hit, or no schema declared so the
        // extractor lane never fired).
        let stages: Vec<brain_protocol::StageKind> = resp.pending_stages.to_vec();
        Some(
            wait_for_stages(
                pre_subscribe_stream,
                MemoryId::from_raw(resp.memory_id),
                &stages,
                resp.lsn,
            )
            .await?,
        )
    } else {
        None
    };

    // When --wait-auto-edges-ms is positive, open a filtered subscribe
    // stream for that window and collect EdgeAdded(AUTO_DERIVED) events
    // whose `from_id` matches this encode's memory id. The watcher
    // returns whatever it sees within the window; non-blocking on
    // success or empty result. The encode response already left the
    // wire — the watcher only amends what we render to the user.
    let auto_edges_delta = if args.wait_auto_edges_ms > 0 {
        let delta = watch_auto_edges(
            client,
            MemoryId::from_raw(resp.memory_id),
            resp.lsn,
            args.wait_auto_edges_ms,
        )
        .await;
        Some(delta)
    } else {
        None
    };

    let mut rendered = EncodeRendered::new(resp).with_source(text.clone());
    if let Some(delta) = auto_edges_delta {
        rendered = rendered.with_auto_edges_delta(delta);
    }
    if let Some(delta) = stage_results {
        rendered = rendered.with_stage_results(delta);
    }
    Ok(Box::new(rendered))
}

/// Pull text from whichever source the user picked. Errors when no
/// source resolves to a non-empty payload.
fn resolve_source_text(args: &EncodeArgs) -> Result<String, ClientError> {
    if args.from_stdin {
        return read_stdin();
    }
    if let Some(path) = &args.from_file {
        if path == "-" {
            return read_stdin();
        }
        if path.ends_with(".jsonl") {
            return Err(ClientError::Internal(
                ".jsonl batching is not supported. Encode files line-by-line via the shell \
                 or use --from-stdin with a single payload."
                    .into(),
            ));
        }
        return std::fs::read_to_string(path)
            .map_err(|e| ClientError::Internal(format!("read {path}: {e}")));
    }
    match &args.text {
        Some(t) if !t.is_empty() => Ok(t.clone()),
        _ => Err(ClientError::Internal(
            "encode requires a TEXT positional or one of --from-file / --from-stdin".into(),
        )),
    }
}

fn read_stdin() -> Result<String, ClientError> {
    let mut s = String::new();
    std::io::stdin()
        .read_to_string(&mut s)
        .map_err(|e| ClientError::Internal(format!("read stdin: {e}")))?;
    Ok(s)
}

fn parse_request_id(arg: Option<&str>) -> Result<Option<RequestId>, ClientError> {
    let Some(s) = arg else { return Ok(None) };
    let uuid = Uuid::parse_str(s.trim())
        .map_err(|e| ClientError::Internal(format!("bad --request-id `{s}`: {e}")))?;
    Ok(Some(RequestId(uuid)))
}

/// Subscribe at `lsn+1` and collect `EdgeAdded(AUTO_DERIVED)` events
/// whose source matches `memory_id` for up to `window_ms` milliseconds.
/// Returns whatever the watcher saw — empty list when the worker
/// didn't pair this memory or didn't run in time.
///
/// Errors during subscribe are logged and swallowed: the encode
/// already succeeded; an observation problem must not crash the
/// caller's response.
async fn watch_auto_edges(
    client: &Client,
    memory_id: MemoryId,
    start_lsn: u64,
    window_ms: u32,
) -> AutoEdgesDelta {
    // Origin discriminator on the wire's `EdgeEventPayload.origin`.
    // Mirrors `brain_metadata::tables::edge::origin::AUTO_DERIVED`;
    // hardcoded here so brain-shell doesn't pull brain-metadata in
    // for a single byte. If the meaning ever changes the broken
    // filter would silently keep `EXPLICIT` edges through, which is
    // visible immediately on first run (the wrong column on the card).
    const AUTO_DERIVED: u8 = 1;

    let started = std::time::Instant::now();
    let from_id_bytes = memory_id.raw().to_be_bytes();
    let mut edges: Vec<AutoEdgeSummary> = Vec::new();

    let stream_result = client
        .subscribe()
        .start_lsn(start_lsn.saturating_add(1))
        .send_stream()
        .await;
    let mut stream = match stream_result {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                target: "brain_shell",
                "encode --wait-auto-edges-ms: subscribe failed ({e}); \
                 encode succeeded, returning empty delta.",
            );
            return AutoEdgesDelta {
                elapsed_ms: started.elapsed().as_millis() as u64,
                edges,
            };
        }
    };

    let window = Duration::from_millis(u64::from(window_ms));
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let next = match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(ev))) => ev,
            Ok(Some(Err(e))) => {
                tracing::debug!(
                    target: "brain_shell",
                    "encode --wait-auto-edges-ms: stream error ({e}); returning partial delta."
                );
                break;
            }
            Ok(None) => break, // server closed the stream
            Err(_) => break,   // timeout — done
        };
        if !matches!(next.event_type, EventType::EdgeAdded) {
            continue;
        }
        let Some(payload) = next.edge_payload.as_ref() else {
            continue;
        };
        // Match the encode's memory on either side of the edge.
        // Different workers stamp the new memory in different
        // positions:
        //   * AutoEdgeWorker (SimilarTo) writes `new → similar`
        //     (and a mirror), so `from_id == this memory`.
        //   * TemporalEdgeWorker (FollowedBy) writes
        //     `predecessor → new`, so `to_id == this memory`.
        //   * CausalEdgeWorker (when it lands) is direction-by-
        //     statement-semantics; either side can match.
        // Reject EXPLICIT-origin events so a concurrent `link`
        // call doesn't appear in the delta line.
        if payload.origin != AUTO_DERIVED {
            continue;
        }
        let this_is_source = payload.from_id == from_id_bytes;
        let this_is_target = payload.to_id == from_id_bytes;
        if !this_is_source && !this_is_target {
            continue;
        }
        // Display the OTHER end of the edge — the one the user
        // actually cares about ("what got linked to my new memory").
        let other_bytes = if this_is_source {
            payload.to_id
        } else {
            payload.from_id
        };
        edges.push(AutoEdgeSummary {
            target: u128::from_be_bytes(other_bytes),
            kind: edge_kind_label(payload.edge_kind_tag, payload.edge_kind_byte),
            weight: payload.weight,
        });
    }

    AutoEdgesDelta {
        elapsed_ms: started.elapsed().as_millis() as u64,
        edges,
    }
}

/// Cheap label for the edge-kind discriminator the wire ships.
/// `tag 0` = `Builtin(EdgeKind)`; the byte is the substrate kind.
/// `tag 1` = `Mentions`; `tag 2` = `Typed(RelationTypeId)`. Auto-edges
/// are always `Builtin(SimilarTo)` today; the other variants render
/// generically in case future workers emit them.
fn edge_kind_label(tag: u8, byte: u8) -> String {
    match tag {
        0 => match byte {
            0 => "Caused",
            1 => "FollowedBy",
            2 => "DerivedFrom",
            3 => "SimilarTo",
            4 => "Contradicts",
            5 => "Supports",
            6 => "References",
            7 => "PartOf",
            _ => "Builtin",
        }
        .to_string(),
        1 => "Mentions".to_string(),
        2 => "Typed".to_string(),
        _ => format!("kind({tag},{byte})"),
    }
}

/// Hard cap on how long `--wait*` blocks. Chosen for interactive
/// REPL feel: long enough that a healthy extractor (1 s tick + LLM
/// round-trip) lands in time; short enough that a broken worker
/// doesn't make the shell look frozen. The render path always
/// surfaces timed-out stages so the operator sees what didn't
/// arrive.
const WAIT_STAGES_TIMEOUT_SECS: u64 = 10;

/// Consume `pre_stream` (opened *before* the encode call to avoid
/// the publish-vs-subscribe race) and block until *every* pending
/// stage for `memory_id` has emitted a `StageCompleted` event — or
/// the timeout fires. Returns one [`StageResult`] per match plus
/// the stage kinds that timed out.
///
/// Empty `pending_stages` or `None` stream returns immediately
/// with a zero-result delta so the caller still renders a stages
/// section (making clear the flag was a no-op for this write).
async fn wait_for_stages(
    pre_stream: Option<brain_sdk_rust::FrameStream<brain_protocol::envelope::response::SubscriptionEvent>>,
    memory_id: MemoryId,
    pending_stages: &[brain_protocol::StageKind],
    _start_lsn: u64,
) -> Result<brain_explore::StageResultsDelta, ClientError> {
    use brain_explore::{StageOutcomeLabel, StageResult, StageResultsDelta};
    use brain_protocol::{StageKind, StageOutcome};

    let started = std::time::Instant::now();
    if pending_stages.is_empty() {
        return Ok(StageResultsDelta {
            elapsed_ms: 0,
            results: Vec::new(),
            timed_out: Vec::new(),
        });
    }
    let target_raw = memory_id.raw();
    let mut remaining_kinds: std::collections::HashSet<StageKind> =
        pending_stages.iter().copied().collect();
    let mut results: Vec<StageResult> = Vec::new();

    let Some(mut stream) = pre_stream else {
        // Pre-subscribe failed; surface every requested stage as
        // timed out so the operator sees the gap explicitly.
        return Ok(StageResultsDelta {
            elapsed_ms: started.elapsed().as_millis() as u64,
            results,
            timed_out: remaining_kinds.into_iter().map(wire_to_label).collect(),
        });
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(WAIT_STAGES_TIMEOUT_SECS);
    while !remaining_kinds.is_empty() {
        let remaining_time = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining_time.is_zero() {
            tracing::warn!(
                target: "brain_shell",
                "encode --wait: timed out waiting for stages {:?} on memory {}.",
                remaining_kinds,
                target_raw,
            );
            break;
        }
        let next = match tokio::time::timeout(remaining_time, stream.next()).await {
            Ok(Some(Ok(ev))) => ev,
            Ok(Some(Err(e))) => return Err(e),
            Ok(None) => {
                tracing::warn!(
                    target: "brain_shell",
                    "encode --wait: subscription closed before stages {:?} completed for {}.",
                    remaining_kinds,
                    target_raw,
                );
                break;
            }
            Err(_) => continue,
        };
        if !matches!(next.event_type, EventType::StageCompleted) {
            continue;
        }
        if next.memory_id != target_raw {
            continue;
        }
        let Some(kind) = next.stage_kind else {
            continue;
        };
        // Only consume events for stages we asked to wait on; an
        // unrelated stage on the same memory_id stays in the stream
        // for whoever else is listening.
        if !remaining_kinds.remove(&kind) {
            continue;
        }
        let outcome = match next.stage_outcome.unwrap_or(StageOutcome::Ok) {
            StageOutcome::Ok => StageOutcomeLabel::Ok,
            StageOutcome::Empty => StageOutcomeLabel::Empty,
            StageOutcome::Failed => StageOutcomeLabel::Failed,
        };
        let summary = summarize_stage_payload(kind, next.stage_payload.as_ref(), outcome);
        results.push(StageResult {
            kind: wire_to_label(kind),
            outcome,
            summary,
        });
    }

    Ok(StageResultsDelta {
        elapsed_ms: started.elapsed().as_millis() as u64,
        results,
        timed_out: remaining_kinds.into_iter().map(wire_to_label).collect(),
    })
}

fn wire_to_label(k: brain_protocol::StageKind) -> brain_explore::StageKindLabel {
    use brain_explore::StageKindLabel;
    use brain_protocol::StageKind;
    match k {
        StageKind::AutoEdge => StageKindLabel::AutoEdge,
        StageKind::TemporalEdge => StageKindLabel::TemporalEdge,
        StageKind::Extractor => StageKindLabel::Extractor,
    }
}

/// Build a one-line summary for the rendered stage row. Uses the
/// payload when present (entity / statement / relation counts for
/// extractor; edges written for edge stages); falls back to a
/// generic phrase when the payload is missing.
fn summarize_stage_payload(
    kind: brain_protocol::StageKind,
    payload: Option<&brain_protocol::StagePayload>,
    outcome: brain_explore::StageOutcomeLabel,
) -> String {
    use brain_explore::StageOutcomeLabel;
    use brain_protocol::{StageAuditStatus, StagePayload};
    match (kind, payload) {
        (_, Some(StagePayload::AutoEdge(p))) => {
            if p.edges_written == 0 {
                "no SimilarTo edges (below threshold or zero-vector)".into()
            } else {
                let plural = if p.edges_written == 1 { "" } else { "s" };
                format!("{} SimilarTo edge{plural} written", p.edges_written)
            }
        }
        (_, Some(StagePayload::TemporalEdge(p))) => {
            if p.edges_written == 0 {
                "no FollowedBy edge (no predecessor in session window)".into()
            } else {
                let plural = if p.edges_written == 1 { "" } else { "s" };
                format!("{} FollowedBy edge{plural} written", p.edges_written)
            }
        }
        (_, Some(StagePayload::Extractor(p))) => {
            let status = match p.audit_status {
                StageAuditStatus::Succeeded => "succeeded",
                StageAuditStatus::PartiallyApplied => "partially applied",
                StageAuditStatus::Failed => "failed",
                StageAuditStatus::Skipped => "skipped",
            };
            format!(
                "{} entit{}, {} statement{}, {} relation{} · {status}",
                p.entity_count,
                if p.entity_count == 1 { "y" } else { "ies" },
                p.statement_count,
                if p.statement_count == 1 { "" } else { "s" },
                p.relation_count,
                if p.relation_count == 1 { "" } else { "s" },
            )
        }
        // No payload on the envelope — happens when a worker
        // publishes a bare StageCompleted (unusual). Use the
        // outcome as the summary instead of leaving the row blank.
        (k, None) => match outcome {
            StageOutcomeLabel::Ok => format!("{} completed", stage_kind_word(k)),
            StageOutcomeLabel::Empty => format!("{} ran — nothing to add", stage_kind_word(k)),
            StageOutcomeLabel::Failed => format!("{} failed", stage_kind_word(k)),
        },
    }
}

fn stage_kind_word(k: brain_protocol::StageKind) -> &'static str {
    use brain_protocol::StageKind;
    match k {
        StageKind::AutoEdge => "auto_edge",
        StageKind::TemporalEdge => "temporal_edge",
        StageKind::Extractor => "extractor",
    }
}
