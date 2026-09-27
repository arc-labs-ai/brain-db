//! Delegated (`act_as`) capabilities + schema ops.
//!
//! The shared-pool gateway authenticates ONE service principal and runs each
//! tenant's op under a per-request `act_as`. The server materializes the
//! effective caller (`RequestScope::to_effective_caller`): namespace/space of
//! the tenant, permissions `STANDARD_SPACE | (grant & delegator & DELEGABLE)`.
//! This file drives the real `dispatch` → handler path with exactly those
//! effective callers on ONE shard's metadata and proves:
//!
//!   - schema GET / VALIDATE / UPLOAD / REPLACE bind to the effective
//!     (tenant) namespace, and a delegated caller cannot read or write another
//!     tenant's schema;
//!   - without a grant a delegated caller keeps the historical
//!     `STANDARD_SPACE` rights (reads + validate only — no UPLOAD / REPLACE);
//!     with `SCHEMA_UPLOAD` / `ADMIN` granted it may upload / replace;
//!   - `GET_CAPABILITIES.schema_namespaces` lists only the caller's own
//!     namespace — never the roster of tenants on the shard.
//!
//! The wire-level gate (R1/R2 + the grant-bound check) and the effective
//! caller's permission arithmetic are covered in brain-server
//! (`network::auth` unit tests and `tests/act_as_isolation.rs`).

#![cfg(target_os = "linux")]

use std::sync::Arc;

use brain_core::SpaceId;
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::api_keys::bits;
use brain_metadata::MetadataDb;
use brain_ops::test_support::{run_in_glommio, single_body};
use brain_ops::{dispatch, OpError, OpsContext, RealWriterHandle, RequestCaller};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::{GetCapabilitiesRequest, ResponseBody};
use brain_protocol::{
    ActAs, SchemaGetRequest, SchemaReplaceRequest, SchemaUploadRequest, SchemaValidateRequest,
};

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

const ACME_V1: &str =
    "namespace acme\ndefine predicate prefers { kind: Preference object: Value<text> }\n";
const ACME_V2: &str =
    "namespace acme\ndefine predicate avoids { kind: Preference object: Value<text> }\n";
const GLOBEX_V1: &str =
    "namespace globex\ndefine predicate dislikes { kind: Preference object: Value<text> }\n";

fn build_ctx() -> (tempfile::TempDir, OpsContext) {
    let dir = tempfile::tempdir().unwrap();
    let metadata: SharedMetadataDb =
        Arc::new(MetadataDb::open(dir.path().join("metadata.redb")).unwrap());
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
    let executor = ExecutorContext::new(
        Arc::new(NopDispatcher) as Arc<dyn Dispatcher>,
        shared,
        metadata,
        writer as Arc<dyn WriterHandle>,
    );
    let ctx = brain_ops::test_support::ops_context_for_tests(executor, dir.path());
    (dir, ctx)
}

/// The wire selector the gateway sends for `namespace`.
fn act_as(namespace: &str, grant: u32) -> Option<ActAs> {
    Some(ActAs {
        namespace: namespace.into(),
        space_id: format!("{namespace}:gateway-user"),
        grant,
    })
}

/// The effective caller the server materializes for `act_as(namespace,
/// grant)` from a delegator holding every delegable bit — mirrors
/// `RequestScope::to_effective_caller` + `delegated_permissions`.
fn delegated(namespace: &str, grant: u32) -> RequestCaller {
    let space = format!("{namespace}:gateway-user");
    RequestCaller::from_scope(
        SpaceId::derive_from_string(namespace, &space),
        [0x6A; 16],
        [0u8; 16],
        namespace.into(),
        bits::STANDARD_SPACE | (grant & bits::DELEGABLE),
    )
    .with_space_string(space)
}

async fn upload(
    ctx: &OpsContext,
    caller: RequestCaller,
    doc: &str,
    grant: u32,
    rid: u8,
) -> Result<u32, OpError> {
    let ns = caller.namespace.clone();
    let out = dispatch(
        RequestBody::SchemaUpload(SchemaUploadRequest {
            schema_document: doc.into(),
            dry_run: false,
            allow_breaking: false,
            request_id: [rid; 16],
            act_as: act_as(&ns, grant),
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

async fn get(ctx: &OpsContext, caller: RequestCaller, namespace: &str) -> Result<u32, OpError> {
    let ns = caller.namespace.clone();
    let out = dispatch(
        RequestBody::SchemaGet(SchemaGetRequest {
            namespace: namespace.into(),
            version: 0,
            act_as: act_as(&ns, 0),
        }),
        caller,
        ctx,
    )
    .await?;
    match single_body(out) {
        ResponseBody::SchemaGet(r) => {
            assert_eq!(r.namespace, namespace);
            Ok(r.schema_version)
        }
        other => panic!("expected SchemaGet, got {other:?}"),
    }
}

async fn capabilities(ctx: &OpsContext, caller: RequestCaller) -> Vec<String> {
    let ns = caller.namespace.clone();
    let out = dispatch(
        RequestBody::GetCapabilities(GetCapabilitiesRequest {
            act_as: if ns.is_empty() { None } else { act_as(&ns, 0) },
        }),
        caller,
        ctx,
    )
    .await
    .unwrap();
    match single_body(out) {
        ResponseBody::GetCapabilities(r) => {
            let mut v = r.capabilities.schema_namespaces;
            v.sort();
            v
        }
        other => panic!("expected GetCapabilities, got {other:?}"),
    }
}

fn assert_unauthorized<T: std::fmt::Debug>(r: Result<T, OpError>, what: &str) {
    match r {
        Err(OpError::Unauthorized(_)) => {}
        other => panic!("{what}: expected Unauthorized, got {other:?}"),
    }
}

#[test]
fn delegated_schema_ops_without_grant_keep_standard_rights() {
    run_in_glommio(|| async {
        let (_dir, ctx) = build_ctx();

        // No grant → STANDARD_SPACE: no SCHEMA_UPLOAD, no ADMIN. Exactly
        // the pre-grant behaviour.
        assert_unauthorized(
            upload(&ctx, delegated("acme", 0), ACME_V1, 0, 1).await,
            "upload without grant",
        );
        let replace = dispatch(
            RequestBody::SchemaReplace(SchemaReplaceRequest {
                schema_document: ACME_V1.into(),
                force_drop_existing: true,
                request_id: [2; 16],
                act_as: act_as("acme", 0),
            }),
            delegated("acme", 0),
            &ctx,
        )
        .await;
        assert_unauthorized(replace, "replace without grant");

        // Reads + validate ride RECALL, which STANDARD_SPACE holds, and bind
        // to the effective namespace.
        let validate = dispatch(
            RequestBody::SchemaValidate(SchemaValidateRequest {
                schema_document: ACME_V1.into(),
                act_as: act_as("acme", 0),
            }),
            delegated("acme", 0),
            &ctx,
        )
        .await
        .expect("delegated validate of own namespace");
        match single_body(validate) {
            ResponseBody::SchemaValidate(r) => {
                assert!(r.validation_errors.is_empty());
                assert_eq!(r.namespace, "acme");
                assert_eq!(r.would_be_version, 1);
            }
            other => panic!("expected SchemaValidate, got {other:?}"),
        }
    })
}

#[test]
fn delegated_schema_ops_with_grant_are_scoped_to_the_tenant() {
    run_in_glommio(|| async {
        let (_dir, ctx) = build_ctx();
        let su = bits::SCHEMA_UPLOAD;

        // Granted SCHEMA_UPLOAD: each tenant uploads its own schema.
        let v = upload(&ctx, delegated("acme", su), ACME_V1, su, 10)
            .await
            .expect("acme upload");
        assert_eq!(v, 1);
        upload(&ctx, delegated("globex", su), GLOBEX_V1, su, 11)
            .await
            .expect("globex upload");

        // Cross-tenant write: acme's delegated caller submitting globex's DSL.
        assert_unauthorized(
            upload(&ctx, delegated("acme", su), GLOBEX_V1, su, 12).await,
            "cross-tenant upload",
        );

        // Own-namespace read works; a foreign namespace is refused by the
        // dispatch namespace gate (bound to the EFFECTIVE namespace) before
        // the handler ever reads it.
        assert_eq!(get(&ctx, delegated("acme", 0), "acme").await.unwrap(), 1);
        assert_unauthorized(
            get(&ctx, delegated("acme", 0), "globex").await,
            "cross-tenant get",
        );
        // Validate of a foreign DSL is rejected too (no version oracle).
        let foreign_validate = dispatch(
            RequestBody::SchemaValidate(SchemaValidateRequest {
                schema_document: GLOBEX_V1.into(),
                act_as: act_as("acme", 0),
            }),
            delegated("acme", 0),
            &ctx,
        )
        .await;
        assert_unauthorized(foreign_validate, "cross-tenant validate");

        // REPLACE needs ADMIN: SCHEMA_UPLOAD alone is not enough ...
        let replace = |grant: u32, doc: &'static str, rid: u8| {
            dispatch(
                RequestBody::SchemaReplace(SchemaReplaceRequest {
                    schema_document: doc.into(),
                    force_drop_existing: true,
                    request_id: [rid; 16],
                    act_as: act_as("acme", grant),
                }),
                delegated("acme", grant),
                &ctx,
            )
        };
        assert_unauthorized(replace(su, ACME_V2, 20).await, "replace with only SU");
        // ... granted ADMIN may replace its own namespace ...
        let out = replace(bits::ADMIN, ACME_V2, 21)
            .await
            .expect("replace with ADMIN grant");
        match single_body(out) {
            ResponseBody::SchemaReplace(r) => {
                assert!(r.validation_errors.is_empty(), "{:?}", r.validation_errors);
                assert_eq!(r.namespace, "acme");
                assert!(r.schema_version >= 2, "replace bumps the version");
            }
            other => panic!("expected SchemaReplace, got {other:?}"),
        }
        // ... but never another tenant's, even with ADMIN.
        assert_unauthorized(
            replace(bits::ADMIN, GLOBEX_V1, 22).await,
            "cross-tenant replace",
        );
        // globex is untouched.
        assert_eq!(
            get(&ctx, delegated("globex", 0), "globex").await.unwrap(),
            1
        );
    })
}

#[test]
fn capabilities_list_only_the_callers_own_schema_namespace() {
    run_in_glommio(|| async {
        let (_dir, ctx) = build_ctx();
        let su = bits::SCHEMA_UPLOAD;
        upload(&ctx, delegated("acme", su), ACME_V1, su, 30)
            .await
            .unwrap();
        upload(&ctx, delegated("globex", su), GLOBEX_V1, su, 31)
            .await
            .unwrap();

        // Delegated callers see only their own tenant.
        assert_eq!(capabilities(&ctx, delegated("acme", 0)).await, ["acme"]);
        assert_eq!(capabilities(&ctx, delegated("globex", 0)).await, ["globex"]);
        // A tenant with no schema learns nothing about the others.
        assert!(capabilities(&ctx, delegated("initech", 0)).await.is_empty());

        // A direct (non-delegated) key-bound caller is filtered the same way.
        let direct = RequestCaller::from_scope(
            SpaceId::derive_from_string("globex", "direct"),
            [0u8; 16],
            [0u8; 16],
            "globex".into(),
            bits::STANDARD_SPACE,
        );
        assert_eq!(capabilities(&ctx, direct).await, ["globex"]);

        // The test-only no-namespace-lock caller still sees the full list.
        assert_eq!(
            capabilities(&ctx, RequestCaller::for_tests()).await,
            ["acme", "globex"]
        );
    })
}
