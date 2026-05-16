//! Fluent builders over the knowledge-layer entity opcodes. Phase 16.8.2.
//!
//! ```no_run
//! # use brain_sdk_rust::{Client, Person};
//! # async fn ex(client: Client) -> Result<(), brain_sdk_rust::ClientError> {
//! // Create
//! let priya = client.entity::<Person>()
//!     .create()
//!     .canonical_name("Priya Patel")
//!     .alias("Priya")
//!     .with_email("priya@example.com")
//!     .with_role("Engineering Manager")
//!     .send()
//!     .await?;
//!
//! // Get
//! let _maybe = client.entity::<Person>().get(priya.id).await?;
//!
//! // Update (full-replace semantics; carries the new attribute snapshot)
//! let _renamed = client.entity::<Person>()
//!     .update(priya.id)
//!     .canonical_name("Priya Singh")
//!     .with_team("Platform")
//!     .send()
//!     .await?;
//!
//! // Rename shortcut (no other field changes)
//! let _ = client.entity::<Person>().rename(priya.id, "Priya P. Singh").await?;
//! # Ok(()) }
//! ```

use std::marker::PhantomData;

use brain_core::EntityId;
use brain_protocol::knowledge::{
    EntityCreateRequest, EntityGetRequest, EntityListRequest, EntityListResponseFrame,
    EntityMergeRequest, EntityRenameRequest, EntityResolveRequest, EntityTombstoneRequest,
    EntityUnmergeRequest, EntityUpdateRequest, ResolutionOutcomeWire,
};
use brain_protocol::opcode::Opcode;
use brain_protocol::{Frame, RequestBody, ResponseBody};

use crate::client::Client;
use crate::error::ClientError;
use crate::knowledge::entity::{BrainEntityType, EntityHandle, EntityHandleFromViewError};
use crate::knowledge::errors::ClientErrorEntityExt;
use crate::knowledge::Person;
use crate::ops::common::{send_and_read_one, FLAG_EOS};

// ---------------------------------------------------------------------------
// EntityClient<T> — the typed entry point returned by client.entity::<T>().
// ---------------------------------------------------------------------------

/// Typed entry point for entity operations against a specific
/// `BrainEntityType`. Construct via [`Client::entity`].
///
/// All operations on this struct return either a one-shot async
/// method or a chainable builder; see the [`crate::knowledge::builder`]
/// module docs for examples.
pub struct EntityClient<'a, T: BrainEntityType> {
    client: &'a Client,
    _marker: PhantomData<T>,
}

impl<'a, T: BrainEntityType> EntityClient<'a, T>
where
    T::Attributes: PartialEq + Eq,
{
    pub(crate) fn new(client: &'a Client) -> Self {
        Self {
            client,
            _marker: PhantomData,
        }
    }

    /// Start a CREATE builder. Chain `.canonical_name(...)`, optional
    /// `.alias(...)` calls and `.with_*(...)` attribute setters, then
    /// `.send().await`.
    #[must_use]
    pub fn create(&self) -> EntityCreateBuilder<'a, T> {
        EntityCreateBuilder::new(self.client)
    }

    /// Fetch an entity by id. Returns `None` if the server doesn't
    /// know it (`EntityNotFound` mapped to None).
    pub async fn get(&self, id: EntityId) -> Result<Option<EntityHandle<T>>, ClientError> {
        let body = RequestBody::EntityGet(EntityGetRequest {
            entity_id: id.to_bytes(),
        });
        match self
            .client
            .send_knowledge_request(body, Opcode::EntityGetReq, Opcode::EntityGetResp)
            .await
        {
            Ok(ResponseBody::EntityGet(r)) => {
                Ok(Some(EntityHandle::from_view(r.entity).map_err(map_type_mismatch)?))
            }
            Ok(other) => Err(unexpected_body("EntityGetResp", other)),
            Err(e) if is_entity_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Start an UPDATE builder for the given entity. The wire shape is
    /// full-replace: the builder's final state becomes the new row.
    #[must_use]
    pub fn update(&self, id: EntityId) -> EntityUpdateBuilder<'a, T> {
        EntityUpdateBuilder::new(self.client, id)
    }

    /// Rename shortcut. Equivalent to `update(id).canonical_name(name).send()`
    /// but using the dedicated `ENTITY_RENAME` opcode (which preserves
    /// existing aliases / attributes server-side).
    pub async fn rename(
        &self,
        id: EntityId,
        new_canonical_name: impl Into<String>,
    ) -> Result<EntityHandle<T>, ClientError> {
        let body = RequestBody::EntityRename(EntityRenameRequest {
            entity_id: id.to_bytes(),
            new_canonical_name: new_canonical_name.into(),
            move_to_alias: true,
            request_id: random_request_id(),
        });
        let resp = self
            .client
            .send_knowledge_request(body, Opcode::EntityRenameReq, Opcode::EntityRenameResp)
            .await?;
        match resp {
            ResponseBody::EntityRename(r) => {
                EntityHandle::from_view(r.entity).map_err(map_type_mismatch)
            }
            other => Err(unexpected_body("EntityRenameResp", other)),
        }
    }

    /// Start a MERGE builder. Merges `merged` into `survivor` once
    /// `.confidence(...)` / `.reason(...)` are set and `.send().await`
    /// runs.
    #[must_use]
    pub fn merge(&self, survivor: EntityId, merged: EntityId) -> EntityMergeBuilder<'a, T> {
        EntityMergeBuilder::new(self.client, survivor, merged)
    }

    /// Reverse a recent merge. Returns the survivor's id for caller
    /// convenience (which is `merged_entity.merged_into` pre-call).
    pub async fn unmerge(&self, merged_entity: EntityId) -> Result<EntityId, ClientError> {
        let body = RequestBody::EntityUnmerge(EntityUnmergeRequest {
            merged_entity: merged_entity.to_bytes(),
            request_id: random_request_id(),
        });
        let resp = self
            .client
            .send_knowledge_request(body, Opcode::EntityUnmergeReq, Opcode::EntityUnmergeResp)
            .await?;
        match resp {
            ResponseBody::EntityUnmerge(r) => Ok(EntityId::from(r.restored_entity_id)),
            other => Err(unexpected_body("EntityUnmergeResp", other)),
        }
    }

    /// Start a RESOLVE builder. Provide optional `.context(...)` text
    /// (helps the embedding tier in phase 21+) and `.allow_create(...)`,
    /// then `.send().await`.
    #[must_use]
    pub fn resolve(&self, candidate_name: impl Into<String>) -> EntityResolveBuilder<'a, T> {
        EntityResolveBuilder::new(self.client, candidate_name.into())
    }

    /// Start a LIST builder. Single-page snapshot in 16.7 (limit cap
    /// 1000); cursor-resume lands in phase 23.
    #[must_use]
    pub fn list(&self) -> EntityListBuilder<'a, T> {
        EntityListBuilder::new(self.client)
    }

    /// Tombstone an entity. Returns the server-clock timestamp at
    /// which the tombstone was committed.
    pub async fn tombstone(
        &self,
        id: EntityId,
        reason: impl Into<String>,
    ) -> Result<u64, ClientError> {
        let body = RequestBody::EntityTombstone(EntityTombstoneRequest {
            entity_id: id.to_bytes(),
            reason: reason.into(),
            request_id: random_request_id(),
        });
        let resp = self
            .client
            .send_knowledge_request(body, Opcode::EntityTombstoneReq, Opcode::EntityTombstoneResp)
            .await?;
        match resp {
            ResponseBody::EntityTombstone(r) => Ok(r.tombstoned_at_unix_nanos),
            other => Err(unexpected_body("EntityTombstoneResp", other)),
        }
    }
}

// ---------------------------------------------------------------------------
// EntityCreateBuilder.
// ---------------------------------------------------------------------------

pub struct EntityCreateBuilder<'a, T: BrainEntityType> {
    client: &'a Client,
    canonical_name: Option<String>,
    aliases: Vec<String>,
    attributes: T::Attributes,
    request_id: Option<[u8; 16]>,
}

impl<'a, T: BrainEntityType> EntityCreateBuilder<'a, T>
where
    T::Attributes: PartialEq + Eq,
{
    pub(crate) fn new(client: &'a Client) -> Self {
        Self {
            client,
            canonical_name: None,
            aliases: Vec::new(),
            attributes: T::Attributes::default(),
            request_id: None,
        }
    }

    #[must_use]
    pub fn canonical_name(mut self, name: impl Into<String>) -> Self {
        self.canonical_name = Some(name.into());
        self
    }

    #[must_use]
    pub fn alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    #[must_use]
    pub fn aliases(mut self, aliases: Vec<String>) -> Self {
        self.aliases = aliases;
        self
    }

    /// Mutate attributes in place. Lets callers set typed fields
    /// without re-constructing the whole struct.
    #[must_use]
    pub fn with(mut self, f: impl FnOnce(&mut T::Attributes)) -> Self {
        f(&mut self.attributes);
        self
    }

    #[must_use]
    pub fn attributes(mut self, attrs: T::Attributes) -> Self {
        self.attributes = attrs;
        self
    }

    #[must_use]
    pub fn request_id(mut self, id: [u8; 16]) -> Self {
        self.request_id = Some(id);
        self
    }

    pub async fn send(self) -> Result<EntityHandle<T>, ClientError> {
        let canonical_name = self
            .canonical_name
            .ok_or_else(|| ClientError::Internal("canonical_name is required for CREATE".into()))?;
        let request_id = self.request_id.unwrap_or_else(random_request_id);
        let body = RequestBody::EntityCreate(EntityCreateRequest {
            entity_type_id: T::ENTITY_TYPE_ID,
            canonical_name: canonical_name.clone(),
            aliases: self.aliases.clone(),
            attributes_blob: T::encode_attributes(&self.attributes),
            request_id,
        });
        let resp = self
            .client
            .send_knowledge_request(body, Opcode::EntityCreateReq, Opcode::EntityCreateResp)
            .await?;
        let id = match resp {
            ResponseBody::EntityCreate(r) => EntityId::from(r.entity_id),
            other => return Err(unexpected_body("EntityCreateResp", other)),
        };
        // Server returns just the id on create; round-trip a GET to
        // produce a typed handle with all derived fields populated.
        let body = RequestBody::EntityGet(EntityGetRequest {
            entity_id: id.to_bytes(),
        });
        let resp = self
            .client
            .send_knowledge_request(body, Opcode::EntityGetReq, Opcode::EntityGetResp)
            .await?;
        match resp {
            ResponseBody::EntityGet(r) => {
                EntityHandle::from_view(r.entity).map_err(map_type_mismatch)
            }
            other => Err(unexpected_body("EntityGetResp", other)),
        }
    }
}

// ---------------------------------------------------------------------------
// EntityUpdateBuilder.
// ---------------------------------------------------------------------------

/// Builder for `ENTITY_UPDATE` (full-replace semantics at the wire
/// layer per spec §28/01 §5.1).
///
/// **Field-by-field semantics in 16.8:**
///
/// - `canonical_name`, `aliases`: if unset on the builder, the
///   builder fetches the current entity via GET and inherits.
/// - `attributes`: same — if unset, GET-and-inherit. If set via
///   [`Self::attributes`], that block replaces the existing
///   attributes wholesale.
/// - The typed setters ([`Self::with`], `with_email` / `with_role` /
///   etc. for [`Person`]) **replace** rather than patch: calling
///   `.with_email("x")` starts from `T::Attributes::default()` then
///   sets email — other fields go to None. Use [`Self::attributes`]
///   with a complete struct for full control, or use the dedicated
///   [`EntityClient::rename`] for name-only changes.
///
/// Full attribute-merge semantics (per-field patching that preserves
/// untouched fields) land in phase 19 alongside the
/// `#[derive(BrainEntity)]` macro, which can introspect schema-
/// declared fields to know which slots exist.
pub struct EntityUpdateBuilder<'a, T: BrainEntityType> {
    client: &'a Client,
    entity_id: EntityId,
    canonical_name: Option<String>,
    aliases: Option<Vec<String>>,
    attributes: Option<T::Attributes>,
    request_id: Option<[u8; 16]>,
}

impl<'a, T: BrainEntityType> EntityUpdateBuilder<'a, T>
where
    T::Attributes: PartialEq + Eq,
{
    pub(crate) fn new(client: &'a Client, entity_id: EntityId) -> Self {
        Self {
            client,
            entity_id,
            canonical_name: None,
            aliases: None,
            attributes: None,
            request_id: None,
        }
    }

    #[must_use]
    pub fn canonical_name(mut self, name: impl Into<String>) -> Self {
        self.canonical_name = Some(name.into());
        self
    }

    #[must_use]
    pub fn aliases(mut self, aliases: Vec<String>) -> Self {
        self.aliases = Some(aliases);
        self
    }

    #[must_use]
    pub fn attributes(mut self, attrs: T::Attributes) -> Self {
        self.attributes = Some(attrs);
        self
    }

    #[must_use]
    pub fn with(mut self, f: impl FnOnce(&mut T::Attributes)) -> Self {
        let mut a = self.attributes.unwrap_or_default();
        f(&mut a);
        self.attributes = Some(a);
        self
    }

    #[must_use]
    pub fn request_id(mut self, id: [u8; 16]) -> Self {
        self.request_id = Some(id);
        self
    }

    pub async fn send(self) -> Result<EntityHandle<T>, ClientError> {
        // Need current state when caller didn't supply every field.
        // ENTITY_UPDATE is full-replace at the wire level, so we read
        // existing state first when any sub-field is unset.
        let need_current =
            self.canonical_name.is_none() || self.aliases.is_none() || self.attributes.is_none();
        let current: Option<EntityHandle<T>> = if need_current {
            let body = RequestBody::EntityGet(EntityGetRequest {
                entity_id: self.entity_id.to_bytes(),
            });
            match self
                .client
                .send_knowledge_request(body, Opcode::EntityGetReq, Opcode::EntityGetResp)
                .await
            {
                Ok(ResponseBody::EntityGet(r)) => {
                    Some(EntityHandle::from_view(r.entity).map_err(map_type_mismatch)?)
                }
                Ok(other) => return Err(unexpected_body("EntityGetResp", other)),
                Err(e) => return Err(e),
            }
        } else {
            None
        };

        let canonical_name = self.canonical_name.unwrap_or_else(|| {
            current
                .as_ref()
                .expect("invariant: need_current set when canonical_name unset")
                .canonical_name
                .clone()
        });
        let aliases = self.aliases.unwrap_or_else(|| {
            current
                .as_ref()
                .expect("invariant: need_current set when aliases unset")
                .aliases
                .clone()
        });
        let attributes = self.attributes.unwrap_or_else(|| {
            current
                .as_ref()
                .expect("invariant: need_current set when attributes unset")
                .attributes
                .clone()
        });
        let request_id = self.request_id.unwrap_or_else(random_request_id);

        let body = RequestBody::EntityUpdate(EntityUpdateRequest {
            entity_id: self.entity_id.to_bytes(),
            canonical_name,
            aliases,
            attributes_blob: T::encode_attributes(&attributes),
            request_id,
        });
        let resp = self
            .client
            .send_knowledge_request(body, Opcode::EntityUpdateReq, Opcode::EntityUpdateResp)
            .await?;
        match resp {
            ResponseBody::EntityUpdate(r) => {
                EntityHandle::from_view(r.entity).map_err(map_type_mismatch)
            }
            other => Err(unexpected_body("EntityUpdateResp", other)),
        }
    }
}

// ---------------------------------------------------------------------------
// EntityMergeBuilder.
// ---------------------------------------------------------------------------

pub struct EntityMergeBuilder<'a, T: BrainEntityType> {
    client: &'a Client,
    survivor: EntityId,
    merged: EntityId,
    confidence: f32,
    reason: String,
    request_id: Option<[u8; 16]>,
    _marker: PhantomData<T>,
}

impl<'a, T: BrainEntityType> EntityMergeBuilder<'a, T>
where
    T::Attributes: PartialEq + Eq,
{
    pub(crate) fn new(client: &'a Client, survivor: EntityId, merged: EntityId) -> Self {
        Self {
            client,
            survivor,
            merged,
            confidence: 0.9,
            reason: String::new(),
            request_id: None,
            _marker: PhantomData,
        }
    }

    #[must_use]
    pub fn confidence(mut self, c: f32) -> Self {
        self.confidence = c;
        self
    }

    #[must_use]
    pub fn reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = reason.into();
        self
    }

    #[must_use]
    pub fn request_id(mut self, id: [u8; 16]) -> Self {
        self.request_id = Some(id);
        self
    }

    /// Returns the merge audit id + grace period seconds.
    pub async fn send(self) -> Result<MergeOutcome, ClientError> {
        let body = RequestBody::EntityMerge(EntityMergeRequest {
            survivor: self.survivor.to_bytes(),
            merged: self.merged.to_bytes(),
            confidence: self.confidence,
            reason: self.reason,
            request_id: self.request_id.unwrap_or_else(random_request_id),
        });
        let resp = self
            .client
            .send_knowledge_request(body, Opcode::EntityMergeReq, Opcode::EntityMergeResp)
            .await?;
        match resp {
            ResponseBody::EntityMerge(r) => Ok(MergeOutcome {
                audit_id: r.audit_id,
                grace_period_seconds: r.grace_period_seconds,
            }),
            other => Err(unexpected_body("EntityMergeResp", other)),
        }
    }
}

/// Returned by [`EntityMergeBuilder::send`]. Carries the merge audit
/// id and the grace-period window during which `unmerge()` can revert.
#[derive(Clone, Copy, Debug)]
pub struct MergeOutcome {
    pub audit_id: [u8; 16],
    pub grace_period_seconds: u64,
}

// ---------------------------------------------------------------------------
// EntityResolveBuilder.
// ---------------------------------------------------------------------------

pub struct EntityResolveBuilder<'a, T: BrainEntityType> {
    client: &'a Client,
    candidate_name: String,
    context: String,
    allow_create: bool,
    request_id: Option<[u8; 16]>,
    _marker: PhantomData<T>,
}

impl<'a, T: BrainEntityType> EntityResolveBuilder<'a, T>
where
    T::Attributes: PartialEq + Eq,
{
    pub(crate) fn new(client: &'a Client, candidate_name: String) -> Self {
        Self {
            client,
            candidate_name,
            context: String::new(),
            allow_create: false,
            request_id: None,
            _marker: PhantomData,
        }
    }

    /// Surrounding text used by tier 3 (embedding) when phase 21+
    /// wires the entity HNSW. Currently informational only.
    #[must_use]
    pub fn context(mut self, ctx: impl Into<String>) -> Self {
        self.context = ctx.into();
        self
    }

    /// Allow tier-5 auto-create when no entity resolves. Phase 16.8
    /// behavior: returns [`ResolutionOutcome::NotFound`] regardless;
    /// auto-create wires when statement extraction lands (phase 17+).
    #[must_use]
    pub fn allow_create(mut self, allow: bool) -> Self {
        self.allow_create = allow;
        self
    }

    #[must_use]
    pub fn request_id(mut self, id: [u8; 16]) -> Self {
        self.request_id = Some(id);
        self
    }

    pub async fn send(self) -> Result<ResolutionOutcome<T>, ClientError> {
        let body = RequestBody::EntityResolve(EntityResolveRequest {
            candidate_name: self.candidate_name,
            context: self.context,
            entity_type_hint: T::ENTITY_TYPE_ID,
            allow_create: self.allow_create,
            request_id: self.request_id.unwrap_or_else(random_request_id),
        });
        let resp = self
            .client
            .send_knowledge_request(body, Opcode::EntityResolveReq, Opcode::EntityResolveResp)
            .await?;
        let r = match resp {
            ResponseBody::EntityResolve(r) => r,
            other => return Err(unexpected_body("EntityResolveResp", other)),
        };
        match r.outcome {
            ResolutionOutcomeWire::Resolved => Ok(ResolutionOutcome::Resolved {
                entity_id: EntityId::from(r.resolved_entity),
                tier: r.tier,
                confidence: r.confidence,
                _marker: PhantomData,
            }),
            ResolutionOutcomeWire::Created => Ok(ResolutionOutcome::Created {
                entity_id: EntityId::from(r.resolved_entity),
                _marker: PhantomData,
            }),
            ResolutionOutcomeWire::Ambiguous => Ok(ResolutionOutcome::Ambiguous {
                candidates: r
                    .candidate_ids
                    .into_iter()
                    .map(EntityId::from)
                    .collect(),
                audit_id: r.audit_id,
                tier: r.tier,
                confidence: r.confidence,
                _marker: PhantomData,
            }),
            ResolutionOutcomeWire::NotFound => Ok(ResolutionOutcome::NotFound),
        }
    }
}

/// SDK-level resolution outcome. Generic over the entity type so
/// callers can downstream-`get()` with a typed `EntityHandle<T>`
/// without re-asserting the entity type.
///
/// Phase 16.8.7: `Resolved` / `Ambiguous` / `NotFound` are the
/// reachable variants. `Created` lights up when phase 17+ wires
/// resolver-driven auto-create through statement extraction.
#[derive(Clone, Debug)]
pub enum ResolutionOutcome<T: BrainEntityType> {
    Resolved {
        entity_id: EntityId,
        tier: u8,
        confidence: f32,
        _marker: PhantomData<T>,
    },
    Created {
        entity_id: EntityId,
        _marker: PhantomData<T>,
    },
    Ambiguous {
        candidates: Vec<EntityId>,
        audit_id: [u8; 16],
        tier: u8,
        confidence: f32,
        _marker: PhantomData<T>,
    },
    NotFound,
}

impl<T: BrainEntityType> ResolutionOutcome<T> {
    /// Extract the resolved entity id if `Resolved` or `Created`.
    #[must_use]
    pub fn entity_id(&self) -> Option<EntityId> {
        match self {
            Self::Resolved { entity_id, .. } | Self::Created { entity_id, .. } => Some(*entity_id),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_resolved(&self) -> bool {
        matches!(self, Self::Resolved { .. })
    }

    #[must_use]
    pub fn is_ambiguous(&self) -> bool {
        matches!(self, Self::Ambiguous { .. })
    }
}

// ---------------------------------------------------------------------------
// EntityListBuilder.
// ---------------------------------------------------------------------------

pub struct EntityListBuilder<'a, T: BrainEntityType> {
    client: &'a Client,
    name_prefix: String,
    mention_count_min: u32,
    include_tombstoned: bool,
    include_merged: bool,
    limit: u32,
    _marker: PhantomData<T>,
}

impl<'a, T: BrainEntityType> EntityListBuilder<'a, T>
where
    T::Attributes: PartialEq + Eq,
{
    pub(crate) fn new(client: &'a Client) -> Self {
        Self {
            client,
            name_prefix: String::new(),
            mention_count_min: 0,
            include_tombstoned: false,
            include_merged: false,
            limit: 100,
            _marker: PhantomData,
        }
    }

    #[must_use]
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.name_prefix = prefix.into();
        self
    }

    #[must_use]
    pub fn min_mentions(mut self, n: u32) -> Self {
        self.mention_count_min = n;
        self
    }

    #[must_use]
    pub fn include_tombstoned(mut self, yes: bool) -> Self {
        self.include_tombstoned = yes;
        self
    }

    #[must_use]
    pub fn include_merged(mut self, yes: bool) -> Self {
        self.include_merged = yes;
        self
    }

    #[must_use]
    pub fn limit(mut self, n: u32) -> Self {
        self.limit = n;
        self
    }

    /// Fetch the (single-page) snapshot. Cursor pagination lands in
    /// phase 23 — currently any non-empty cursor would be rejected by
    /// the server.
    pub async fn fetch(self) -> Result<Vec<EntityHandle<T>>, ClientError> {
        let body = RequestBody::EntityList(EntityListRequest {
            entity_type_id: T::ENTITY_TYPE_ID,
            name_prefix: self.name_prefix,
            mention_count_min: self.mention_count_min,
            include_tombstoned: self.include_tombstoned,
            include_merged: self.include_merged,
            limit: self.limit,
            cursor: Vec::new(),
        });
        let resp = self
            .client
            .send_knowledge_request(body, Opcode::EntityListReq, Opcode::EntityListResp)
            .await?;
        let frame: EntityListResponseFrame = match resp {
            ResponseBody::EntityList(f) => f,
            other => return Err(unexpected_body("EntityListResp", other)),
        };
        let mut out = Vec::with_capacity(frame.items.len());
        for item in frame.items {
            out.push(EntityHandle::from_view(item.entity).map_err(map_type_mismatch)?);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Client integration — send_knowledge_request internal helper.
// ---------------------------------------------------------------------------

impl Client {
    /// Typed entry point for entity operations. Use turbofish or
    /// type ascription to pick the entity type:
    ///
    /// ```no_run
    /// # use brain_sdk_rust::{Client, Person};
    /// # async fn ex(client: Client) -> Result<(), brain_sdk_rust::ClientError> {
    /// let handle = client.entity::<Person>()
    ///     .create()
    ///     .canonical_name("Alice")
    ///     .send()
    ///     .await?;
    /// # let _ = handle;
    /// # Ok(()) }
    /// ```
    #[must_use]
    pub fn entity<T: BrainEntityType>(&self) -> EntityClient<'_, T>
    where
        T::Attributes: PartialEq + Eq,
    {
        EntityClient::new(self)
    }

    /// Internal: send a knowledge entity request and read one
    /// response frame. Used by every builder above.
    pub(crate) async fn send_knowledge_request(
        &self,
        body: RequestBody,
        req_opcode: Opcode,
        resp_opcode: Opcode,
    ) -> Result<ResponseBody, ClientError> {
        let payload = body.encode();
        let mut guard = self.acquire().await?;
        let stream_id = guard.next_stream_id();
        let frame = Frame::new(req_opcode.as_u16(), FLAG_EOS, stream_id, payload);
        let resp_frame = send_and_read_one(&mut guard, frame, resp_opcode).await?;
        ResponseBody::decode(resp_opcode, &resp_frame.payload).map_err(ClientError::Protocol)
    }
}

// ---------------------------------------------------------------------------
// Person-specific attribute setters — sugar over with(|p| p.email = ...).
// ---------------------------------------------------------------------------

impl<'a> EntityCreateBuilder<'a, Person> {
    #[must_use]
    pub fn with_email(self, email: impl Into<String>) -> Self {
        self.with(|p| p.email = Some(email.into()))
    }

    #[must_use]
    pub fn with_role(self, role: impl Into<String>) -> Self {
        self.with(|p| p.role = Some(role.into()))
    }

    #[must_use]
    pub fn with_team(self, team: impl Into<String>) -> Self {
        self.with(|p| p.team = Some(team.into()))
    }

    #[must_use]
    pub fn with_timezone(self, tz: impl Into<String>) -> Self {
        self.with(|p| p.timezone = Some(tz.into()))
    }
}

impl<'a> EntityUpdateBuilder<'a, Person> {
    #[must_use]
    pub fn with_email(self, email: impl Into<String>) -> Self {
        self.with(|p| p.email = Some(email.into()))
    }

    #[must_use]
    pub fn with_role(self, role: impl Into<String>) -> Self {
        self.with(|p| p.role = Some(role.into()))
    }

    #[must_use]
    pub fn with_team(self, team: impl Into<String>) -> Self {
        self.with(|p| p.team = Some(team.into()))
    }

    #[must_use]
    pub fn with_timezone(self, tz: impl Into<String>) -> Self {
        self.with(|p| p.timezone = Some(tz.into()))
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn random_request_id() -> [u8; 16] {
    *uuid::Uuid::now_v7().as_bytes()
}

fn map_type_mismatch(err: EntityHandleFromViewError) -> ClientError {
    ClientError::Internal(format!("entity type mismatch: {err}"))
}

fn unexpected_body(expected: &str, body: ResponseBody) -> ClientError {
    ClientError::Protocol(brain_protocol::error::ProtocolError::BadFrame(format!(
        "expected {expected}, got {:?}",
        std::mem::discriminant(&body)
    )))
}

/// Returns `true` iff `err` is a server-side ENTITY_NOT_FOUND. Used
/// by `get()` to translate "not found" into `Ok(None)`. Delegates to
/// [`ClientErrorEntityExt`] which inspects the server's message
/// (Strategy B per spec §28/03 §2). Strategy A migration is a
/// follow-up tracked in §28/09 Q1.
fn is_entity_not_found(err: &ClientError) -> bool {
    err.is_entity_not_found()
}
