> **SUPERSEDED (2026-05-30).** Brain is now a standalone database with no first-party SDK, client library, or JSON gateway. The public interface is the §04 wire protocol (CBOR payloads). This plan is retained for historical context only and is not being executed. See `docs/development/spec-standalone-db-proposal.md`.

# Plan — Production-grade hand-written SDK family (Python + TypeScript, Go later)

**Status:** draft, awaiting approval. Per [[plan-first-workflow]] and [[production-ready-bar]] — no code until this is signed off.

## 0. Recap of decisions you've already made

- **Transport:** JSON-over-HTTP gateway (`brain-server/src/gateway/`, default `127.0.0.1:9093`). Built; needs hardening.
- **Build style:** **Hand-written** clients hardened to production grade. Not Stainless/Speakeasy codegen. Drift guard is a **cross-language conformance suite**, not a shared generator.
- **Languages:** Python + TypeScript now; Go later. Rust SDK (`brain-sdk-rust`) already exists for the binary wire — separate track, untouched here.
- **Pre-freeze churn accepted:** wire is still in flux pre-v1.0; the gateway DTO layer absorbs churn.
- **Repo layout:** `clients/python/`, `clients/typescript/` in this repo for now (co-evolve); split out post-freeze if needed (no cross-repo `just` targets per [[brain-eval-separate-repo]]).
- **Naming:** public verbs are domain verbs (`recall`, `query`), per [[no-db-wire-versioning]] — no engine names like `recall_hybrid`.

## 1. The production bar — what "done" looks like

A capability matrix every SDK in the family must satisfy. Sourced from the openai-python / anthropic-sdk-python / stripe-python / openai-node / stripe-node teardown (see `Research notes` at the bottom).

| # | Capability | What it means | Why |
|---|---|---|---|
| 1 | **Sync + async dual client** | `Client` (sync) and `AsyncClient` (async), parallel impls sharing one generic `BaseClient`. No private event loop, no `asyncio.run` from sync. | Real async I/O; no thread-pool gymnastics; clean stack traces. The Stainless pattern. |
| 2 | **Typed result models** | Pydantic v2 (Python). Typed interfaces + `export type` (TS). `py.typed` marker, full type coverage. | Compile-time safety; IDE completion; contract precision. |
| 3 | **Connection pooling** | `httpx.Limits(max_connections=100, max_keepalive_connections=20, keepalive_expiry=30)` (Py). Per-instance undici dispatcher (TS). | Throughput; predictable resource use; no per-call socket churn. |
| 4 | **Retries: exp backoff + jitter** | Default `max_retries=2`. Retry on 408/409/429/5xx + `x-brain-should-retry` header override. Honor `Retry-After` (capped 60s). Negative-jitter `1 − 0.25·rand` or positive `0.5·(1+rand)` — pick one. | Survive transient failures without thundering herds. |
| 5 | **Idempotency-Key on every write** | Auto-mint `brain-{lang}-{uuid7}` on every POST. User can pass their own via per-call `request_id=`. Reuse across retries. | Stripe behavior, not Stainless. Protects against app-level retry re-entry. |
| 6 | **Request-ID propagation** | Read server `X-Request-Id` header; surface on every error and every successful response (Py: `response.request_id`; TS: `client.encode(...).withResponse() → {data, response, requestId}`). | Support triage. Non-negotiable. |
| 7 | **Structured error taxonomy** | `BrainError → APIError → APIStatusError → {BadRequest 400, Unauthorized 401, Forbidden 403, NotFound 404, Conflict 409, RateLimit 429, UnprocessableEntity 422, Internal 500, ServiceUnavailable 503, GatewayTimeout 504}`. Plus `APIConnectionError → APITimeoutError`. Every error carries `status, code, request_id, headers, retryable`. | Match the OpenAI/Anthropic shape; users branch on type, not strings. |
| 8 | **Pagination with auto-iterators** | Cursor-based `Page[T]` / `AsyncPage[T]`; `__iter__`/`__aiter__` walks pages transparently. Also `.next_page()` and `.has_more`. | Standard pattern; users don't write loops. |
| 9 | **Streaming responses** | Hand-rolled SSE/NDJSON decoder. `Stream[T]` + `AsyncStream[T]` both implementing context-manager protocols. Sync iter + async iter. Mid-stream cancellation closes the socket. | Recall over large k; future subscribe. Backpressure via lazy reads. |
| 10 | **Auth: bearer header + rotation** | `auth=<key>` constructor or env (`BRAIN_API_KEY`). Sent as `Authorization: Bearer <secret>`. `client.with_options(auth=...)` for per-call. | Strict-mode gateway requires it. Rotation = pass a `Callable[[], str]` (async: `Callable[[], Awaitable[str]]`) — Stainless pattern. |
| 11 | **Per-call overrides** | Every method takes `timeout=`, `max_retries=`, `extra_headers=`, `extra_body=`, `idempotency_key=`. Per-call wins over client default. | Stripe's `RequestOptions` and OpenAI's `extra_*` kwargs both work; pick one per language. |
| 12 | **Telemetry headers** | Every request sends `X-Brain-Lang`, `X-Brain-Lang-Version`, `X-Brain-Package-Version`, `X-Brain-OS`, `X-Brain-Retry-Count`. | Support triage; tracks SDK adoption from server logs. |
| 13 | **Observability hooks** | Event emitter: `client.on('request', cb)` / `on('response', cb)` / `on('retry', cb)` / `on('error', cb)`. Payload: `{method, path, status, request_id, elapsed_ms, attempt}`. | Stripe-node's pattern; better than per-call callbacks. Integrates with any logger/metrics/tracer. |
| 14 | **Graceful shutdown** | `with Client(...) as c:` / `async with AsyncClient(...) as c:` (Py). `await client.close()` (TS). Closes pool, cancels in-flight ops gracefully. | Avoid socket leaks; clean shutdown in tests. |
| 15 | **Sane defaults** | Timeout 60s (encode), 30s (recall), per-op. Max retries 2. Max body 16 MiB (matches gateway). | Pick once, document; users override when needed. |
| 16 | **TLS + mTLS escape hatch** | Pass custom `httpx.Client(verify=ctx)` / `fetch` with dispatcher carrying `connect: {ca, cert, key}`. | Production deployments behind mTLS. |
| 17 | **Versioning + deprecation** | Semver. Public symbols documented in `CHANGELOG.md`. `DeprecationWarning` (Py) / `@deprecated` (TS) for one minor version before removal. SDK pins a wire-protocol version range (TBD when freeze lands). | Deliberate evolution; no surprise breaks. |
| 18 | **Packaging** | **Py:** `pyproject.toml` + hatchling + `src/` layout + `py.typed` + PyPI wheel. **TS:** dual ESM+CJS via `exports` map; 5 platform conditions (`node/browser/bun/deno/worker/workerd`); zero runtime deps; `types` sibling files. | Cross-runtime support; clean install story. |
| 19 | **Tests** | Unit tests against mocked transport (Py: `httpx.MockTransport`; TS: injected `fetch` stub). Integration tests against the in-process server harness. **Cross-language conformance suite** (see §5). CI runs all three on every PR. | Drift guard. |
| 20 | **Docs** | `README.md` with quickstart + every verb + every error code. Generated API reference (Py: pdoc; TS: typedoc). Tutorial: encode → recall → forget. Operational notes (timeouts, retries, idempotency, telemetry). | First-contact UX. The SDK *is* the docs for most users. |

**Done = all 20 boxes ticked for both Python and TypeScript. Anything less is a prototype, not a production SDK.**

## 2. Architecture — the 6-file framework (per language)

Per the teardown: every Stainless-generated SDK is ~6 framework files + per-resource files. Hand-write the same skeleton.

### Python (`clients/python/brain/`)

```
brain/
  __init__.py           # public re-exports + __all__
  py.typed              # empty PEP-561 marker
  _base_client.py       # BaseClient[HttpxT, StreamT]; retry; idempotency; telemetry headers; build_request; parse_response
  _client.py            # SyncAPIClient, AsyncAPIClient (parallel impls of BaseClient); Brain, AsyncBrain (thin auth + resource accessors)
  _streaming.py         # Stream[T], AsyncStream[T], SSEDecoder, NDJSONDecoder; context managers; cancellation
  _exceptions.py        # BrainError → APIError → APIStatusError → {BadRequest, Unauthorized, ...}; APIConnectionError → APITimeoutError
  _models.py            # Pydantic BaseModel base; ConfigDict; common types (MemoryId, EdgeKind, MemoryKind, AgentId)
  _response.py          # APIResponse[T] wrapper exposing {data, response, request_id, headers}
  pagination.py         # Page[T], AsyncPage[T]; __iter__/__aiter__; cursor protocol
  _events.py            # Emitter for on('request'/'response'/'retry'/'error')
  _retry.py             # RetryConfig; should_retry; calculate_delay; honor Retry-After
  resources/
    __init__.py
    memory.py           # encode, recall, forget, link, unlink
    capabilities.py     # get_capabilities
    schema.py           # (M3) entities, statements, relations, schema_upload
    query.py            # (M3) query, query_explain, query_trace
```

### TypeScript (`clients/typescript/src/`)

```
src/
  index.ts              # public exports; `export type {...}` for type-only
  client.ts             # Brain (sync-ish), AsyncBrain (async); resource accessors
  baseClient.ts         # BaseClient with retry/idempotency/telemetry/build/parse
  apiPromise.ts         # APIPromise<T> extends Promise<T>; .asResponse(), .withResponse() → {data, response, requestId}
  streaming.ts          # Stream<T>, AsyncStream<T>, SSEDecoder, NDJSONDecoder
  errors.ts             # BrainError → APIError → APIStatusError → {BadRequest, ...}
  models.ts             # All typed interfaces; type-only exports
  pagination.ts         # Page<T>, AsyncPage<T>; async iterator
  events.ts             # Emitter
  retry.ts              # RetryConfig; shouldRetry; calculateDelay
  resources/
    memory.ts           # encode, recall, forget, link, unlink
    capabilities.ts
    schema.ts           # (M3)
    query.ts            # (M3)
```

### Key architectural decisions

1. **Sync/async = parallel impls sharing a generic base.** Verified: openai-python does NOT use a private event loop. Cost is "duplicate" methods; gain is true async I/O + clean traces.
2. **Pydantic v2 only.** Skip the openai/anthropic v1+v2 compat shim (~300 LOC); v1 is EOL in 2024. (Open Q1.)
3. **httpx (Py) + native fetch (TS) only.** Don't ship Stripe-style 6-backend pluggability — costs ~500 LOC for a non-existent customer ask.
4. **Idempotency on every POST**, not just on retry. Stripe behavior. Safer against app-level retries.
5. **Domain verbs only** at the public surface: `recall`, `query` — not `recall_hybrid`, per [[no-db-wire-versioning]]. The gateway already exposes domain paths.
6. **Repo verb naming follows the gateway**, which follows `spec/06_sdk/02_core_api.md`. No SDK-side rewording.

## 3. Gateway hardening (Rust side)

The gateway exists and proves the seam (3 of 6 e2e tests pass). To call it a production transport for the SDKs, the following must land:

| # | Item | Why |
|---|---|---|
| G1 | **Root-cause the dispatch hang.** All 3 failing tests are gateway handlers that `.await dispatch_op()`; the binary path via `sdk_e2e` works. Likely a `!Send` issue or a hyper-task / Glommio channel interaction. Diagnose, fix, no band-aid. | Blocks every dispatch-reaching op. |
| G2 | **Gateway-level dispatch deadline.** Today the only timeout is brain-http's 30s `request_timeout`. Add an explicit per-op deadline (default 60s for writes, 30s for reads) that returns `504 GatewayTimeout` with a structured body — never hang the socket. | Production gateways always have explicit deadlines. |
| G3 | **Request-ID generation.** If the client didn't send `X-Request-Id`, mint one (UUIDv7); echo on every response (success + error). | Required for SDK capability #6. |
| G4 | **Error envelope: add `request_id` field** alongside `{error, code, retryable}`. SDK reads it onto every error/response. | SDK contract. |
| G5 | **Telemetry headers — accept + log.** Server should structured-log `x-brain-lang/version/...` on every request. Operators get free SDK-adoption metrics. | Cheap, big payoff. |
| G6 | **TLS support** for the gateway listener (config-gated, mirroring the binary listener). | Production deployments. |
| G7 | **Auth hardening**: today permissive mode trusts the `X-Brain-Agent-Id` header — fine for dev, but the strict-mode path (API-key bound) must be CI-tested end-to-end through the gateway. | Currently only the binary AUTH path has strict-mode integration tests. |
| G8 | **Remaining substrate ops over JSON**: `plan`, `reason`, `get_capabilities` (working but untested w/ stream), `link`, `unlink`, plus `entity_*` / `statement_*` / `relation_*` / `schema_*` / `query_*` as M3 ramps up. Streaming via NDJSON for `DispatchOutcome::Stream`. | The SDKs can't expose verbs the gateway doesn't carry. |
| G9 | **OpenAPI spec for the gateway.** Even though we're hand-writing, the spec is the API contract — drives the conformance suite, the docs, and (if ever wanted) future codegen. Generated from Rust types where possible; hand-edited where not. | Single source of truth for cross-language parity. |

## 4. Milestones

Sequenced so each milestone produces a usable, demoable slice. No big-bang.

### M-1: Reset (this PR, before any code)
- Delete the existing happy-path Python + TS scaffolding (or aggressively prune to skeleton). Keep `pyproject.toml`/`package.json` as starting points.
- Land this plan in `.claude/plans/` (this file).
- Record deviation `SD-6.x-1` already in place. Add `SD-6.x-2` for "hand-written, not codegen" decision.

### M0: Gateway production-readiness (Rust side) — must precede SDK work
- G1 (root-cause hang), G2 (deadline), G3+G4 (request_id), G5 (telemetry log)
- Integration test: all 6 substrate verbs over JSON + streaming
- Acceptance: every `gateway_e2e.rs` test passes; `bad_json_is_400` + `unauthorized_is_401` + `timeout_is_504` + `unknown_route_is_404` all green.

### M1: Python SDK framework (6-file skeleton)
- `_base_client`, `_client`, `_streaming`, `_exceptions`, `_models`, `_response`, `_retry`, `_events`, `pagination`
- One resource: `memory.encode` (sync + async, retries, idempotency, telemetry headers, request_id propagation, error mapping)
- Unit tests: 30+ against `httpx.MockTransport` (retry edge cases, Retry-After, error mapping, request_id surfacing, telemetry header verification, idempotency reuse on retry, idempotency-on-every-POST)
- Acceptance: capability boxes #1, #3, #4, #5, #6, #7, #10, #11, #12, #14, #15, #18, #19 ticked for one verb.

### M2: Python SDK substrate completion
- Remaining substrate verbs: `recall` (with streaming Page → AsyncPage), `forget`, `link`, `unlink`, `get_capabilities`, `plan`, `reason`
- Streaming for `recall` (Stream/AsyncStream context managers)
- Observability hooks (#13)
- README + tutorial + per-verb examples (#20)
- Integration tests against in-process server harness
- Acceptance: all 20 capability boxes ticked for substrate ops.

### M3: TypeScript SDK framework + substrate
- Mirror M1+M2 in TS. Reuse the architecture; idiomatic where TS demands (options-object for per-call overrides, `APIPromise<T>`, ESM+CJS dual publish, 5-condition `exports` map, type-only exports).
- Same 20-box acceptance.

### M4: Cross-language conformance suite
- `clients/conformance/` — Go binary `brain-mock` (Stripe-mock pattern): reads OpenAPI spec + fixture JSON; serves deterministic responses
- `clients/conformance/corpus/*.yaml` — language-agnostic test cases: `[request, expected_response]` pairs covering every retryable status, idempotency reuse, pagination walk, streaming with mid-stream disconnect, error mapping per code, request_id propagation, telemetry headers verified
- Per-SDK harness: imports the corpus, runs each case via the real client, asserts on model. CI runs Py + TS in matrix.
- Acceptance: 100+ cases, every retry/error code/streaming path covered, both SDKs pass.

### M5: Typed-graph verbs (entity/statement/relation/schema/query)
- Both SDKs gain the typed-graph ops as the gateway carries them.
- Conformance corpus extended.

### M6: Polish + 1.0-readiness
- Pre-1.0 release: tag SDK versions independently from server; pin wire-protocol range in `__init__.py` / `index.ts`
- Generate docs via pdoc/typedoc; publish to GH Pages
- PyPI + npm publish dry-run; `pre-commit` config; CI badges; CHANGELOG discipline
- Go SDK: scoped as a separate post-1.0 deliverable.

## 5. Cross-language conformance suite (M4 design)

The drift guard for hand-written SDKs. Pattern: stripe-mock + YAML corpus.

```
clients/
  conformance/
    Cargo.toml                  # Rust mock-server, reuses brain-protocol DTO types
    src/main.rs                 # serves the OpenAPI spec deterministically
    fixtures/                   # canned response bodies
    corpus/
      01-encode-basic.yaml
      02-encode-with-edges.yaml
      03-recall-pagination.yaml
      04-recall-streaming.yaml
      05-forget-idempotency.yaml
      06-retry-429.yaml
      07-retry-5xx.yaml
      08-retry-after-header.yaml
      09-error-mapping-404.yaml
      ...                       # ~100 cases total
    README.md                   # how to add a case
  python/tests/conformance_test.py    # imports corpus, runs each case
  typescript/test/conformance.test.ts # same
```

Each YAML case:
```yaml
name: encode-basic
request:
  method: POST
  path: /v1/encode
  headers: {X-Brain-Agent-Id: "..."}
  body: {text: "hello", kind: "episodic"}
expected:
  status: 200
  body_schema: EncodeResult
  body_assertions:
    - memory_id: "matches /^[0-9]+$/"
    - kind: "episodic"
  request_id_in_response: true
```

CI: GH Actions matrix `{lang: [python, typescript]}` — bring up `brain-mock`, run the SDK conformance harness, compare. Drift is caught at merge time.

(Implementation in Rust rather than Go, since we already have the Rust DTO types — avoids re-translating them.)

## 6. What we are NOT building (scope discipline)

- **No codegen.** No Stainless/Speakeasy/Fern integration. Decision locked in [[production-ready-bar]].
- **No Stripe-style HTTP-backend pluggability.** httpx (Py) + fetch (TS) only. Single dependency, single code path.
- **No batch/queue helpers** beyond what the gateway exposes natively. Application concern per `spec/06_sdk/00_purpose.md` §5.
- **No client-side caching / pre-fetching / re-ranking** per `spec/06_sdk/00_purpose.md` §5.
- **No Go SDK in this plan.** Scoped to post-1.0; framework is the same skeleton ported.
- **No browser/Edge SDK as a separate package.** The TS package's `exports` map already covers worker/edge runtimes; "browser SDK" is the same wheels with `dangerouslyAllowBrowser: true` (OpenAI pattern) if/when needed.
- **Typed-graph ops** (entity/statement/relation/schema/query) deferred to M5; substrate-only through M3.

## 7. Open questions (need your call before M1)

| # | Question | Recommended default | Notes |
|---|---|---|---|
| Q1 | **Pydantic v2 only**, or also support v1? | **v2 only.** | Saves ~300 LOC of compat shim. v1 EOL Q3 2024. openai/anthropic still support both for legacy users; we have no users yet. |
| Q2 | **Request-ID header name:** `X-Request-Id` (OpenAI convention) or `request-id` (Anthropic + Stripe)? | **`X-Request-Id`.** | Broader industry standard; OpenAI's. The other two are minor outliers. |
| Q3 | **Per-call override style** (Python): OpenAI's `extra_*` kwargs or Stripe's `RequestOptions` second arg? | **Stripe's `options=RequestOptions(...)`** as a second positional arg — explicit, no kwarg sprawl. | OpenAI's bleeds many kwargs into every method signature. |
| Q4 | **Sync wrapper:** ship sync `Client` alongside `AsyncClient`, or async-only with users wrapping themselves? | **Ship both.** | spec/06_sdk/00 §6 mandates both. The cost is ~30% more code; the gain is Jupyter notebooks, scripts, sync ML pipelines. |
| Q5 | **Jitter direction:** Stainless's negative (`1 − 0.25·rand`, range 0.75–1.0) or Stripe's positive (`0.5·(1+rand)`, range 0.5–1.0)? | **Positive (Stripe).** | More common in classical thundering-herd literature; bigger spread; either works. |
| Q6 | **Default `max_retries`:** 2 (Stainless) or 1 (Stripe-node) or 0 (Stripe-python)? | **2.** | Industry typical; balances "transient bumps survive" vs "real outages surface quickly." |
| Q7 | **Auth env var name:** `BRAIN_API_KEY` (standard) or other? | **`BRAIN_API_KEY`.** | Convention; matches `OPENAI_API_KEY` / `STRIPE_API_KEY` discoverability. |
| Q8 | **OpenAPI spec (G9) hand-written or generated from Rust types?** | **Hand-written initially**, generated later if/when the wire surface stabilizes. | utoipa/aide-style Rust→OpenAPI tooling adds workspace deps; we can defer that decision. |
| Q9 | **`clients/python` vs `sdks/python` for the repo location** — does the directory name matter? | Keep `clients/` (already in place). | "client" = "SDK that talks to a server." Matches industry. |
| Q10 | **Where the gap report from the existing prototype lives** — should I delete the existing `clients/python/` and `clients/typescript/` code (since they don't meet the bar), or aggressively prune them to skeleton and rebuild on top? | **Delete + rebuild from skeleton.** The existing code mixed concerns (no `_base_client` split, no streaming/pagination, ad-hoc errors). Easier to do right than to refactor in place. | Keep `pyproject.toml` / `package.json` / `tsconfig.json` / `README.md` as starting scaffolding. |

## 8. Deviations to record (`docs/development/spec-deviations.md`)

- **SD-6.x-1** (already recorded): JSON-over-HTTP gateway + non-Rust SDKs ahead of roadmap.
- **SD-6.x-2** (new, on plan approval): "Hand-written SDKs, not codegen — drift guarded by a cross-language conformance suite rather than a shared generator. Reconciled at v1.0 wire freeze: revisit codegen then if maintenance burden has bitten."

## 9. Risk register

| Risk | Likelihood | Mitigation |
|---|---|---|
| Wire-protocol churn pre-1.0 ⇒ gateway DTO + SDK models churn | High | All translation centralized in `gateway/dto/`; SDK models regenerated from gateway as it stabilizes. Conformance corpus catches drift early. |
| Hand-written SDKs drift between Py and TS over time | Medium-High | M4 conformance suite. Reviews require both SDKs updated in the same PR for any verb addition. |
| Gateway hang (G1) turns out to be deeper (e.g. Glommio/Tokio scheduling) | Medium | Worst case: add a thin `tokio::task::spawn` shim in front of `dispatch_op` to break the executor coupling. Time-box to 2 days; escalate if no progress. |
| Cross-language consistency is fundamentally fragile without codegen | Medium | Conformance suite catches behavior drift; review process catches API surface drift; documented API style guide caps subjective variation. Revisit codegen at v1.0 if maintenance is biting. |
| pydantic v2 in `_models.py` makes the SDK heavy (~3 MB install) | Low | Acceptable; openai-python ships pydantic. Future: optional `[lite]` extra with dataclasses-only models. |
| TS `fetch` not available on some target (old Node 16) | Low | Drop Node 16; require Node 18+ (LTS since Oct 2022). Matches openai-node and stripe-node. |

## 10. Done-when (for the whole initiative)

- Both Python and TS SDKs pass all 20 capability boxes (§1) for substrate ops.
- Conformance suite runs in CI with 100+ cases, green on both SDKs.
- README + tutorial render correctly on GitHub; one new user can follow it to a working query in <15 minutes.
- Gateway has explicit per-op deadline + request-id + structured error envelope.
- Pre-merge gate: `just docker-verify` green + conformance CI green.
- Versions: `brain-sdk` 0.1.0 on PyPI dry-run; `@brain/sdk` 0.1.0 on npm dry-run (don't publish until v1.0 of the server).

---

## Research notes (primary-source, cited)

These ground every claim in §1, §2, §3. Full report at `clients/research/sdk-best-practices-2026-05.md` (writing as part of M-1).

| Claim | Source |
|---|---|
| 6-file skeleton matches Stainless layout | `src/openai/` directory listing in [openai/openai-python](https://github.com/openai/openai-python/tree/main/src/openai) |
| Sync/async = parallel impls of generic `BaseClient`, no event-loop trickery | [openai-python `_base_client.py`](https://github.com/openai/openai-python/blob/main/src/openai/_base_client.py) line 557 (`BaseClient[_HttpxClientT, _DefaultStreamT]`), line 1087 (`SyncAPIClient(BaseClient[httpx.Client, Stream[Any]])`) |
| Retry: exp backoff + negative jitter | openai-python `_base_client.py` lines 1423–1430: `sleep_seconds = min(INITIAL_RETRY_DELAY * pow(2.0, nb_retries), MAX_RETRY_DELAY); jitter = 1 - 0.25 * random()` |
| Retry on 408/409/429/5xx + `x-should-retry` header | openai-python `_base_client.py` lines 1436–1456 |
| Honor `Retry-After` (ms or seconds or HTTP-date), capped 60s | openai-python `_base_client.py` lines 1393–1417 |
| Idempotency-key on every POST (Stripe) vs only on retry (Stainless) | Stripe-python [`_api_requestor.py`](https://github.com/stripe/stripe-python/blob/master/stripe/_api_requestor.py): `if method == "post" or (api_mode == "V2" and method == "delete"): headers.setdefault("Idempotency-Key", str(uuid.uuid4()))` |
| Telemetry headers `x-stainless-*` | openai-python `_base_client.py` lines 921–927 |
| Pagination: cursor with `__iter__`/`__aiter__` walking `next_page_info()` | openai-python [`pagination.py`](https://github.com/openai/openai-python/blob/main/src/openai/pagination.py) |
| Streaming: hand-rolled `SSEDecoder` with `__enter__`/`__aenter__` context managers | openai-python [`_streaming.py`](https://github.com/openai/openai-python/blob/main/src/openai/_streaming.py) |
| Error taxonomy with `status_code: Literal[400|401|...]` leaf classes | openai-python [`_exceptions.py`](https://github.com/openai/openai-python/blob/main/src/openai/_exceptions.py) |
| Request-ID header: OpenAI uses `x-request-id`; Anthropic + Stripe use `request-id` | openai-python `_exceptions.py`: `request_id = response.headers.get("x-request-id")` vs anthropic-sdk-python `_exceptions.py`: `request_id = response.headers.get("request-id")` |
| `py.typed` marker for PEP 561 | [PEP 561](https://peps.python.org/pep-0561/); shipped in both openai-python and anthropic-sdk-python src trees |
| httpx pluggable pool: `httpx.Limits(max_connections=100, max_keepalive_connections=20)` | [Anthropic SDK README](https://github.com/anthropics/anthropic-sdk-python) — `DefaultHttpxClient` |
| TS native fetch only (no node-fetch, no undici dep) | openai-node [`src/internal/shims.ts`](https://github.com/openai/openai-node/blob/master/src/internal/shims.ts); PR [#402](https://github.com/openai/openai-node/pull/402) |
| TS dual ESM+CJS via `exports` map + 5 platform conditions | stripe-node [`package.json`](https://github.com/stripe/stripe-node/blob/master/package.json) maps `browser/bun/deno/worker/workerd` |
| TS retry: positive jitter `0.5*(1+random)` clamped to `[initial, max]` | stripe-node [`RequestSender.ts`](https://github.com/stripe/stripe-node/blob/master/src/RequestSender.ts) lines 349–375 |
| TS observability via event emitter `client.on('request', cb)` | stripe-node README |
| TS APIPromise<T> for `{data, response, request_id}` ergonomics | openai-node [`api-promise.ts`](https://github.com/openai/openai-node/blob/master/src/core/api-promise.ts) |
| Conformance: OpenAPI-driven mock server (stripe-mock) | [stripe-mock README](https://github.com/stripe/stripe-mock/blob/master/README.md): "We use it in the test suites of our server-side SDKs, like stripe-ruby, stripe-go, etc, to help validate that the SDK hits the right URL and sends the right parameters." |
| gRPC interop test suite as reference for cross-language conformance | [grpc/grpc `interop-test-descriptions.md`](https://github.com/grpc/grpc/blob/master/doc/interop-test-descriptions.md) |

Done. Awaiting answers to §7's 10 questions and approval (or pushback) on §1's bar, §2's architecture, §4's milestone sequencing, and §6's exclusions.
