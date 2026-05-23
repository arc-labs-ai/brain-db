//! Procedural-memory materialization — request side (W3.1, wire v2).
//!
//! Procedural memory is a `Statement{ kind: Preference, subject: Agent,
//! predicate: brain:behavior_*, object: prompt_fragment }`. The
//! `MATERIALIZE_PROCEDURAL` op walks the agent's active behavior_*
//! Preferences and renders them as a single system block ready to
//! drop into an LLM prompt.
//!
//! Response side: [`crate::responses::procedural`].

use rkyv::{Archive, Deserialize, Serialize};

use crate::envelope::request::{WireContextId, WireUuid};

/// `MATERIALIZE_PROCEDURAL` (`0x0164`).
///
/// Fields:
/// - `agent_id` — the agent whose learned behaviors are rendered. The
///   handler resolves the entity for this agent and filters statements
///   whose `subject == AgentEntity(agent_id)`.
/// - `context_filter` — when set, restrict to evidence sourced from
///   memories in this context. `0` means no restriction.
/// - `top_k` — hard cap on rendered statements. Must be in `1..=100`.
///   Defaults to 20 when the client passes `0`.
/// - `min_confidence` — floor for inclusion; rows with `confidence`
///   strictly less than this are skipped. Clamped to `[0.0, 1.0]`.
/// - `categories` — optional predicate-suffix filter (`["tone",
///   "style"]` matches `behavior_tone` and `behavior_style`). Empty
///   means "every brain:behavior_* predicate".
/// - `request_id` — idempotency / tracing id.
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct MaterializeProceduralRequest {
    pub agent_id: WireUuid,
    pub context_filter: WireContextId,
    pub top_k: u32,
    pub min_confidence: f32,
    pub categories: Vec<String>,
    pub request_id: WireUuid,
}

#[cfg(test)]
mod tests_req {
    use super::*;
    use crate::codec::opcode::Opcode;
    use crate::envelope::request::RequestBody;
    use crate::codec::rkyv::{from_rkyv_bytes, to_rkyv_bytes};

    fn sample_uuid(seed: u8) -> WireUuid {
        let mut u = [0u8; 16];
        for (i, b) in u.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8);
        }
        u
    }

    #[test]
    fn opcode_byte_assignments() {
        assert_eq!(Opcode::MaterializeProceduralReq.as_u16(), 0x0164);
        assert!(Opcode::MaterializeProceduralReq.is_request());
        assert!(Opcode::MaterializeProceduralReq.is_typed_graph());
    }

    #[test]
    fn request_round_trips_via_rkyv() {
        let req = MaterializeProceduralRequest {
            agent_id: sample_uuid(1),
            context_filter: 7,
            top_k: 20,
            min_confidence: 0.5,
            categories: vec!["tone".into(), "style".into()],
            request_id: sample_uuid(2),
        };
        let bytes = to_rkyv_bytes(&req);
        let back: MaterializeProceduralRequest = from_rkyv_bytes(&bytes).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn request_round_trips_through_request_body() {
        let req = MaterializeProceduralRequest {
            agent_id: sample_uuid(5),
            context_filter: 0,
            top_k: 0,
            min_confidence: 0.0,
            categories: Vec::new(),
            request_id: sample_uuid(6),
        };
        let body = RequestBody::MaterializeProcedural(req.clone());
        assert_eq!(body.opcode(), Opcode::MaterializeProceduralReq);
        let bytes = body.encode();
        let decoded = RequestBody::decode(Opcode::MaterializeProceduralReq, &bytes).unwrap();
        assert_eq!(decoded, RequestBody::MaterializeProcedural(req));
    }
}

// ============================================================
// Response payloads
// ============================================================

/// `MATERIALIZE_PROCEDURAL_RESP` (`0x01E4`).
///
/// `system_block` is fully rendered Markdown suitable for injection
/// into an LLM system prompt. `statement_ids` lists every contributing
/// statement (in rendering order) so callers can audit / explain.
/// `total_candidates` reports how many behavior_* statements matched
/// before the `top_k` cap; `trimmed_by_budget` is true iff the cap
/// actually fired (`total_candidates > top_k`).
#[derive(Archive, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[archive(check_bytes)]
#[archive_attr(derive(Debug))]
pub struct MaterializeProceduralResponse {
    pub system_block: String,
    pub statement_ids: Vec<WireUuid>,
    pub total_candidates: u32,
    pub trimmed_by_budget: bool,
}

#[cfg(test)]
mod tests_resp {
    use super::*;
    use crate::codec::opcode::Opcode;
    use crate::codec::rkyv::{from_rkyv_bytes, to_rkyv_bytes};
    use crate::envelope::response::ResponseBody;

    fn sample_uuid(seed: u8) -> WireUuid {
        let mut u = [0u8; 16];
        for (i, b) in u.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8);
        }
        u
    }

    #[test]
    fn opcode_byte_assignment() {
        assert_eq!(Opcode::MaterializeProceduralResp.as_u16(), 0x01E4);
        assert!(Opcode::MaterializeProceduralResp.is_response());
    }

    #[test]
    fn response_round_trips_via_rkyv() {
        let resp = MaterializeProceduralResponse {
            system_block: "# Learned behaviors\n\n- be concise\n".into(),
            statement_ids: vec![sample_uuid(3), sample_uuid(4)],
            total_candidates: 5,
            trimmed_by_budget: true,
        };
        let bytes = to_rkyv_bytes(&resp);
        let back: MaterializeProceduralResponse = from_rkyv_bytes(&bytes).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn response_round_trips_through_response_body() {
        let resp = MaterializeProceduralResponse {
            system_block: String::new(),
            statement_ids: Vec::new(),
            total_candidates: 0,
            trimmed_by_budget: false,
        };
        let body = ResponseBody::MaterializeProcedural(resp.clone());
        assert_eq!(body.opcode(), Opcode::MaterializeProceduralResp);
        let bytes = body.encode();
        let decoded = ResponseBody::decode(Opcode::MaterializeProceduralResp, &bytes).unwrap();
        assert_eq!(decoded, ResponseBody::MaterializeProcedural(resp));
    }
}
