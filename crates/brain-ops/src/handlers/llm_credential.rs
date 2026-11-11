//! LLM_CREDENTIAL_PUT / GET / DELETE / VERIFY handlers.
//!
//! A tenant registers their own provider key here and Brain's extraction and
//! HyPE stages bill to it. A deployment holds no key of its own, so this is
//! the only way a namespace becomes able to run the model derivation stages.
//!
//! # The namespace is the authenticated one
//!
//! Every handler keys off the namespace the caller's API key is bound to, not
//! anything in the request body, so a key can only ever register, read or
//! revoke its own tenant's credential.
//!
//! # A plaintext key only travels a confidential connection
//!
//! Registration is refused unless the connection is TLS, mTLS or loopback.
//! The seal protects the key at rest; refusing the request protects it in
//! flight, and accepting one over cleartext TCP would hand it to anyone on
//! the path — a worse exposure than the one sealing addresses.

use brain_metadata::llm_credential::LlmCredentialError;
use brain_metadata::tables::scope::DeclScope;
use brain_protocol::ops::llm_credential::{
    CredentialScope, LlmCredentialDeleteRequest, LlmCredentialDeleteResponse,
    LlmCredentialGetRequest, LlmCredentialGetResponse, LlmCredentialPutRequest,
    LlmCredentialPutResponse, LlmCredentialVerifyRequest, LlmCredentialVerifyResponse,
    LlmCredentialView, VerifyOutcome,
};

use crate::context::OpsContext;
use crate::dispatch::RequestCaller;
use crate::error::OpError;

/// The providers this build can serve. A key registered for anything else
/// would seal successfully and then fail every extraction, so it is rejected
/// at registration instead.
const SUPPORTED_PROVIDERS: [&str; 2] = ["openai", "anthropic"];

/// Reach the credential store, or explain that this deployment has none.
fn credentials(
    ctx: &OpsContext,
) -> Result<&std::sync::Arc<brain_metadata::llm_credential::LlmCredentialAccess>, OpError> {
    ctx.llm_credentials
        .as_ref()
        .ok_or(OpError::CapabilityNotEnabled {
            capability: "llm_credential_store",
        })
}

/// Resolve the request's scope to a storage scope.
///
/// Space-scoped credentials are storable, but derivation does not carry the
/// space as far as credential resolution, so a key registered at a space
/// would never be used. Refusing is the honest answer: accepting it would
/// leave a tenant with a key that silently does nothing.
fn decl_scope<'a>(scope: CredentialScope, namespace: &'a str) -> Result<DeclScope<'a>, OpError> {
    match scope {
        CredentialScope::Namespace => Ok(DeclScope::base(namespace)),
        CredentialScope::Space => Err(OpError::NotYetImplemented(
            "space-scoped LLM credentials: derivation resolves at the namespace, so a \
             space-scoped key would never be used",
        )),
    }
}

fn storage_error(e: LlmCredentialError) -> OpError {
    OpError::Internal(format!("llm credential store: {e}"))
}

/// `LLM_CREDENTIAL_PUT` — register or replace this tenant's provider key.
///
/// # Errors
/// `Unauthorized` over a non-confidential connection; `InvalidRequest` for an
/// unsupported provider, an empty model, or an empty key; `NotYetImplemented`
/// for a space scope; `Internal` on a storage or sealing failure.
pub fn handle_llm_credential_put(
    req: LlmCredentialPutRequest,
    caller: &RequestCaller,
    ctx: &OpsContext,
) -> Result<LlmCredentialPutResponse, OpError> {
    if !caller.transport_confidential {
        return Err(OpError::Unauthorized(
            "registering an LLM provider key requires a TLS, mTLS or loopback connection: \
             the key would otherwise cross the network in cleartext"
                .to_string(),
        ));
    }

    let provider = req.provider.trim().to_ascii_lowercase();
    if !SUPPORTED_PROVIDERS.contains(&provider.as_str()) {
        return Err(OpError::InvalidRequest(format!(
            "unsupported provider `{}`: expected one of {}",
            req.provider,
            SUPPORTED_PROVIDERS.join(", ")
        )));
    }
    let model = req.model.trim();
    if model.is_empty() {
        return Err(OpError::InvalidRequest("`model` must not be empty".into()));
    }
    if req.api_key.is_empty() {
        return Err(OpError::InvalidRequest(
            "`api_key` must not be empty".into(),
        ));
    }
    // Surrounding whitespace on a key is almost always a copy-paste artifact.
    // It would seal verbatim and then fail upstream with an opaque 401 that
    // looks like a wrong key rather than a stray space.
    if req.api_key.trim() != req.api_key {
        return Err(OpError::InvalidRequest(
            "`api_key` has leading or trailing whitespace".into(),
        ));
    }

    let namespace = caller.namespace.as_str();
    let scope = decl_scope(req.scope, namespace)?;
    let now = now_unix_nanos();

    let (status, replaced) = credentials(ctx)?
        .put(scope, &provider, model, &req.api_key, now)
        .map_err(storage_error)?;

    tracing::info!(
        target: "brain_ops::llm_credential",
        namespace,
        provider = %status.provider,
        model = %status.model,
        replaced,
        "registered a tenant LLM provider credential",
    );

    Ok(LlmCredentialPutResponse {
        provider: status.provider,
        model: status.model,
        fingerprint: status.fingerprint,
        created_at_unix_nanos: status.created_at_unix_nanos,
        replaced,
    })
}

/// `LLM_CREDENTIAL_GET` — what this tenant has installed.
///
/// # Errors
/// `NotYetImplemented` for a space scope; `Internal` on a storage failure.
pub fn handle_llm_credential_get(
    req: LlmCredentialGetRequest,
    caller: &RequestCaller,
    ctx: &OpsContext,
) -> Result<LlmCredentialGetResponse, OpError> {
    let namespace = caller.namespace.as_str();
    let scope = decl_scope(req.scope, namespace)?;

    let found = credentials(ctx)?
        .get_exact(scope)
        .map_err(storage_error)?
        .map(|status| LlmCredentialView {
            provider: status.provider,
            model: status.model,
            fingerprint: status.fingerprint,
            created_at_unix_nanos: status.created_at_unix_nanos,
            // Resolution is at the namespace today, so an answer is always
            // the scope's own. This turns true once a space-scoped read can
            // fall back to the tenant's key.
            inherited: false,
        });

    Ok(LlmCredentialGetResponse { credential: found })
}

/// `LLM_CREDENTIAL_DELETE` — revoke this tenant's key.
///
/// # Errors
/// `NotYetImplemented` for a space scope; `Internal` on a storage failure.
pub fn handle_llm_credential_delete(
    req: LlmCredentialDeleteRequest,
    caller: &RequestCaller,
    ctx: &OpsContext,
) -> Result<LlmCredentialDeleteResponse, OpError> {
    let namespace = caller.namespace.as_str();
    let scope = decl_scope(req.scope, namespace)?;

    let existed = credentials(ctx)?.delete(scope).map_err(storage_error)?;

    if existed {
        tracing::info!(
            target: "brain_ops::llm_credential",
            namespace,
            "revoked a tenant LLM provider credential",
        );
    }

    Ok(LlmCredentialDeleteResponse { existed })
}

/// `LLM_CREDENTIAL_VERIFY` — exercise the registered key against its provider.
///
/// Reports an outcome rather than failing, because every result here is
/// information the tenant asked for: a rejected key is a successful
/// verification that says the key is bad.
///
/// # Errors
/// `NotYetImplemented` for a space scope; `Internal` on a storage failure.
pub async fn handle_llm_credential_verify(
    req: LlmCredentialVerifyRequest,
    caller: &RequestCaller,
    ctx: &OpsContext,
) -> Result<LlmCredentialVerifyResponse, OpError> {
    let namespace = caller.namespace.as_str();
    let _ = decl_scope(req.scope, namespace)?;

    let Some(status) = credentials(ctx)?
        .resolve(namespace, brain_metadata::tables::scope::SPACE_BASE)
        .map_err(storage_error)?
    else {
        return Ok(LlmCredentialVerifyResponse {
            outcome: VerifyOutcome::NoCredential,
            provider: None,
            model: None,
            detail: None,
        });
    };

    // Verification goes through the same resolver derivation uses, so a pass
    // here means the path extraction will take actually works. Probing the
    // provider independently could pass while derivation still failed.
    let Some(resolver) = ctx.llm_credential_resolver.as_ref() else {
        return Ok(LlmCredentialVerifyResponse {
            outcome: VerifyOutcome::Unreachable,
            provider: Some(status.provider),
            model: Some(status.model),
            detail: Some("this shard has no credential resolver wired".into()),
        });
    };

    let outcome = match resolver.client_for(namespace, Some(&status.model)).await {
        brain_llm::CredentialOutcome::Ready(c) => probe(c.client, &status.model).await,
        brain_llm::CredentialOutcome::Missing => (VerifyOutcome::NoCredential, None),
        brain_llm::CredentialOutcome::Unavailable => (
            VerifyOutcome::Unreachable,
            Some("the credential could not be resolved right now".to_string()),
        ),
    };

    Ok(LlmCredentialVerifyResponse {
        outcome: outcome.0,
        provider: Some(status.provider),
        model: Some(status.model),
        detail: outcome.1,
    })
}

/// The cheapest call that proves a key works: a one-token completion.
async fn probe(
    client: std::sync::Arc<dyn brain_llm::LlmClient>,
    model: &str,
) -> (VerifyOutcome, Option<String>) {
    use brain_llm::{LlmError, LlmRequest};

    let mut request = LlmRequest::new(model.to_string(), "ping");
    request.max_tokens = 1;
    request.timeout = std::time::Duration::from_secs(10);

    match client.complete(request).await {
        Ok(_) => (VerifyOutcome::Ok, None),
        Err(LlmError::Auth { provider }) => (
            VerifyOutcome::Unauthorized,
            Some(format!("provider `{provider}` rejected the key")),
        ),
        Err(LlmError::ProviderError { status, message }) if status == 401 || status == 403 => {
            (VerifyOutcome::Unauthorized, Some(message))
        }
        // A provider reports an unknown or unentitled model as a bad request,
        // which is a different fix for the tenant than a bad key.
        Err(LlmError::ProviderError { status, message }) if status == 404 || status == 400 => {
            (VerifyOutcome::ModelUnavailable, Some(message))
        }
        Err(other) => (VerifyOutcome::Unreachable, Some(other.to_string())),
    }
}

fn now_unix_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}
