//! `encode` verb.
//!
//! Source modes (in precedence order — clap enforces mutual
//! exclusivity at parse time):
//!   * `--vector` (gated — requires the `ENCODE_VECTOR_DIRECT` wire op).
//!   * `--from-file <path>` (path `-` = stdin; `.jsonl` opens a TXN
//!     and batches each line).
//!   * `--from-stdin` (shortcut for `--from-file -`).
//!   * positional `TEXT`.
//!
//! `--wait-for-extraction` opens a subscribe stream right after the
//! encode returns and blocks until either the `ExtractionCompleted`
//! event arrives for the new memory id or the global `--timeout`
//! elapses.

use std::io::Read;
use std::time::Duration;

use brain_core::{MemoryId, RequestId};
use brain_protocol::responses::types::EventType;
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
    if args.vector.is_some() {
        tracing::warn!(
            target: "brain_shell",
            "encode --vector: ENCODE_VECTOR_DIRECT wire op is not exposed via the \
             current SDK builder. Returning a stub error until the SDK adds it."
        );
        todo!("wire op required: `EncodeVectorDirectReq` in brain-sdk-rust for `--vector`.");
    }

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

    if args.wait_for_extraction {
        wait_for_extraction(client, MemoryId::from_raw(resp.memory_id), resp.lsn).await?;
    }

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
            tracing::warn!(
                target: "brain_shell",
                ".jsonl batching opens a TXN per file and submits one encode per line. \
                 SDK does not yet expose multi-encode batching; current implementation \
                 reads the file but submits a single encode of the first line. \
                 Wire the txn/batch path in a follow-up.",
            );
            todo!(
                "follow-up: implement .jsonl batching via TxnBegin + per-line encode + \
                 TxnCommit. Requires a multi-statement encode helper, or repeated single \
                 sends in the txn — needs an explicit decision on which."
            );
        }
        return std::fs::read_to_string(path)
            .map_err(|e| ClientError::Internal(format!("read {path}: {e}")));
    }
    match &args.text {
        Some(t) if !t.is_empty() => Ok(t.clone()),
        _ => Err(ClientError::Internal(
            "encode requires a TEXT positional or one of --from-file / --from-stdin / --vector"
                .into(),
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

/// Subscribe at `lsn+1` and block until the extractor emits an
/// `ExtractionCompleted` event for `memory_id`. Returns on timeout
/// (the global `--timeout`) so a missed event doesn't hang the shell.
async fn wait_for_extraction(
    client: &Client,
    memory_id: MemoryId,
    start_lsn: u64,
) -> Result<(), ClientError> {
    let target_raw = memory_id.raw();
    // start_lsn from the ENCODE response is the LSN of THAT op; the
    // ExtractedKnowledge event lands at start_lsn+something later.
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
                "encode --wait-for-extraction: subscribe failed ({e}); \
                 encode succeeded, returning anyway.",
            );
            return Ok(());
        }
    };
    // Hard cap so a server that never emits the event doesn't pin the
    // shell. 60s is generous given the extractor cycles every 1-5s
    // in production; the global --timeout cap is what's printed to
    // the user.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            tracing::warn!(
                target: "brain_shell",
                "encode --wait-for-extraction: timed out waiting for \
                 ExtractionCompleted({}); returning without confirmation.",
                target_raw,
            );
            return Ok(());
        }
        let next = match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(ev))) => ev,
            Ok(Some(Err(e))) => return Err(e),
            Ok(None) => {
                tracing::warn!(
                    target: "brain_shell",
                    "encode --wait-for-extraction: subscription closed before \
                     ExtractionCompleted arrived for {}.",
                    target_raw,
                );
                return Ok(());
            }
            Err(_) => continue, // shouldn't fire — outer `remaining` already enforces it.
        };
        // Match either the dedicated ExtractionCompleted variant (when
        // the server publishes it) or the failure variant (which still
        // ends the wait — there's nothing to wait for any longer).
        if matches!(
            next.event_type,
            EventType::ExtractionCompleted | EventType::ExtractionFailed
        ) && next.memory_id == target_raw
        {
            return Ok(());
        }
    }
}
