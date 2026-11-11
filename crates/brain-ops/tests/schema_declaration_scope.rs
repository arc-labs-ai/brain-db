//! Where a `SCHEMA_UPLOAD` declares: the namespace base, or one space.
//!
//! Entity-type declarations resolve narrowest-first — `(namespace, space)` →
//! `(namespace, base)` → `(system, base)` — so an upload can either extend
//! the base every space in the tenant reads, or lay an overlay only one space
//! reads. Which one it does is a field on the request, and this file exists
//! because getting that wrong is silent in both directions.
//!
//! The requests here all run through the real `dispatch` → handler path with
//! an effective caller the gateway would materialize from `act_as`, i.e. with
//! a space already named — which is the normal case, not the exotic one. That
//! is the whole reason the scope cannot be read off `act_as.space`:
//!
//!   - `scope` omitted (`SchemaScope::Namespace`, the serde default and what
//!     every already-encoded request means) declares at the base, even though
//!     the caller named a space. Inferring the scope from `act_as` would turn
//!     every ordinary tenant upload into a per-space overlay that the
//!     tenant's other spaces cannot see.
//!   - `scope: Space` declares at the caller's own space and nowhere else: a
//!     sibling space does not resolve it, and neither does the base.
//!   - A caller supplies a two-state selector, never a space id, so it can
//!     only ever target the space dispatch already authorized for the
//!     request.

#![cfg(target_os = "linux")]

use std::sync::Arc;

use brain_core::SpaceId;
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::api_keys::bits;
use brain_metadata::entity::types::entity_type_lookup_rtxn;
use brain_metadata::tables::scope::SPACE_BASE;
use brain_metadata::MetadataDb;
use brain_ops::test_support::{run_in_glommio, single_body};
use brain_ops::{dispatch, OpError, OpsContext, RealWriterHandle, RequestCaller};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::{ActAs, SchemaScope, SchemaUploadRequest};

struct NopDispatcher;

impl Dispatcher for NopDispatcher {
    fn embed(&self, _: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        Ok([0.0; VECTOR_DIM])
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        Ok(vec![[0.0; VECTOR_DIM]; texts.len()])
    }
    fn fingerprint(&self) -> [u8; 16] {
        [0; 16]
    }
}

const NS: &str = "acme";
/// One entity type per document, with distinct names, so each assertion is
/// about the scope a declaration landed at and never about the merge
/// pre-flight classifying a re-declaration.
const BASE_DOC: &str = "namespace acme\ndefine entity_type Invoice { attributes {} }\n";
const OVERLAY_DOC: &str = "namespace acme\ndefine entity_type Ticket { attributes {} }\n";

fn build_ctx() -> (tempfile::TempDir, OpsContext, SharedMetadataDb) {
    let dir = tempfile::tempdir().unwrap();
    let metadata: SharedMetadataDb =
        Arc::new(MetadataDb::open(dir.path().join("metadata.redb")).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata.clone(),
        writer as Arc<dyn WriterHandle>,
    );
    let ctx = brain_ops::test_support::ops_context_for_tests(executor, dir.path());
    (dir, ctx, metadata)
}

/// The wire selector a shared-pool gateway sends for one of its end users.
fn act_as(space: &str) -> Option<ActAs> {
    Some(ActAs {
        namespace: NS.into(),
        space: space.into(),
        grant: bits::SCHEMA_UPLOAD,
    })
}

/// The effective caller the server materializes for `act_as(space)`.
fn caller_in(space: &str) -> RequestCaller {
    RequestCaller::from_scope(
        SpaceId::derive_from_string(NS, space),
        [0x6A; 16],
        [0u8; 16],
        NS.into(),
        bits::STANDARD_SPACE | bits::SCHEMA_UPLOAD,
    )
    .with_space_string(space.into())
}

async fn upload(
    ctx: &OpsContext,
    caller: RequestCaller,
    space_selector: &str,
    doc: &str,
    scope: SchemaScope,
    rid: u8,
) -> Result<u32, OpError> {
    let out = dispatch(
        RequestBody::SchemaUpload(SchemaUploadRequest {
            schema_document: doc.into(),
            dry_run: false,
            allow_breaking: false,
            request_id: [rid; 16],
            act_as: act_as(space_selector),
            scope,
        }),
        caller,
        ctx,
    )
    .await?;
    match single_body(out) {
        ResponseBody::SchemaUpload(r) => {
            assert!(r.validation_errors.is_empty(), "{:?}", r.validation_errors);
            Ok(r.schema_version)
        }
        other => panic!("expected SchemaUpload, got {other:?}"),
    }
}

/// Does `name` resolve for a reader in `space`? Walks the real overlay chain,
/// so this is the question a reader asks, not a scope-exact probe.
fn resolves_in(metadata: &SharedMetadataDb, space: [u8; 16], name: &str) -> bool {
    let rtxn = metadata.read_txn().unwrap();
    let ns = brain_metadata::namespace::namespace_lookup_by_name(&rtxn, NS)
        .unwrap()
        .expect("the upload interns its namespace");
    entity_type_lookup_rtxn(
        &rtxn,
        brain_metadata::TypeScope::space(ns.raw(), space),
        name,
    )
    .unwrap()
    .is_some()
}

fn space_bytes(selector: &str) -> [u8; 16] {
    SpaceId::derive_from_string(NS, selector).into()
}

/// The default scope is the namespace base — including for a caller that
/// named a space, which is nearly every caller.
///
/// This is the regression that the explicit opt-in exists to prevent. The
/// request below is byte-identical to one written before the `scope` field
/// existed, and it has to keep meaning what it meant: a declaration the
/// tenant's other spaces can see. Deriving the scope from `act_as.space`
/// would flip this single assertion, and with it every tenant's schema.
#[test]
fn upload_without_a_scope_declares_at_the_namespace_base() {
    run_in_glommio(|| async {
        let (_dir, ctx, metadata) = build_ctx();

        let v = upload(
            &ctx,
            caller_in("gateway:user-a"),
            "gateway:user-a",
            BASE_DOC,
            SchemaScope::Namespace,
            1,
        )
        .await
        .expect("default-scope upload");
        assert_eq!(v, 1);

        assert!(
            resolves_in(&metadata, SPACE_BASE, "Invoice"),
            "the default scope is the namespace base"
        );
        assert!(
            resolves_in(&metadata, space_bytes("gateway:user-a"), "Invoice"),
            "the uploading space still resolves it, through the base"
        );
        assert!(
            resolves_in(&metadata, space_bytes("gateway:user-b"), "Invoice"),
            "and so does every sibling space — that is what a base declaration is"
        );
    })
}

/// An explicit `scope: Space` lands in the caller's space and nowhere else.
#[test]
fn upload_with_space_scope_is_invisible_to_a_sibling_space() {
    run_in_glommio(|| async {
        let (_dir, ctx, metadata) = build_ctx();

        upload(
            &ctx,
            caller_in("gateway:user-a"),
            "gateway:user-a",
            OVERLAY_DOC,
            SchemaScope::Space,
            2,
        )
        .await
        .expect("space-scoped upload");

        assert!(
            resolves_in(&metadata, space_bytes("gateway:user-a"), "Ticket"),
            "the declaring space must resolve its own overlay"
        );
        assert!(
            !resolves_in(&metadata, SPACE_BASE, "Ticket"),
            "an overlay must not land at the namespace base"
        );
        assert!(
            !resolves_in(&metadata, space_bytes("gateway:user-b"), "Ticket"),
            "a sibling space must not see another space's overlay"
        );
    })
}

/// The two scopes are independent: a base declaration and an overlay coexist,
/// each visible exactly where it was declared.
///
/// Worth asserting together rather than only apart, because the registry key
/// is shared — a scope that was being ignored somewhere would show up here as
/// one of the two uploads shadowing or erasing the other.
#[test]
fn a_base_declaration_and_a_space_overlay_coexist_at_their_own_scopes() {
    run_in_glommio(|| async {
        let (_dir, ctx, metadata) = build_ctx();

        upload(
            &ctx,
            caller_in("gateway:user-a"),
            "gateway:user-a",
            BASE_DOC,
            SchemaScope::Namespace,
            3,
        )
        .await
        .expect("base upload");
        upload(
            &ctx,
            caller_in("gateway:user-a"),
            "gateway:user-a",
            OVERLAY_DOC,
            SchemaScope::Space,
            4,
        )
        .await
        .expect("overlay upload");

        let a = space_bytes("gateway:user-a");
        let b = space_bytes("gateway:user-b");

        // The declaring space sees both.
        assert!(resolves_in(&metadata, a, "Invoice"));
        assert!(resolves_in(&metadata, a, "Ticket"));
        // A sibling space sees the base one only.
        assert!(resolves_in(&metadata, b, "Invoice"));
        assert!(!resolves_in(&metadata, b, "Ticket"));
        // And the base itself carries only what was declared at it.
        assert!(resolves_in(&metadata, SPACE_BASE, "Invoice"));
        assert!(!resolves_in(&metadata, SPACE_BASE, "Ticket"));
    })
}

// The remaining case — `scope: Space` from a caller with no effective space —
// cannot be driven from here. `SpaceId::NIL` is dispatch's "anonymous request"
// marker, so a caller with no space never reaches a schema handler bound to a
// tenant namespace at all. It is covered against the handler directly, in
// `brain_ops::handlers::schema`'s
// `space_scope_without_an_effective_space_is_refused`.
