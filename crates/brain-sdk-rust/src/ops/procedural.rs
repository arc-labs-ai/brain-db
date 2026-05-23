//! `MATERIALIZE_PROCEDURAL` SDK builder (W3.1, wire v2).
//!
//! Reads an agent's stored `brain:behavior_*` Preferences and returns
//! a fully-rendered system block ready for LLM-prompt injection.

use brain_core::{AgentId, ContextId, RequestId};
use brain_protocol::{MaterializeProceduralRequest, MaterializeProceduralResponse};
use brain_protocol::codec::opcode::Opcode;
use brain_protocol::{Frame, RequestBody, ResponseBody};

use crate::client::Client;
use crate::error::ClientError;
use crate::ops::common::{send_and_read_one, FLAG_EOS};

/// Builder for `Client::materialize_procedural`. All knobs default to
/// substrate-sensible values; the caller chains only the ones they want
/// to override.
pub struct MaterializeProceduralBuilder<'a> {
    client: &'a Client,
    /// `None` → the SDK fills in the AUTH-time agent on `send`. Set
    /// when the caller wants to materialize a different agent's
    /// procedural memory (the substrate enforces scope via the API
    /// key — cross-agent reads need the RECALL permission scoped to
    /// the target agent).
    agent_id: Option<AgentId>,
    context_filter: Option<ContextId>,
    top_k: u32,
    min_confidence: f32,
    categories: Vec<String>,
    request_id: Option<RequestId>,
}

impl<'a> MaterializeProceduralBuilder<'a> {
    pub(crate) fn new(client: &'a Client) -> Self {
        Self {
            client,
            agent_id: None,
            context_filter: None,
            top_k: 20,
            min_confidence: 0.5,
            categories: Vec::new(),
            request_id: None,
        }
    }

    /// Override the agent whose procedural memory is rendered.
    /// Defaults to the AUTH-time caller.
    #[must_use]
    pub fn agent_id(mut self, agent: AgentId) -> Self {
        self.agent_id = Some(agent);
        self
    }

    /// Restrict to statements whose evidence touched this context.
    /// Advisory in v1 — see the handler doc for the substrate's
    /// current behavior.
    #[must_use]
    pub fn context(mut self, ctx: ContextId) -> Self {
        self.context_filter = Some(ctx);
        self
    }

    /// Hard cap on rendered statements. Must be in `1..=100`; the
    /// substrate rejects larger values. Defaults to 20.
    #[must_use]
    pub fn top_k(mut self, k: u32) -> Self {
        self.top_k = k;
        self
    }

    /// Floor for inclusion. Statements with `confidence < c` are
    /// dropped. Defaults to 0.5. Clamp to `[0, 1]` is the substrate's
    /// job; values outside that range return `InvalidRequest`.
    #[must_use]
    pub fn min_confidence(mut self, c: f32) -> Self {
        self.min_confidence = c;
        self
    }

    /// Add a category suffix to the allow-list (e.g. `"tone"` matches
    /// `behavior_tone`). Empty list → every `brain:behavior_*`
    /// predicate is in scope. Repeatable.
    #[must_use]
    pub fn category(mut self, suffix: impl Into<String>) -> Self {
        self.categories.push(suffix.into());
        self
    }

    #[must_use]
    pub fn request_id(mut self, id: RequestId) -> Self {
        self.request_id = Some(id);
        self
    }

    pub async fn send(self) -> Result<MaterializeProceduralResponse, ClientError> {
        let request_id = self
            .request_id
            .unwrap_or_else(|| self.client.next_request_id());
        let request_id_bytes: [u8; 16] = request_id.into();
        // The agent_id wire field is opt-in: `[0; 16]` tells the
        // substrate to use the AUTH-bound caller.
        let agent_id_bytes: [u8; 16] = self.agent_id.map(|a| a.0.into_bytes()).unwrap_or([0u8; 16]);
        let context_filter = self.context_filter.map(|c| c.raw()).unwrap_or(0);
        let top_k = self.top_k;
        let min_confidence = self.min_confidence;
        let categories = self.categories;
        let client = self.client.clone();

        client
            .run_op("materialize_procedural", || {
                let client = client.clone();
                let categories = categories.clone();
                async move {
                    let body = RequestBody::MaterializeProcedural(MaterializeProceduralRequest {
                        agent_id: agent_id_bytes,
                        context_filter,
                        top_k,
                        min_confidence,
                        categories,
                        request_id: request_id_bytes,
                    });
                    let mut guard = client.acquire().await?;
                    let stream_id = guard.next_stream_id();
                    let frame = Frame::new(
                        Opcode::MaterializeProceduralReq.as_u16(),
                        FLAG_EOS,
                        stream_id,
                        body.encode(),
                    );
                    let resp =
                        send_and_read_one(&mut guard, frame, Opcode::MaterializeProceduralResp)
                            .await?;
                    match ResponseBody::decode(Opcode::MaterializeProceduralResp, &resp.payload)? {
                        ResponseBody::MaterializeProcedural(r) => Ok(r),
                        _ => Err(ClientError::Protocol(
                            brain_protocol::error::ProtocolError::BadFrame(
                                "MaterializeProceduralResp opcode but body variant didn't match"
                                    .into(),
                            ),
                        )),
                    }
                }
            })
            .await
    }
}
