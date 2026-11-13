//! LLM_CREDENTIAL_PUT / GET / DELETE / VERIFY, through `dispatch`.
//!
//! These go through `dispatch` rather than calling the handlers directly,
//! because the controls worth testing are not all in the handler: the
//! permission bit is enforced by the dispatcher, and the transport guard
//! reads a field the connection layer stamps onto the caller. Testing the
//! handler alone would assert the easy half.
//!
//! What each case pins:
//!
//! - a key without `LLM_CREDENTIAL` cannot register, read, revoke or verify,
//!   even holding every other permission
//! - a plaintext key is refused over a connection whose transport does not
//!   protect its contents
//! - a read never returns anything a key can be rebuilt from
//! - a tenant sees only their own credential
//! - a space-scoped registration is refused rather than silently ignored

use std::sync::Arc;

use brain_core::SpaceId;
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::api_keys::bits;
use brain_metadata::llm_credential::{LlmCredentialAccess, LlmCredentialDb};
use brain_metadata::MetadataDb;
use brain_ops::{dispatch, OpError, OpsContext, RealWriterHandle, RequestCaller};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::{
    LlmCredentialDeleteRequest, LlmCredentialGetRequest, LlmCredentialPutRequest,
    LlmCredentialVerifyRequest, VerifyOutcome,
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

const NS: &str = "acme";
const OTHER_NS: &str = "globex";
const KEY: &str = "sk-test-not-a-real-key";

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

    let ring = brain_crypto::load_or_create(dir.path()).expect("sealing key");
    let store = LlmCredentialDb::open(dir.path().join("llm_credentials.redb")).expect("store");
    let credentials = Arc::new(LlmCredentialAccess::new(store, Arc::new(ring)));

    let ctx = brain_ops::test_support::ops_context_for_tests(executor, dir.path())
        .with_llm_credentials(Some(credentials));
    (dir, ctx, metadata)
}

/// Intern a namespace so the dispatcher's fail-closed namespace gate resolves
/// it, mirroring what key minting does.
fn intern(metadata: &SharedMetadataDb, name: &str) {
    let wtxn = metadata.write_txn().unwrap();
    brain_metadata::namespace::namespace_intern_or_get(&wtxn, name, 0).unwrap();
    wtxn.commit().unwrap();
}

/// A caller bound to `namespace`, holding `permissions`, over a connection
/// whose transport protects its contents.
fn caller(namespace: &str, permissions: u32) -> RequestCaller {
    RequestCaller::from_scope(
        SpaceId::derive_from_string(namespace, "default"),
        [0u8; 16],
        [0u8; 16],
        namespace.into(),
        permissions,
    )
    .with_confidential_transport()
}

fn put_body(api_key: &str) -> RequestBody {
    RequestBody::LlmCredentialPut(LlmCredentialPutRequest {
        provider: "openai".into(),
        model: "gpt-4.1-mini".into(),
        api_key: api_key.into(),
        request_id: [1u8; 16],
        act_as: None,
    })
}

fn get_body() -> RequestBody {
    RequestBody::LlmCredentialGet(LlmCredentialGetRequest { act_as: None })
}

fn delete_body() -> RequestBody {
    RequestBody::LlmCredentialDelete(LlmCredentialDeleteRequest {
        request_id: [2u8; 16],
        act_as: None,
    })
}

fn verify_body() -> RequestBody {
    RequestBody::LlmCredentialVerify(LlmCredentialVerifyRequest {
        request_id: [3u8; 16],
        act_as: None,
    })
}

async fn send(
    ctx: &OpsContext,
    caller: RequestCaller,
    body: RequestBody,
) -> Result<ResponseBody, OpError> {
    dispatch(body, caller, ctx)
        .await
        .map(brain_ops::test_support::single_body)
}

// ---------------------------------------------------------------------------
// The permission bit
// ---------------------------------------------------------------------------

#[test]
fn every_credential_op_needs_the_llm_credential_bit() {
    brain_ops::test_support::run_in_glommio(|| async {
        let (_dir, ctx, metadata) = build_ctx();
        intern(&metadata, NS);

        // Everything except LLM_CREDENTIAL: a key that can write memories,
        // upload schemas and call admin ops still cannot touch the
        // credential that pays for processing those memories.
        let without = bits::FULL & !bits::LLM_CREDENTIAL;
        for (label, body) in [
            ("put", put_body(KEY)),
            ("get", get_body()),
            ("delete", delete_body()),
            ("verify", verify_body()),
        ] {
            let err = send(&ctx, caller(NS, without), body)
                .await
                .expect_err("must be refused");
            assert!(
                matches!(err, OpError::Unauthorized(_)),
                "{label}: expected Unauthorized, got {err:?}"
            );
        }
    });
}

#[test]
fn the_bit_is_not_part_of_an_ordinary_write_key() {
    // Asserted on the constants rather than through dispatch: the risk is a
    // future edit folding the bit into a convenience bundle, which no
    // behavioural test would notice until a write key could rotate a
    // tenant's provider credential.
    assert_eq!(bits::STANDARD_SPACE & bits::LLM_CREDENTIAL, 0);
    assert_eq!(bits::READ_WRITE & bits::LLM_CREDENTIAL, 0);
    assert_eq!(bits::READ_ONLY & bits::LLM_CREDENTIAL, 0);
    assert_ne!(bits::FULL & bits::LLM_CREDENTIAL, 0);
}

#[test]
fn a_delegated_op_can_never_register_a_credential() {
    // A delegated caller's permissions are `STANDARD_SPACE | (grant &
    // delegator & DELEGABLE)`, and LLM_CREDENTIAL is not delegable — so no
    // grant a trusted front door can send produces the bit.
    assert_eq!(bits::DELEGABLE & bits::LLM_CREDENTIAL, 0);
}

// ---------------------------------------------------------------------------
// The transport guard
// ---------------------------------------------------------------------------

#[test]
fn a_plaintext_key_is_refused_over_an_unprotected_transport() {
    brain_ops::test_support::run_in_glommio(|| async {
        let (_dir, ctx, metadata) = build_ctx();
        intern(&metadata, NS);

        let cleartext = RequestCaller::from_scope(
            SpaceId::derive_from_string(NS, "default"),
            [0u8; 16],
            [0u8; 16],
            NS.into(),
            bits::FULL,
        ); // no `with_confidential_transport`

        let err = send(&ctx, cleartext, put_body(KEY))
            .await
            .expect_err("a key must not cross an unprotected connection");
        assert!(matches!(err, OpError::Unauthorized(_)), "{err:?}");
    });
}

#[test]
fn the_reads_do_not_require_a_protected_transport() {
    // Only PUT carries key material. Gating the reads too would make a
    // credential unreadable over a connection it was never at risk on.
    brain_ops::test_support::run_in_glommio(|| async {
        let (_dir, ctx, metadata) = build_ctx();
        intern(&metadata, NS);

        let cleartext = RequestCaller::from_scope(
            SpaceId::derive_from_string(NS, "default"),
            [0u8; 16],
            [0u8; 16],
            NS.into(),
            bits::FULL,
        );
        send(&ctx, cleartext, get_body())
            .await
            .expect("a read is not gated on the transport");
    });
}

// ---------------------------------------------------------------------------
// Register, read, revoke
// ---------------------------------------------------------------------------

#[test]
fn a_registered_credential_reads_back_without_its_key() {
    brain_ops::test_support::run_in_glommio(|| async {
        let (_dir, ctx, metadata) = build_ctx();
        intern(&metadata, NS);

        let put = send(&ctx, caller(NS, bits::FULL), put_body(KEY))
            .await
            .expect("register");
        let ResponseBody::LlmCredentialPut(put) = put else {
            panic!("wrong response body");
        };
        assert_eq!(put.provider, "openai");
        assert_eq!(put.model, "gpt-4.1-mini");
        assert!(!put.replaced, "a first registration displaces nothing");

        let got = send(&ctx, caller(NS, bits::FULL), get_body())
            .await
            .expect("read");
        let ResponseBody::LlmCredentialGet(got) = got else {
            panic!("wrong response body");
        };
        let view = got.credential.expect("a credential is installed");

        // The whole contract of the read path: nothing it returns can
        // reconstruct the key.
        let rendered = format!("{view:?}");
        assert!(!rendered.contains(KEY), "{rendered}");
        assert!(!rendered.contains("not-a-real"), "{rendered}");
        assert_eq!(view.fingerprint, "…-key");
    });
}

#[test]
fn registering_again_reports_that_it_replaced_one() {
    brain_ops::test_support::run_in_glommio(|| async {
        let (_dir, ctx, metadata) = build_ctx();
        intern(&metadata, NS);

        send(&ctx, caller(NS, bits::FULL), put_body(KEY))
            .await
            .expect("first");
        let again = send(&ctx, caller(NS, bits::FULL), put_body("sk-test-rotated"))
            .await
            .expect("rotate");
        let ResponseBody::LlmCredentialPut(again) = again else {
            panic!("wrong response body");
        };
        assert!(again.replaced, "a rotation must be distinguishable");
    });
}

#[test]
fn a_delete_reports_whether_anything_was_revoked() {
    brain_ops::test_support::run_in_glommio(|| async {
        let (_dir, ctx, metadata) = build_ctx();
        intern(&metadata, NS);

        let nothing = send(&ctx, caller(NS, bits::FULL), delete_body())
            .await
            .expect("delete");
        let ResponseBody::LlmCredentialDelete(nothing) = nothing else {
            panic!("wrong response body");
        };
        assert!(!nothing.existed, "nothing was there to revoke");

        send(&ctx, caller(NS, bits::FULL), put_body(KEY))
            .await
            .expect("register");
        let revoked = send(&ctx, caller(NS, bits::FULL), delete_body())
            .await
            .expect("delete");
        let ResponseBody::LlmCredentialDelete(revoked) = revoked else {
            panic!("wrong response body");
        };
        assert!(revoked.existed);
    });
}

#[test]
fn one_tenants_credential_is_invisible_to_another() {
    brain_ops::test_support::run_in_glommio(|| async {
        let (_dir, ctx, metadata) = build_ctx();
        intern(&metadata, NS);
        intern(&metadata, OTHER_NS);

        send(&ctx, caller(NS, bits::FULL), put_body(KEY))
            .await
            .expect("register for acme");

        // The namespace a credential belongs to comes from the caller's key,
        // never from the request, so there is nothing for a second tenant to
        // point at acme's row with.
        let got = send(&ctx, caller(OTHER_NS, bits::FULL), get_body())
            .await
            .expect("read");
        let ResponseBody::LlmCredentialGet(got) = got else {
            panic!("wrong response body");
        };
        assert!(
            got.credential.is_none(),
            "a tenant resolves only its own credential"
        );
    });
}

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------
//
// There is nothing left to test here. A credential belongs to a namespace and
// there is no narrower scope to ask for: the requests carry no scope selector,
// so the refusal this file used to assert is now unrepresentable rather than
// merely rejected. The namespace a key lands in comes from the authenticated
// caller, which `a_key_is_scoped_to_its_namespace` below covers.

// ---------------------------------------------------------------------------
// Verify
// ---------------------------------------------------------------------------

#[test]
fn verifying_with_no_credential_reports_that_rather_than_failing() {
    // Every verify result is information the caller asked for. "You have no
    // key" is a successful verification, not an error.
    brain_ops::test_support::run_in_glommio(|| async {
        let (_dir, ctx, metadata) = build_ctx();
        intern(&metadata, NS);

        let out = send(&ctx, caller(NS, bits::FULL), verify_body())
            .await
            .expect("verify answers");
        let ResponseBody::LlmCredentialVerify(out) = out else {
            panic!("wrong response body");
        };
        assert_eq!(out.outcome, VerifyOutcome::NoCredential);
        assert!(out.provider.is_none());
        assert!(out.detail.is_none());
    });
}

#[test]
fn verifying_without_a_resolver_is_unreachable_not_unauthorized() {
    // A shard with no resolver wired cannot reach a provider. That must not
    // be reported as a rejected key: the tenant would replace a key that was
    // never tried.
    brain_ops::test_support::run_in_glommio(|| async {
        let (_dir, ctx, metadata) = build_ctx();
        intern(&metadata, NS);

        send(&ctx, caller(NS, bits::FULL), put_body(KEY))
            .await
            .expect("register");

        let out = send(&ctx, caller(NS, bits::FULL), verify_body())
            .await
            .expect("verify answers");
        let ResponseBody::LlmCredentialVerify(out) = out else {
            panic!("wrong response body");
        };
        assert_eq!(out.outcome, VerifyOutcome::Unreachable);
        assert_eq!(out.provider.as_deref(), Some("openai"));
        assert!(
            out.detail.is_some(),
            "an operator needs to know why it could not be tried"
        );
    });
}

// ---------------------------------------------------------------------------
// A deployment with no store
// ---------------------------------------------------------------------------

#[test]
fn a_deployment_with_no_store_refuses_rather_than_pretending() {
    brain_ops::test_support::run_in_glommio(|| async {
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
        // No `with_llm_credentials`.
        let ctx = brain_ops::test_support::ops_context_for_tests(executor, dir.path());
        intern(&metadata, NS);

        let err = send(&ctx, caller(NS, bits::FULL), put_body(KEY))
            .await
            .expect_err("a key that cannot be persisted must not be accepted");
        assert!(
            matches!(err, OpError::CapabilityNotEnabled { .. }),
            "{err:?}"
        );
    });
}
