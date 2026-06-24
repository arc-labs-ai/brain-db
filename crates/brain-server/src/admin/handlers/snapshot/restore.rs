//! `POST /v1/snapshots/{id}/restore?shard=N` — restore a shard from a
//! snapshot bundle.
//!
//! Restore is **destructive**: it overwrites the shard's `arena.bin`,
//! `metadata.redb`, and `wal/` with the bundle's, then recovery (on the
//! next spawn) replays the bundled WAL to the snapshot LSN. The request
//! body must carry `{"confirm": true}`; anything else is a `400`.
//!
//! Brain restores by *placing files, then running normal recovery* — it
//! never hot-swaps a live shard's mmap or open redb file. The actual
//! file placement therefore requires the target shard to be **offline**.
//! This route always verifies the bundle (BLAKE3 of every file +
//! shard-UUID match) without touching the data dir; it performs the file
//! swap only when the shard is confirmed not serving. While the shard is
//! live it returns `409 Conflict` with the offline-restore instruction,
//! so the bundle is never swapped out from under a running mmap.

use std::path::PathBuf;
use std::sync::Arc;

use brain_http::body::ResponseBody;
use bytes::Bytes;
use http::{Response, StatusCode};
use serde::Deserialize;
use tracing::{info, warn};

use crate::admin::query;
use crate::admin::util::{json_response, text_response};
use crate::admin::AdminState;
use crate::shard::restore::{restore_snapshot, verify_snapshot, RestoreError};

/// JSON body of the restore request.
#[derive(Debug, Deserialize)]
struct RestoreReq {
    /// Must be `true` — restore is destructive.
    #[serde(default)]
    confirm: bool,
}

pub async fn handle(
    id_str: &str,
    query_str: &str,
    body: Bytes,
    state: &Arc<AdminState>,
) -> Response<ResponseBody> {
    let Ok(snapshot_id) = id_str.parse::<u64>() else {
        return text_response(StatusCode::BAD_REQUEST, "snapshot id must be a u64\n");
    };
    let shard_id = match query::shard_required(query_str) {
        Ok(id) => id,
        Err(msg) => return text_response(StatusCode::BAD_REQUEST, &format!("{msg}\n")),
    };

    // Confirmation gate — destructive op.
    let req: RestoreReq = if body.is_empty() {
        RestoreReq { confirm: false }
    } else {
        match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => {
                return text_response(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid JSON body: {e}\n"),
                )
            }
        }
    };
    if !req.confirm {
        return text_response(
            StatusCode::BAD_REQUEST,
            "restore is destructive; send {\"confirm\":true} to proceed\n",
        );
    }

    let Some(shard) = state.shards.get(shard_id) else {
        return text_response(StatusCode::NOT_FOUND, "shard out of range\n");
    };

    // Derive the shard's data root + snapshot dir from the handle. The
    // shard root is the WAL directory's parent; snapshots live under
    // `<root>/snapshots/<id:020>`.
    let Some(shard_root) = shard.wal_dir().parent().map(PathBuf::from) else {
        return text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "cannot derive shard root from wal dir\n",
        );
    };
    let snapshot_dir = shard_root
        .join("snapshots")
        .join(format!("{snapshot_id:020}"));
    if !snapshot_dir.is_dir() {
        return text_response(
            StatusCode::NOT_FOUND,
            &format!("snapshot {snapshot_id} not found for shard {shard_id}\n"),
        );
    }
    let target_uuid = shard.shard_uuid();

    // Always verify first — never touch the data dir on a bad bundle.
    let snapshot_dir_v = snapshot_dir.clone();
    let verify =
        tokio::task::spawn_blocking(move || verify_snapshot(&snapshot_dir_v, target_uuid)).await;
    let manifest = match verify {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => return restore_error_response(e),
        Err(join_err) => {
            warn!(error = %join_err, "restore verify task panicked");
            return text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "restore verification task failed\n",
            );
        }
    };

    // Liveness guard: a live shard has its arena.mmap + redb file open.
    // Swapping the files underneath it would corrupt the running shard,
    // so we refuse while it's serving. `ping` round-trips through the
    // executor; if it succeeds the shard is live.
    if shard.ping().await.is_ok() {
        return json_response(
            StatusCode::CONFLICT,
            format!(
                "{{\"status\":\"verified\",\"snapshot_lsn\":{lsn},\"checkpoint_id\":{ckpt},\
                 \"detail\":\"shard {shard_id} is live; stop the shard process before restoring, \
                 then re-run restore — Brain places files then recovers on next spawn\"}}\n",
                lsn = manifest.snapshot_lsn,
                ckpt = manifest.checkpoint_id,
            ),
        );
    }

    // Shard is not serving — safe to place files. Recovery on the next
    // spawn replays the bundled WAL to snapshot_lsn and rebuilds HNSW.
    let res = tokio::task::spawn_blocking(move || {
        restore_snapshot(&snapshot_dir, &shard_root, target_uuid)
    })
    .await;
    match res {
        Ok(Ok(report)) => {
            info!(
                shard_id,
                snapshot_id,
                snapshot_lsn = report.snapshot_lsn,
                wal_segments = report.wal_segments_placed,
                "snapshot restored; re-spawn the shard to complete recovery"
            );
            json_response(
                StatusCode::OK,
                format!(
                    "{{\"status\":\"restored\",\"shard\":{shard_id},\"snapshot_lsn\":{lsn},\
                     \"checkpoint_id\":{ckpt},\"wal_segments_placed\":{segs}}}\n",
                    lsn = report.snapshot_lsn,
                    ckpt = report.checkpoint_id,
                    segs = report.wal_segments_placed,
                ),
            )
        }
        Ok(Err(e)) => restore_error_response(e),
        Err(join_err) => {
            warn!(error = %join_err, "restore task panicked");
            text_response(StatusCode::INTERNAL_SERVER_ERROR, "restore task failed\n")
        }
    }
}

fn restore_error_response(e: RestoreError) -> Response<ResponseBody> {
    let status = match e {
        RestoreError::Manifest { .. } => StatusCode::NOT_FOUND,
        RestoreError::ShardUuidMismatch { .. } | RestoreError::Integrity { .. } => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        RestoreError::Place { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    };
    warn!(error = %e, "snapshot restore failed");
    text_response(status, &format!("{e}\n"))
}
