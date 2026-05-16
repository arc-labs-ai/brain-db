//! Fluent builders + uniform `RelationHandle` over the 7 relation
//! wire opcodes. Phase 18.8.
//!
//! See `spec/29_knowledge_sdk/00_purpose.md` §"Typed relation API"
//! for the target ergonomics.
//!
//! ```no_run
//! # use brain_sdk_rust::{Client, ClientError, EntityId};
//! # async fn ex(client: Client, bob: EntityId, priya: EntityId) -> Result<(), ClientError> {
//! let rel = client.relation()
//!     .relation_type("brain:related_to")
//!     .from(bob)
//!     .to(priya)
//!     .confidence(0.9)
//!     .create()
//!     .await?;
//! # let _ = rel;
//! # Ok(()) }
//! ```
//!
//! Hand-written. Phase 19 adds `#[derive(BrainRelation)]` and typed
//! wrappers `Relation<ReportsTo>` etc.; v1 returns the uniform
//! [`RelationHandle`].

use brain_core::{EntityId, MemoryId, RelationId, RelationTypeId};
use brain_protocol::knowledge::{
    RelationCreateRequest, RelationGetRequest, RelationListFromRequest, RelationListToRequest,
    RelationSupersedeRequest, RelationTombstoneRequest, RelationTraverseRequest, RelationView,
    RelationWireError,
};
use brain_protocol::knowledge::statement_req::EvidenceRefWire;
use brain_protocol::opcode::Opcode;
use brain_protocol::{RequestBody, ResponseBody};

use crate::client::Client;
use crate::error::ClientError;

const SOFT_EVIDENCE_CAP: usize = 32;

// ---------------------------------------------------------------------------
// RelationHandle — uniform read-side handle.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct RelationHandle {
    pub id: RelationId,
    pub chain_root: RelationId,
    pub relation_type: String,
    pub from_entity: EntityId,
    pub to_entity: EntityId,
    pub properties_blob: Vec<u8>,
    pub evidence: Vec<MemoryId>,
    pub extractor_id: u32,
    pub extracted_at_unix_nanos: u64,
    pub confidence: f32,
    pub valid_from_unix_nanos: Option<u64>,
    pub valid_to_unix_nanos: Option<u64>,
    pub version: u32,
    pub superseded_by: Option<RelationId>,
    pub supersedes: Option<RelationId>,
    pub tombstoned: bool,
    pub tombstoned_at_unix_nanos: Option<u64>,
    pub is_symmetric: bool,
}

impl RelationHandle {
    /// Build from the wire-side `RelationView`. The wire shape
    /// carries `relation_type` as the canonical string, so this
    /// projection doesn't need a registry lookup — we synthesise a
    /// placeholder `RelationTypeId(0)` and keep the canonical
    /// string. The SDK never round-trips the id.
    pub fn from_view(view: RelationView) -> Result<Self, ClientError> {
        let qname = view.relation_type.clone();
        let r = view.to_relation(RelationTypeId::from(0)).map_err(
            |e: RelationWireError| ClientError::Internal(format!("relation decode: {e}")),
        )?;
        Ok(Self {
            id: r.id,
            chain_root: r.chain_root,
            relation_type: qname,
            from_entity: r.from_entity,
            to_entity: r.to_entity,
            properties_blob: r.properties_blob,
            evidence: r.evidence,
            extractor_id: r.extractor_id.raw(),
            extracted_at_unix_nanos: r.extracted_at_unix_nanos,
            confidence: r.confidence,
            valid_from_unix_nanos: r.valid_from_unix_nanos,
            valid_to_unix_nanos: r.valid_to_unix_nanos,
            version: r.version,
            superseded_by: r.superseded_by,
            supersedes: r.supersedes,
            tombstoned: r.tombstoned,
            tombstoned_at_unix_nanos: r.tombstoned_at_unix_nanos,
            is_symmetric: r.is_symmetric,
        })
    }

    /// Mirrors `Relation::is_current`.
    #[must_use]
    pub fn is_current(&self, now_unix_nanos: u64) -> bool {
        if self.tombstoned || self.superseded_by.is_some() {
            return false;
        }
        if let Some(start) = self.valid_from_unix_nanos {
            if now_unix_nanos < start {
                return false;
            }
        }
        if let Some(end) = self.valid_to_unix_nanos {
            if now_unix_nanos >= end {
                return false;
            }
        }
        true
    }

    #[must_use]
    pub fn is_chain_root(&self) -> bool {
        self.supersedes.is_none() && self.chain_root == self.id
    }
}

// ---------------------------------------------------------------------------
// Traversal value types.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraverseDirection {
    Outgoing,
    Incoming,
    Both,
}

impl TraverseDirection {
    fn as_wire(self) -> u8 {
        match self {
            Self::Outgoing => 0,
            Self::Incoming => 1,
            Self::Both => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TraversalStep {
    pub relation_id: RelationId,
    pub from: EntityId,
    pub to: EntityId,
    pub relation_type: String,
    pub depth: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TraversalPath {
    pub steps: Vec<TraversalStep>,
}

// ---------------------------------------------------------------------------
// Validation helpers.
// ---------------------------------------------------------------------------

fn invalid(msg: impl Into<String>) -> ClientError {
    ClientError::Internal(msg.into())
}

fn validate_qname(q: &str) -> Result<(), ClientError> {
    if q.is_empty() {
        return Err(invalid("relation_type must be non-empty"));
    }
    if !q.contains(':') {
        return Err(invalid(
            "relation_type must use \"namespace:name\" form (e.g. \"acme:reports_to\")",
        ));
    }
    if q.len() > 96 {
        return Err(invalid("relation_type qname exceeds 96 chars"));
    }
    Ok(())
}

fn validate_confidence(c: f32) -> Result<(), ClientError> {
    if c.is_nan() || !(0.0..=1.0).contains(&c) {
        return Err(invalid("confidence must be in [0, 1] and not NaN"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// RelationBuilder — CREATE / SUPERSEDE.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RelationBuild {
    relation_type: Option<String>,
    from_entity: Option<[u8; 16]>,
    to_entity: Option<[u8; 16]>,
    properties_blob: Vec<u8>,
    evidence: Vec<[u8; 16]>,
    confidence: Option<f32>,
    extractor_id: u32,
    valid_from_unix_nanos: u64,
    valid_to_unix_nanos: u64,
    supersedes: Option<RelationId>,
    request_id: Option<[u8; 16]>,
}

/// Builder for `RELATION_CREATE`. Routes to `RELATION_SUPERSEDE` if
/// `.supersedes(prior_id)` is set.
pub struct RelationBuilder<'a> {
    client: &'a Client,
    build: RelationBuild,
}

impl<'a> RelationBuilder<'a> {
    pub(crate) fn new(client: &'a Client) -> Self {
        Self {
            client,
            build: RelationBuild::default(),
        }
    }

    #[must_use]
    pub fn relation_type(mut self, qname: impl Into<String>) -> Self {
        self.build.relation_type = Some(qname.into());
        self
    }

    #[must_use]
    pub fn from(mut self, id: EntityId) -> Self {
        self.build.from_entity = Some(id.to_bytes());
        self
    }

    #[must_use]
    pub fn to(mut self, id: EntityId) -> Self {
        self.build.to_entity = Some(id.to_bytes());
        self
    }

    #[must_use]
    pub fn properties(mut self, blob: Vec<u8>) -> Self {
        self.build.properties_blob = blob;
        self
    }

    /// Set the evidence list (≤ 32 entries per spec §20/05 §3 soft
    /// cap). Overflow evidence is not supported for relations in v1.
    #[must_use]
    pub fn evidence(mut self, memories: Vec<MemoryId>) -> Self {
        self.build.evidence = memories.into_iter().map(|m| m.to_be_bytes()).collect();
        self
    }

    #[must_use]
    pub fn confidence(mut self, c: f32) -> Self {
        self.build.confidence = Some(c);
        self
    }

    #[must_use]
    pub fn extractor_id(mut self, id: u32) -> Self {
        self.build.extractor_id = id;
        self
    }

    #[must_use]
    pub fn valid_from(mut self, unix_nanos: u64) -> Self {
        self.build.valid_from_unix_nanos = unix_nanos;
        self
    }

    #[must_use]
    pub fn valid_to(mut self, unix_nanos: u64) -> Self {
        self.build.valid_to_unix_nanos = unix_nanos;
        self
    }

    #[must_use]
    pub fn supersedes(mut self, prior: RelationId) -> Self {
        self.build.supersedes = Some(prior);
        self
    }

    #[must_use]
    pub fn request_id(mut self, id: [u8; 16]) -> Self {
        self.build.request_id = Some(id);
        self
    }

    pub async fn create(self) -> Result<RelationHandle, ClientError> {
        let create_req = build_create_request(&self.build)?;

        let created_id = if let Some(prior) = self.build.supersedes {
            let body = RequestBody::RelationSupersede(RelationSupersedeRequest {
                old_relation_id: prior.to_bytes(),
                new_relation: create_req,
                request_id: self.build.request_id.unwrap_or_else(random_request_id),
            });
            let resp = self
                .client
                .send_knowledge_request(
                    body,
                    Opcode::RelationSupersedeReq,
                    Opcode::RelationSupersedeResp,
                )
                .await?;
            match resp {
                ResponseBody::RelationSupersede(r) => RelationId::from(r.new_relation_id),
                other => return Err(unexpected_body("RelationSupersedeResp", other)),
            }
        } else {
            let body = RequestBody::RelationCreate(create_req);
            let resp = self
                .client
                .send_knowledge_request(body, Opcode::RelationCreateReq, Opcode::RelationCreateResp)
                .await?;
            match resp {
                ResponseBody::RelationCreate(r) => RelationId::from(r.relation_id),
                other => return Err(unexpected_body("RelationCreateResp", other)),
            }
        };

        // Round-trip a GET to fetch the full RelationView with derived
        // chain / valid_to fields.
        let body = RequestBody::RelationGet(RelationGetRequest {
            relation_id: created_id.to_bytes(),
            follow_supersession: false,
        });
        let resp = self
            .client
            .send_knowledge_request(body, Opcode::RelationGetReq, Opcode::RelationGetResp)
            .await?;
        match resp {
            ResponseBody::RelationGet(r) => RelationHandle::from_view(r.relation),
            other => Err(unexpected_body("RelationGetResp", other)),
        }
    }
}

fn build_create_request(b: &RelationBuild) -> Result<RelationCreateRequest, ClientError> {
    let relation_type = b
        .relation_type
        .clone()
        .ok_or_else(|| invalid("relation_type is required"))?;
    validate_qname(&relation_type)?;
    let from_entity = b.from_entity.ok_or_else(|| invalid("from is required"))?;
    let to_entity = b.to_entity.ok_or_else(|| invalid("to is required"))?;
    let confidence = b.confidence.unwrap_or(0.5);
    validate_confidence(confidence)?;
    if b.evidence.len() > SOFT_EVIDENCE_CAP {
        return Err(invalid(format!(
            "relation evidence list exceeds soft cap of {SOFT_EVIDENCE_CAP}; got {}",
            b.evidence.len()
        )));
    }
    let request_id = b.request_id.unwrap_or_else(random_request_id);
    Ok(RelationCreateRequest {
        relation_type,
        from_entity,
        to_entity,
        properties_blob: b.properties_blob.clone(),
        evidence: EvidenceRefWire::Inline(b.evidence.clone()),
        extractor_id: b.extractor_id,
        confidence,
        valid_from_unix_nanos: b.valid_from_unix_nanos,
        valid_to_unix_nanos: b.valid_to_unix_nanos,
        request_id,
    })
}

// ---------------------------------------------------------------------------
// RelationsClient — query / get / tombstone / list_from / list_to / traverse.
// ---------------------------------------------------------------------------

pub struct RelationsClient<'a> {
    client: &'a Client,
}

impl<'a> RelationsClient<'a> {
    pub(crate) fn new(client: &'a Client) -> Self {
        Self { client }
    }

    /// Fetch by id. Does NOT follow supersession.
    pub async fn get(&self, id: RelationId) -> Result<Option<RelationHandle>, ClientError> {
        self.fetch(id, false).await
    }

    /// Fetch the current relation in the chain anchored at `id`.
    pub async fn get_current(
        &self,
        id: RelationId,
    ) -> Result<Option<RelationHandle>, ClientError> {
        self.fetch(id, true).await
    }

    async fn fetch(
        &self,
        id: RelationId,
        follow: bool,
    ) -> Result<Option<RelationHandle>, ClientError> {
        let body = RequestBody::RelationGet(RelationGetRequest {
            relation_id: id.to_bytes(),
            follow_supersession: follow,
        });
        match self
            .client
            .send_knowledge_request(body, Opcode::RelationGetReq, Opcode::RelationGetResp)
            .await
        {
            Ok(ResponseBody::RelationGet(r)) => Ok(Some(RelationHandle::from_view(r.relation)?)),
            Ok(other) => Err(unexpected_body("RelationGetResp", other)),
            Err(e) if is_relation_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Soft-delete a relation. Returns the server-clock timestamp at
    /// which the tombstone was committed.
    pub async fn tombstone(
        &self,
        id: RelationId,
        reason: impl Into<String>,
    ) -> Result<u64, ClientError> {
        let body = RequestBody::RelationTombstone(RelationTombstoneRequest {
            relation_id: id.to_bytes(),
            reason: reason.into(),
            request_id: random_request_id(),
        });
        let resp = self
            .client
            .send_knowledge_request(
                body,
                Opcode::RelationTombstoneReq,
                Opcode::RelationTombstoneResp,
            )
            .await?;
        match resp {
            ResponseBody::RelationTombstone(r) => Ok(r.tombstoned_at_unix_nanos),
            other => Err(unexpected_body("RelationTombstoneResp", other)),
        }
    }

    /// Start a LIST_FROM builder.
    #[must_use]
    pub fn list_from(&self, entity: EntityId) -> RelationListFromBuilder<'a> {
        RelationListFromBuilder::new(self.client, entity)
    }

    /// Start a LIST_TO builder.
    #[must_use]
    pub fn list_to(&self, entity: EntityId) -> RelationListToBuilder<'a> {
        RelationListToBuilder::new(self.client, entity)
    }

    /// Start a TRAVERSE builder.
    #[must_use]
    pub fn traverse(&self, start: EntityId) -> RelationTraverseBuilder<'a> {
        RelationTraverseBuilder::new(self.client, start)
    }
}

// ---------------------------------------------------------------------------
// RelationListFromBuilder / RelationListToBuilder.
// ---------------------------------------------------------------------------

pub struct RelationListFromBuilder<'a> {
    client: &'a Client,
    from_entity: EntityId,
    relation_type_filter: String,
    time_range_start_unix_nanos: u64,
    time_range_end_unix_nanos: u64,
    include_superseded: bool,
    include_tombstoned: bool,
    limit: u32,
}

impl<'a> RelationListFromBuilder<'a> {
    pub(crate) fn new(client: &'a Client, from_entity: EntityId) -> Self {
        Self {
            client,
            from_entity,
            relation_type_filter: String::new(),
            time_range_start_unix_nanos: 0,
            time_range_end_unix_nanos: 0,
            include_superseded: false,
            include_tombstoned: false,
            limit: 100,
        }
    }

    #[must_use]
    pub fn with_type(mut self, qname: impl Into<String>) -> Self {
        self.relation_type_filter = qname.into();
        self
    }

    #[must_use]
    pub fn include_superseded(mut self) -> Self {
        self.include_superseded = true;
        self
    }

    #[must_use]
    pub fn include_tombstoned(mut self) -> Self {
        self.include_tombstoned = true;
        self
    }

    #[must_use]
    pub fn time_range(mut self, start: u64, end: u64) -> Self {
        self.time_range_start_unix_nanos = start;
        self.time_range_end_unix_nanos = end;
        self
    }

    #[must_use]
    pub fn limit(mut self, n: u32) -> Self {
        self.limit = n;
        self
    }

    pub async fn send(self) -> Result<Vec<RelationHandle>, ClientError> {
        if self.limit == 0 || self.limit > 1000 {
            return Err(invalid("limit must be in 1..=1000"));
        }
        if !self.relation_type_filter.is_empty() {
            validate_qname(&self.relation_type_filter)?;
        }
        let body = RequestBody::RelationListFrom(RelationListFromRequest {
            from_entity: self.from_entity.to_bytes(),
            relation_type_filter: self.relation_type_filter,
            time_range_start_unix_nanos: self.time_range_start_unix_nanos,
            time_range_end_unix_nanos: self.time_range_end_unix_nanos,
            include_superseded: self.include_superseded,
            include_tombstoned: self.include_tombstoned,
            limit: self.limit,
            cursor: Vec::new(),
        });
        let resp = self
            .client
            .send_knowledge_request(
                body,
                Opcode::RelationListFromReq,
                Opcode::RelationListFromResp,
            )
            .await?;
        match resp {
            ResponseBody::RelationListFrom(frame) => {
                let mut out = Vec::with_capacity(frame.items.len());
                for v in frame.items {
                    out.push(RelationHandle::from_view(v)?);
                }
                Ok(out)
            }
            other => Err(unexpected_body("RelationListFromResp", other)),
        }
    }
}

pub struct RelationListToBuilder<'a> {
    client: &'a Client,
    to_entity: EntityId,
    relation_type_filter: String,
    time_range_start_unix_nanos: u64,
    time_range_end_unix_nanos: u64,
    include_superseded: bool,
    include_tombstoned: bool,
    limit: u32,
}

impl<'a> RelationListToBuilder<'a> {
    pub(crate) fn new(client: &'a Client, to_entity: EntityId) -> Self {
        Self {
            client,
            to_entity,
            relation_type_filter: String::new(),
            time_range_start_unix_nanos: 0,
            time_range_end_unix_nanos: 0,
            include_superseded: false,
            include_tombstoned: false,
            limit: 100,
        }
    }

    #[must_use]
    pub fn with_type(mut self, qname: impl Into<String>) -> Self {
        self.relation_type_filter = qname.into();
        self
    }

    #[must_use]
    pub fn include_superseded(mut self) -> Self {
        self.include_superseded = true;
        self
    }

    #[must_use]
    pub fn include_tombstoned(mut self) -> Self {
        self.include_tombstoned = true;
        self
    }

    #[must_use]
    pub fn time_range(mut self, start: u64, end: u64) -> Self {
        self.time_range_start_unix_nanos = start;
        self.time_range_end_unix_nanos = end;
        self
    }

    #[must_use]
    pub fn limit(mut self, n: u32) -> Self {
        self.limit = n;
        self
    }

    pub async fn send(self) -> Result<Vec<RelationHandle>, ClientError> {
        if self.limit == 0 || self.limit > 1000 {
            return Err(invalid("limit must be in 1..=1000"));
        }
        if !self.relation_type_filter.is_empty() {
            validate_qname(&self.relation_type_filter)?;
        }
        let body = RequestBody::RelationListTo(RelationListToRequest {
            to_entity: self.to_entity.to_bytes(),
            relation_type_filter: self.relation_type_filter,
            time_range_start_unix_nanos: self.time_range_start_unix_nanos,
            time_range_end_unix_nanos: self.time_range_end_unix_nanos,
            include_superseded: self.include_superseded,
            include_tombstoned: self.include_tombstoned,
            limit: self.limit,
            cursor: Vec::new(),
        });
        let resp = self
            .client
            .send_knowledge_request(
                body,
                Opcode::RelationListToReq,
                Opcode::RelationListToResp,
            )
            .await?;
        match resp {
            ResponseBody::RelationListTo(frame) => {
                let mut out = Vec::with_capacity(frame.items.len());
                for v in frame.items {
                    out.push(RelationHandle::from_view(v)?);
                }
                Ok(out)
            }
            other => Err(unexpected_body("RelationListToResp", other)),
        }
    }
}

// ---------------------------------------------------------------------------
// RelationTraverseBuilder.
// ---------------------------------------------------------------------------

pub struct RelationTraverseBuilder<'a> {
    client: &'a Client,
    start_entity: EntityId,
    relation_types: Vec<String>,
    direction: TraverseDirection,
    max_depth: u32,
    max_nodes: u32,
    time_at_unix_nanos: u64,
    include_superseded: bool,
}

impl<'a> RelationTraverseBuilder<'a> {
    pub(crate) fn new(client: &'a Client, start_entity: EntityId) -> Self {
        Self {
            client,
            start_entity,
            relation_types: Vec::new(),
            direction: TraverseDirection::Outgoing,
            max_depth: 3,
            max_nodes: 100,
            time_at_unix_nanos: 0,
            include_superseded: false,
        }
    }

    #[must_use]
    pub fn with_types(mut self, qnames: &[&str]) -> Self {
        self.relation_types = qnames.iter().map(|s| s.to_string()).collect();
        self
    }

    #[must_use]
    pub fn with_type(mut self, qname: impl Into<String>) -> Self {
        self.relation_types = vec![qname.into()];
        self
    }

    #[must_use]
    pub fn direction(mut self, d: TraverseDirection) -> Self {
        self.direction = d;
        self
    }

    #[must_use]
    pub fn depth(mut self, max_depth: u32) -> Self {
        self.max_depth = max_depth;
        self
    }

    #[must_use]
    pub fn max_nodes(mut self, n: u32) -> Self {
        self.max_nodes = n;
        self
    }

    #[must_use]
    pub fn as_of(mut self, unix_nanos: u64) -> Self {
        self.time_at_unix_nanos = unix_nanos;
        self
    }

    #[must_use]
    pub fn include_superseded(mut self) -> Self {
        self.include_superseded = true;
        self
    }

    pub async fn send(self) -> Result<Vec<TraversalPath>, ClientError> {
        if self.max_depth == 0 || self.max_depth > 5 {
            return Err(invalid("max_depth must be in 1..=5"));
        }
        if self.max_nodes == 0 || self.max_nodes > 1000 {
            return Err(invalid("max_nodes must be in 1..=1000"));
        }
        for q in &self.relation_types {
            validate_qname(q)?;
        }
        let body = RequestBody::RelationTraverse(RelationTraverseRequest {
            start_entity: self.start_entity.to_bytes(),
            relation_types: self.relation_types,
            direction: self.direction.as_wire(),
            max_depth: self.max_depth,
            max_nodes: self.max_nodes,
            time_at_unix_nanos: self.time_at_unix_nanos,
            include_superseded: self.include_superseded,
            request_id: random_request_id(),
        });
        let resp = self
            .client
            .send_knowledge_request(
                body,
                Opcode::RelationTraverseReq,
                Opcode::RelationTraverseResp,
            )
            .await?;
        match resp {
            ResponseBody::RelationTraverse(frame) => {
                let mut out = Vec::with_capacity(frame.paths.len());
                for p in frame.paths {
                    let mut steps = Vec::with_capacity(p.steps.len());
                    for s in p.steps {
                        steps.push(TraversalStep {
                            relation_id: RelationId::from_bytes(s.relation_id),
                            from: EntityId::from_bytes(s.from),
                            to: EntityId::from_bytes(s.to),
                            relation_type: s.relation_type,
                            depth: s.depth,
                        });
                    }
                    out.push(TraversalPath { steps });
                }
                Ok(out)
            }
            other => Err(unexpected_body("RelationTraverseResp", other)),
        }
    }
}

// ---------------------------------------------------------------------------
// `Client` entry-point methods.
// ---------------------------------------------------------------------------

impl Client {
    /// Start a relation CREATE / SUPERSEDE builder.
    #[must_use]
    pub fn relation(&self) -> RelationBuilder<'_> {
        RelationBuilder::new(self)
    }

    /// Entry point for non-create relation operations.
    #[must_use]
    pub fn relations(&self) -> RelationsClient<'_> {
        RelationsClient::new(self)
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn random_request_id() -> [u8; 16] {
    *uuid::Uuid::now_v7().as_bytes()
}

fn unexpected_body(expected: &str, body: ResponseBody) -> ClientError {
    ClientError::Protocol(brain_protocol::error::ProtocolError::BadFrame(format!(
        "expected {expected}, got {:?}",
        std::mem::discriminant(&body)
    )))
}

fn is_relation_not_found(err: &ClientError) -> bool {
    use crate::knowledge::errors::ClientErrorRelationExt;
    err.is_relation_not_found()
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use brain_protocol::knowledge::statement_req::EvidenceRefWire;

    fn sample_view() -> RelationView {
        RelationView {
            relation_id: [1u8; 16],
            chain_root: [1u8; 16],
            relation_type: "brain:related_to".into(),
            from_entity: [2u8; 16],
            to_entity: [3u8; 16],
            properties_blob: Vec::new(),
            evidence: EvidenceRefWire::Inline(vec![]),
            extractor_id: 0,
            extracted_at_unix_nanos: 1_700_000_000_000_000_000,
            confidence: 0.9,
            valid_from_unix_nanos: 0,
            valid_to_unix_nanos: 0,
            version: 1,
            superseded_by: [0u8; 16],
            supersedes: [0u8; 16],
            tombstoned: false,
            tombstoned_at_unix_nanos: 0,
            flags: 0,
        }
    }

    #[test]
    fn handle_from_view_round_trips() {
        let v = sample_view();
        let h = RelationHandle::from_view(v).unwrap();
        assert_eq!(h.relation_type, "brain:related_to");
        assert_eq!(h.id, RelationId::from_bytes([1u8; 16]));
        assert_eq!(h.version, 1);
        assert!(!h.tombstoned);
        assert!(h.is_chain_root());
    }

    #[test]
    fn handle_is_current_logic() {
        let h = RelationHandle::from_view(sample_view()).unwrap();
        assert!(h.is_current(1_700_000_000_000_000_001));
    }

    #[test]
    fn handle_tombstoned_not_current() {
        let mut v = sample_view();
        v.tombstoned = true;
        v.tombstoned_at_unix_nanos = 1_700_000_000_000_000_001;
        let h = RelationHandle::from_view(v).unwrap();
        assert!(!h.is_current(1_700_000_000_000_000_002));
    }

    #[test]
    fn handle_symmetric_flag_propagates() {
        let mut v = sample_view();
        v.flags = 1;
        let h = RelationHandle::from_view(v).unwrap();
        assert!(h.is_symmetric);
    }

    #[test]
    fn qname_validation() {
        assert!(validate_qname("brain:related_to").is_ok());
        assert!(validate_qname("").is_err());
        assert!(validate_qname("no_colon").is_err());
        assert!(validate_qname(&"a".repeat(97)).is_err());
    }

    #[test]
    fn confidence_validation() {
        assert!(validate_confidence(0.0).is_ok());
        assert!(validate_confidence(1.0).is_ok());
        assert!(validate_confidence(-0.1).is_err());
        assert!(validate_confidence(1.1).is_err());
        assert!(validate_confidence(f32::NAN).is_err());
    }

    #[test]
    fn build_create_request_requires_type() {
        let build = RelationBuild {
            from_entity: Some([1u8; 16]),
            to_entity: Some([2u8; 16]),
            ..Default::default()
        };
        let err = build_create_request(&build).unwrap_err();
        match err {
            ClientError::Internal(m) => assert!(m.contains("relation_type")),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn build_create_request_requires_from() {
        let build = RelationBuild {
            relation_type: Some("brain:related_to".into()),
            to_entity: Some([2u8; 16]),
            ..Default::default()
        };
        let err = build_create_request(&build).unwrap_err();
        match err {
            ClientError::Internal(m) => assert!(m.contains("from")),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn build_create_request_requires_to() {
        let build = RelationBuild {
            relation_type: Some("brain:related_to".into()),
            from_entity: Some([1u8; 16]),
            ..Default::default()
        };
        let err = build_create_request(&build).unwrap_err();
        match err {
            ClientError::Internal(m) => assert!(m.contains("\"to\"") || m.contains("to is required")),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn build_create_request_evidence_cap_rejected() {
        let build = RelationBuild {
            relation_type: Some("brain:related_to".into()),
            from_entity: Some([1u8; 16]),
            to_entity: Some([2u8; 16]),
            evidence: (0..SOFT_EVIDENCE_CAP + 1).map(|_| [0u8; 16]).collect(),
            ..Default::default()
        };
        let err = build_create_request(&build).unwrap_err();
        match err {
            ClientError::Internal(m) => assert!(m.contains("evidence")),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn traverse_direction_wire_bytes() {
        assert_eq!(TraverseDirection::Outgoing.as_wire(), 0);
        assert_eq!(TraverseDirection::Incoming.as_wire(), 1);
        assert_eq!(TraverseDirection::Both.as_wire(), 2);
    }
}
