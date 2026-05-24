//! Admin sub-client — operator-only ops over the wire.
//!
//! Current surface:
//! - `client.admin().backfill(scope).send().await` — submit a
//!   backfill run, returns a [`BackfillHandle`] carrying the
//!   server-assigned id + initial progress snapshot.
//! - `BackfillHandle::cancel(&client).await` — cancel the run.
//!
//! All admin ops are fire-and-forget on the wire today: callers
//! poll progress out of band (via `ADMIN_STATS` or a future
//! `ADMIN_BACKFILL_PROGRESS` opcode). Streaming progress mirrors
//! `ADMIN_MIGRATE_EMBEDDINGS` and is deferred until the worker
//! exposes a streaming channel.

use brain_core::{BackfillId, MemoryId, RequestId};
use brain_protocol::codec::opcode::Opcode;
use brain_protocol::envelope::request::{
    AdminBackfillCancelRequest, AdminBackfillRequest, BackfillScope,
};
use brain_protocol::envelope::response::{
    AdminBackfillCancelResponse, AdminBackfillResponse, BackfillProgress,
};
use brain_protocol::{Frame, RequestBody, ResponseBody};

use crate::client::Client;
use crate::error::ClientError;
use crate::ops::common::{send_and_read_one, FLAG_EOS};

/// Sub-client for admin / operator ops. Construct via
/// [`Client::admin`].
pub struct AdminClient<'a> {
    client: &'a Client,
}

impl<'a> AdminClient<'a> {
    pub(crate) fn new(client: &'a Client) -> Self {
        Self { client }
    }

    /// Start a backfill request builder. Chain extractor ids /
    /// dry-run flag, then `.send().await`.
    ///
    /// ```no_run
    /// # use brain_sdk_rust::{Client, BackfillScope};
    /// # async fn ex(client: Client) -> Result<(), brain_sdk_rust::ClientError> {
    /// let handle = client.admin()
    ///     .backfill(BackfillScope::All)
    ///     .extractor(1)
    ///     .extractor(2)
    ///     .dry_run()
    ///     .send()
    ///     .await?;
    /// println!("submitted backfill {:?}", handle.id());
    /// # Ok(()) }
    /// ```
    #[must_use]
    pub fn backfill(&self, scope: BackfillScope) -> BackfillBuilder<'a> {
        BackfillBuilder::new(self.client, scope)
    }

    /// Cancel an in-flight backfill run by id. Mirrors
    /// [`BackfillHandle::cancel`] for callers that hold a raw id
    /// (e.g. one read from `ADMIN_STATS`).
    pub async fn backfill_cancel(
        &self,
        backfill_id: BackfillId,
    ) -> Result<AdminBackfillCancelResponse, ClientError> {
        let request_id = self.client.next_request_id();
        backfill_cancel_inner(self.client, backfill_id, request_id).await
    }
}

/// Builder for [`AdminBackfillRequest`].
pub struct BackfillBuilder<'a> {
    client: &'a Client,
    scope: BackfillScope,
    extractor_ids: Vec<u32>,
    dry_run: bool,
    request_id: Option<RequestId>,
}

impl<'a> BackfillBuilder<'a> {
    fn new(client: &'a Client, scope: BackfillScope) -> Self {
        Self {
            client,
            scope,
            extractor_ids: Vec::with_capacity(4),
            dry_run: false,
            request_id: None,
        }
    }

    /// Add an extractor id to the run. Call repeatedly for multiple
    /// extractors (capped at 4 server-side).
    #[must_use]
    pub fn extractor(mut self, id: u32) -> Self {
        self.extractor_ids.push(id);
        self
    }

    /// Replace the extractor list wholesale.
    #[must_use]
    pub fn extractors(mut self, ids: impl IntoIterator<Item = u32>) -> Self {
        self.extractor_ids = ids.into_iter().collect();
        self
    }

    /// Mark the run as dry-run — the worker walks the plan + records
    /// per-item `Completed` checkpoints without invoking extractors.
    #[must_use]
    pub fn dry_run(mut self) -> Self {
        self.dry_run = true;
        self
    }

    /// Override the idempotency key. Defaults to a fresh
    /// `RequestId`. Re-submitting with the same id returns the
    /// cached response.
    #[must_use]
    pub fn request_id(mut self, id: RequestId) -> Self {
        self.request_id = Some(id);
        self
    }

    /// Send the request. Returns a [`BackfillHandle`] carrying the
    /// worker-assigned `BackfillId` + the initial progress snapshot.
    pub async fn send(self) -> Result<BackfillHandle, ClientError> {
        let request_id = self
            .request_id
            .unwrap_or_else(|| self.client.next_request_id());
        let request_id_bytes: [u8; 16] = request_id.into();
        let scope = self.scope;
        let extractor_ids = self.extractor_ids;
        let dry_run = self.dry_run;
        let client = self.client.clone();

        client
            .run_op("admin.backfill", || {
                let client = client.clone();
                let extractor_ids = extractor_ids.clone();
                async move {
                    let body = RequestBody::AdminBackfill(AdminBackfillRequest {
                        scope,
                        extractor_ids,
                        dry_run,
                        request_id: request_id_bytes,
                    });
                    let mut guard = client.acquire().await?;
                    let stream_id = guard.next_stream_id();
                    let frame = Frame::new(
                        Opcode::AdminBackfillReq.as_u16(),
                        FLAG_EOS,
                        stream_id,
                        body.encode(),
                    );
                    let resp =
                        send_and_read_one(&mut guard, frame, Opcode::AdminBackfillResp).await?;
                    match ResponseBody::decode(Opcode::AdminBackfillResp, &resp.payload)? {
                        ResponseBody::AdminBackfill(r) => Ok(BackfillHandle::from_response(r)),
                        _ => Err(ClientError::Protocol(
                            brain_protocol::error::ProtocolError::BadFrame(
                                "AdminBackfillResp opcode but body variant didn't match".into(),
                            ),
                        )),
                    }
                }
            })
            .await
    }
}

/// Handle returned by [`BackfillBuilder::send`]. Carries the
/// worker-assigned id (needed for cancellation) and the initial
/// progress snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackfillHandle {
    id: BackfillId,
    progress: BackfillProgress,
}

impl BackfillHandle {
    fn from_response(r: AdminBackfillResponse) -> Self {
        Self {
            id: BackfillId::from_bytes(r.backfill_id),
            progress: r.progress,
        }
    }

    /// The worker-assigned id for this run.
    #[must_use]
    pub fn id(&self) -> BackfillId {
        self.id
    }

    /// Initial progress snapshot at submission. For a fresh
    /// submission this is the worker's idle-state snapshot.
    #[must_use]
    pub fn progress(&self) -> BackfillProgress {
        self.progress
    }

    /// `last_processed_memory_id` decoded from the flattened wire
    /// representation. `None` when the worker hasn't advanced past
    /// any item yet.
    #[must_use]
    pub fn last_processed_memory_id(&self) -> Option<MemoryId> {
        if self.progress.last_processed_memory_id_present {
            Some(MemoryId::from_raw(self.progress.last_processed_memory_id))
        } else {
            None
        }
    }

    /// Cancel the in-flight run. The server flips the worker's
    /// per-run cancel flag; the run finalises at the next item
    /// boundary. Returns the final progress snapshot.
    pub async fn cancel(self, client: &Client) -> Result<AdminBackfillCancelResponse, ClientError> {
        let request_id = client.next_request_id();
        backfill_cancel_inner(client, self.id, request_id).await
    }
}

async fn backfill_cancel_inner(
    client: &Client,
    backfill_id: BackfillId,
    request_id: RequestId,
) -> Result<AdminBackfillCancelResponse, ClientError> {
    let request_id_bytes: [u8; 16] = request_id.into();
    let backfill_id_bytes = backfill_id.to_bytes();
    let client_owned = client.clone();
    client_owned
        .run_op("admin.backfill_cancel", || {
            let client = client_owned.clone();
            async move {
                let body = RequestBody::AdminBackfillCancel(AdminBackfillCancelRequest {
                    backfill_id: backfill_id_bytes,
                    request_id: request_id_bytes,
                });
                let mut guard = client.acquire().await?;
                let stream_id = guard.next_stream_id();
                let frame = Frame::new(
                    Opcode::AdminBackfillCancelReq.as_u16(),
                    FLAG_EOS,
                    stream_id,
                    body.encode(),
                );
                let resp =
                    send_and_read_one(&mut guard, frame, Opcode::AdminBackfillCancelResp).await?;
                match ResponseBody::decode(Opcode::AdminBackfillCancelResp, &resp.payload)? {
                    ResponseBody::AdminBackfillCancel(r) => Ok(r),
                    _ => Err(ClientError::Protocol(
                        brain_protocol::error::ProtocolError::BadFrame(
                            "AdminBackfillCancelResp opcode but body variant didn't match".into(),
                        ),
                    )),
                }
            }
        })
        .await
}
