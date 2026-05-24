//! ENCODE op.
//!
//! ```no_run
//! # use brain_sdk_rust::Client;
//! # async fn ex(client: Client) -> Result<(), brain_sdk_rust::ClientError> {
//! let result = client.encode("the user said hi")
//!     .salience(0.8)
//!     .send()
//!     .await?;
//! println!("memory_id = {}", result.memory_id);
//! # Ok(()) }
//! ```

use brain_core::RequestId;
use brain_protocol::codec::opcode::Opcode;
use brain_protocol::envelope::request::{EdgeRequest, EncodeRequest, MemoryKindWire};
use brain_protocol::envelope::response::EncodeResponse;
use brain_protocol::{Frame, RequestBody, ResponseBody};

use crate::client::Client;
use crate::error::ClientError;
use crate::ops::common::{send_and_read_one, FLAG_EOS};

/// Extension trait giving [`EncodeResponse`] a pattern-matchable LSN
/// accessor.
///
/// The wire field `EncodeResponse.lsn: u64` overloads `0` as a
/// "no LSN" sentinel — emitted when the request hit the fingerprint
/// dedup index (no fresh WAL record was appended) or when an
/// idempotency replay returned a cached dedup hit. Callers that chain
/// `encode → subscribe --start-lsn` need to distinguish that case from
/// a legitimate LSN of zero, so this trait lifts the sentinel into an
/// `Option<u64>` rather than forcing every call site to compare against
/// the magic number.
///
/// ```no_run
/// # use brain_sdk_rust::EncodeResponseExt;
/// # async fn ex(client: brain_sdk_rust::Client) -> Result<(), brain_sdk_rust::ClientError> {
/// let resp = client.encode("hello").send().await?;
/// match resp.lsn() {
///     Some(lsn) => println!("subscribe from {}", lsn + 1),
///     None => println!("no fresh LSN; subscribe from tail"),
/// }
/// # Ok(()) }
/// ```
pub trait EncodeResponseExt {
    /// Returns the WAL LSN this ENCODE was committed at, or `None`
    /// when the wire reported the `0` sentinel.
    fn lsn(&self) -> Option<u64>;
}

impl EncodeResponseExt for EncodeResponse {
    fn lsn(&self) -> Option<u64> {
        if self.lsn == 0 {
            None
        } else {
            Some(self.lsn)
        }
    }
}

/// Builder for `client.encode(text)`. Required: `text`.
/// Optional: context, kind, salience, edges, txn, request_id,
/// deduplicate.
pub struct EncodeBuilder<'a> {
    client: &'a Client,
    text: String,
    context_id: u64,
    kind: MemoryKindWire,
    salience: f32,
    edges: Vec<EdgeRequest>,
    txn_id: Option<[u8; 16]>,
    request_id: Option<RequestId>,
    deduplicate: bool,
}

impl<'a> EncodeBuilder<'a> {
    pub(crate) fn new(client: &'a Client, text: impl Into<String>) -> Self {
        Self {
            client,
            text: text.into(),
            context_id: 0,
            kind: MemoryKindWire::Episodic,
            salience: 0.5,
            edges: Vec::new(),
            txn_id: None,
            request_id: None,
            deduplicate: false,
        }
    }

    /// Override the context id. Default `0` (the default context).
    #[must_use]
    pub fn context(mut self, context_id: u64) -> Self {
        self.context_id = context_id;
        self
    }

    /// Override the memory kind. Default `Episodic`.
    #[must_use]
    pub fn kind(mut self, kind: MemoryKindWire) -> Self {
        self.kind = kind;
        self
    }

    /// Override the salience hint. Default `0.5`.
    #[must_use]
    pub fn salience(mut self, salience: f32) -> Self {
        self.salience = salience;
        self
    }

    /// Attach edges to the new memory.
    #[must_use]
    pub fn edges(mut self, edges: Vec<EdgeRequest>) -> Self {
        self.edges = edges;
        self
    }

    /// Bind to an active transaction. Default `None`.
    #[must_use]
    pub fn txn(mut self, txn_id: [u8; 16]) -> Self {
        self.txn_id = Some(txn_id);
        self
    }

    /// Override the auto-generated request id —
    /// reuse the same id on retries (the SDK does this automatically).
    #[must_use]
    pub fn request_id(mut self, id: RequestId) -> Self {
        self.request_id = Some(id);
        self
    }

    /// Ask the server to deduplicate by fingerprint. Default `false`.
    #[must_use]
    pub fn deduplicate(mut self, on: bool) -> Self {
        self.deduplicate = on;
        self
    }

    /// Execute the ENCODE, retrying on retryable errors per the
    /// client's [`crate::RetryConfig`].
    pub async fn send(self) -> Result<EncodeResponse, ClientError> {
        // Mint the request id once so retries reuse it.
        let request_id = self
            .request_id
            .unwrap_or_else(|| self.client.next_request_id());
        let request_id_bytes: [u8; 16] = request_id.into();

        let text = self.text;
        let context_id = self.context_id;
        let kind = self.kind;
        let salience = self.salience;
        let edges = self.edges;
        let txn_id = self.txn_id;
        let deduplicate = self.deduplicate;
        let client = self.client.clone();

        client
            .run_op("encode", || {
                let client = client.clone();
                let text = text.clone();
                let edges = edges.clone();
                async move {
                    let body = RequestBody::Encode(EncodeRequest {
                        text,
                        context_id,
                        kind,
                        salience_hint: salience,
                        edges,
                        request_id: request_id_bytes,
                        txn_id,
                        deduplicate,
                    });
                    let mut guard = client.acquire().await?;
                    let stream_id = guard.next_stream_id();
                    let frame = Frame::new(
                        Opcode::EncodeReq.as_u16(),
                        FLAG_EOS,
                        stream_id,
                        body.encode(),
                    );
                    let resp = send_and_read_one(&mut guard, frame, Opcode::EncodeResp).await?;
                    match ResponseBody::decode(Opcode::EncodeResp, &resp.payload)? {
                        ResponseBody::Encode(r) => Ok(r),
                        _ => Err(ClientError::Protocol(
                            brain_protocol::error::ProtocolError::BadFrame(
                                "EncodeResp opcode but body variant didn't match".into(),
                            ),
                        )),
                    }
                }
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::MemoryId;
    use brain_protocol::envelope::request::MemoryKindWire;

    fn response_with_lsn(lsn: u64) -> EncodeResponse {
        EncodeResponse {
            memory_id: MemoryId::pack(0, 1, 1).raw(),
            was_deduplicated: false,
            salience: 0.5,
            auto_edges_added: 0,
            lsn,
            agent_id: [0; 16],
            context_id: 0,
            kind: MemoryKindWire::Episodic,
            created_at_unix_nanos: 0,
            edges_out_count: 0,
            embedding_model_fp: [0; 16],
            pending_stages: Vec::new(),
            has_active_schema: false,
        }
    }

    #[test]
    fn lsn_helper_returns_none_for_zero() {
        let resp = response_with_lsn(0);
        assert_eq!(resp.lsn(), None);
    }

    #[test]
    fn lsn_helper_returns_some_for_nonzero() {
        let resp = response_with_lsn(42);
        assert_eq!(resp.lsn(), Some(42));
    }
}
