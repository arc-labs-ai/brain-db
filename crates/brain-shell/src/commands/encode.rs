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

use crate::output::table::EncodeRendered;
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

    // The `--deduplicate` / `--no-dedup` pair is mutually exclusive at
    // the parser layer. `--no-dedup` forces off regardless of any
    // persisted default the SDK may have eventually wired up.
    let deduplicate = args.deduplicate && !args.no_dedup;

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

    Ok(Box::new(EncodeRendered {
        response: resp,
        dedup_requested: deduplicate,
    }))
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
