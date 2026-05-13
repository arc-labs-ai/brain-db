# Sub-task 9.15 — OpenAI / Ollama Summarizer adapter (feature-gated)

**Reads:**
- `spec/11_background_workers/03_consolidation.md` §6, §7 (LLM
  integration shape, prompt).
- `spec/11_background_workers/09_failure_modes.md` §6 (consolidation
  LLM down: log + skip, optional circuit breaker).
- `crates/brain-workers/src/summarizer.rs` (existing trait surface
  with `DisabledSummarizer` default — left in place since 7.10).
- `crates/brain-server/src/shard.rs::register_phase8_workers` —
  current wiring constructs `ConsolidationWorker::new(Arc::new(DisabledSummarizer))`.

**Phase doc:** orientation §11 sub-task **9.15**.

**Done when:** brain-server can construct a real Summarizer pointed
at OpenAI's Chat Completions API or Ollama's `/api/generate` endpoint
based on `cfg.summarizer.backend`; both adapters are feature-gated
(default off — the substrate works without consolidation per spec
§6); the summarizer lives on its own bridge Tokio runtime so reqwest
calls don't pollute the per-shard Glommio executor.

---

## 1. Scope, pragmatically

- **In scope:** two HTTP adapters (`OpenAiSummarizer`, `OllamaSummarizer`),
  a config-driven factory, integration into `register_phase8_workers`
  via a shared `Arc<dyn Summarizer>`, mock-server integration tests.
- **Out of scope:** circuit breaker / extended backoff (spec §11/09 §6
  mentions it as a future enhancement; v1 logs + skips the cycle);
  prompt customisation via TOML (use the spec default prompt);
  streaming responses (we wait for the full completion);
  retries (one shot per cycle, fail-skip);
  alternative API shapes (Anthropic, Azure OpenAI variants — share
  the OpenAI adapter's wire shape, document at the time).

---

## 2. Async-runtime collision

The consolidation worker runs on the **per-shard Glommio executor**
(post-9.7a). Spec §11/03 §6 calls into an LLM service, which means
HTTPS. The two viable HTTP clients:

| Client | Verdict |
| ------ | ------- |
| `reqwest::async` | Uses Tokio's reactor under the hood. Awaiting a `reqwest::Response` future inside a Glommio task = undefined behavior (no reactor registered with Glommio's event loop). |
| `reqwest::blocking` | Synchronous — blocks the entire Glommio executor for the duration of the HTTPS round-trip (~50–500 ms). Tail-latency catastrophe for every other request on that shard. |
| Hand-rolled TLS via `tokio-rustls` + manual HTTP/1.1 | We already vendor both. ~300 LOC. Same Tokio-reactor problem as reqwest async. |

**Solution — bridge Tokio runtime:** the summarizer owns a dedicated
single-thread `tokio::runtime::Runtime` spun up at construction. The
trait method:

```rust
fn summarize<'a>(&'a self, memories: &'a [&'a str]) -> Pin<Box<dyn Future…>>
```

returns a future that:
1. Sends the request payload through a `flume::Sender<…>` into the
   bridge runtime (flume is runtime-agnostic).
2. Awaits the response via `flume::Receiver::recv_async()` on the
   Glommio side.

Inside the bridge runtime, a worker task pulls requests, calls
`reqwest::Client::post(...).send().await`, packs the response, sends
back. Standard cross-runtime bridge pattern.

The bridge runtime runs *one* OS thread. CPU cost is negligible —
HTTPS round-trips are I/O-bound. Memory cost ~5 MB.

---

## 3. Module layout

| File | Action | Approx LOC |
| ---- | ------ | ---------- |
| `crates/brain-server/src/llm/mod.rs` | new — feature-gated module root | 30 |
| `crates/brain-server/src/llm/bridge.rs` | new — single-thread bridge runtime + request channel | ~120 |
| `crates/brain-server/src/llm/openai.rs` | new — `OpenAiSummarizer`: API-key auth, Chat Completions JSON | ~180 |
| `crates/brain-server/src/llm/ollama.rs` | new — `OllamaSummarizer`: `/api/generate`, no auth | ~140 |
| `crates/brain-server/src/llm/prompt.rs` | new — `build_consolidation_prompt(memories)` per spec §11/03 §7 | ~40 |
| `crates/brain-server/src/llm/factory.rs` | new — `build_summarizer(&Config) -> Arc<dyn Summarizer>` | ~80 |
| `crates/brain-server/src/config.rs` | extend — `SummarizerConfig` enum + serde plumbing | ~80 delta |
| `crates/brain-server/src/shard.rs` | extend — `register_phase8_workers` takes `Arc<dyn Summarizer>` instead of constructing `DisabledSummarizer` inline | ~30 delta |
| `crates/brain-server/src/main.rs` | extend — build summarizer once in `linux_main::run`, pass through to each shard | ~10 delta |
| `crates/brain-server/Cargo.toml` | add `reqwest` (feature-gated), `serde_json` | ~10 |
| `config/dev.toml` | add `[summarizer]` section (backend = "disabled") | ~10 |
| `crates/brain-server/tests/summarizer.rs` | new — 4 integration tests against a hand-rolled mock HTTP server | ~400 |

Total: ~1130 LOC. Larger than 9.13/9.14; this is a full feature with
new external surface (HTTPS, config, JSON parsing).

---

## 4. Cargo features

```toml
[features]
default = []
summarizer-openai = ["reqwest", "serde_json"]
summarizer-ollama = ["reqwest", "serde_json"]

[dependencies]
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"], optional = true }
serde_json = { workspace = true, optional = true }
```

Both features include `reqwest` (shared). `default-features = false +
rustls-tls` keeps us on rustls (already vendored) — no openssl pull-in.
The `json` feature gives `reqwest::RequestBuilder::json()`.

`build_summarizer` is conditional:
- If neither feature is enabled and `cfg.summarizer.backend != "disabled"`:
  fail with a clear error at startup.
- If `summarizer-openai` is enabled and `backend == "openai"`: construct
  `OpenAiSummarizer`.
- If `summarizer-ollama` is enabled and `backend == "ollama"`: construct
  `OllamaSummarizer`.
- Else: `DisabledSummarizer` (always available).

---

## 5. Config schema

New section in `config/dev.toml`:

```toml
[summarizer]
backend = "disabled"             # disabled | openai | ollama
request_timeout_sec = 30
max_summary_chars = 4096

# openai (only used when backend == "openai")
openai_api_base = "https://api.openai.com/v1"
openai_api_key_env = "OPENAI_API_KEY"   # read from env at startup
openai_model = "gpt-4o-mini"
openai_temperature = 0.3

# ollama (only used when backend == "ollama")
ollama_base = "http://localhost:11434"
ollama_model = "llama3.1:8b"
```

`openai_api_key_env` names an env var to read at startup. Reading the
key from env keeps secrets out of `config.toml` (which is often
checked in). Empty / missing env var → startup error if
`backend == "openai"`.

The default `dev.toml` ships `backend = "disabled"` so the server
boots without an LLM dependency.

```rust
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SummarizerConfig {
    #[serde(default = "default_backend")]
    pub backend: SummarizerBackend,
    #[serde(default = "default_timeout")]
    pub request_timeout_sec: u32,
    #[serde(default = "default_max_chars")]
    pub max_summary_chars: u32,

    // OpenAI fields default-on so dev.toml doesn't have to spell them.
    #[serde(default = "default_openai_base")]
    pub openai_api_base: String,
    #[serde(default)]
    pub openai_api_key_env: Option<String>,
    #[serde(default = "default_openai_model")]
    pub openai_model: String,
    #[serde(default = "default_temperature")]
    pub openai_temperature: f32,

    #[serde(default = "default_ollama_base")]
    pub ollama_base: String,
    #[serde(default = "default_ollama_model")]
    pub ollama_model: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SummarizerBackend {
    Disabled,
    Openai,
    Ollama,
}
```

---

## 6. Bridge runtime

```rust
pub(crate) struct SummarizerBridge {
    request_tx: flume::Sender<BridgeRequest>,
    _runtime: tokio::runtime::Runtime,   // kept alive
}

struct BridgeRequest {
    payload: BridgePayload,
    reply: flume::Sender<Result<String, SummarizerError>>,
}

enum BridgePayload {
    OpenAi { /* prompt, model, temperature, api_key, base */ },
    Ollama { /* prompt, model, base */ },
}

impl SummarizerBridge {
    pub fn new(timeout: Duration) -> io::Result<Self> {
        let (tx, rx) = flume::bounded::<BridgeRequest>(64);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .thread_name("brain-llm")
            .build()?;
        // Use the runtime's spawn from the OS thread we just created.
        // We park the runtime on its own thread by `Runtime::block_on`
        // — but that's blocking. Instead we use the multi-thread
        // builder with 1 worker:
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("brain-llm")
            .enable_all()
            .build()?;
        runtime.spawn(worker_loop(rx, timeout));
        Ok(Self { request_tx: tx, _runtime: runtime })
    }
}

async fn worker_loop(rx: flume::Receiver<BridgeRequest>, timeout: Duration) {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .expect("reqwest client");
    while let Ok(req) = rx.recv_async().await {
        let result = match req.payload {
            BridgePayload::OpenAi { .. } => openai::call(&client, ...).await,
            BridgePayload::Ollama { .. } => ollama::call(&client, ...).await,
        };
        let _ = req.reply.send_async(result).await;
    }
}
```

Drop semantics: when the `SummarizerBridge` drops (server shutdown),
`request_tx` drops → worker loop's `rx.recv_async()` returns Err →
worker exits → runtime is dropped → background thread joins.

---

## 7. OpenAI wire shape

POST `<openai_api_base>/chat/completions` with:

```json
{
  "model": "gpt-4o-mini",
  "messages": [
    {"role": "system", "content": "You are a memory consolidation system..."},
    {"role": "user", "content": "Memories:\n1. ...\n2. ...\n\nSummary:"}
  ],
  "temperature": 0.3,
  "max_tokens": 1024
}
```

Headers: `authorization: Bearer <api_key>`, `content-type: application/json`.

Response is parsed as:
```json
{"choices": [{"message": {"content": "<summary>"}}]}
```

`max_tokens` from `cfg.summarizer.max_summary_chars / 4` (rough tokens
estimate).

Errors: 4xx → `SummarizerError::Failed(format!("openai 4xx: {body}"))`;
5xx + timeouts → `SummarizerError::Failed`. The worker treats both
the same — logs and skips the cycle.

## 8. Ollama wire shape

POST `<ollama_base>/api/generate` with:

```json
{
  "model": "llama3.1:8b",
  "prompt": "<full prompt>",
  "stream": false,
  "options": {"temperature": 0.3}
}
```

No auth.

Response (non-streaming):
```json
{"response": "<summary>", "done": true, ...}
```

---

## 9. Wire-up

### 9.1 `register_phase8_workers` signature

Today:
```rust
fn register_phase8_workers(
    scheduler: &mut WorkerScheduler,
    ops: Arc<OpsContext>,
    rebuild_source: Arc<dyn RebuildSource<{ VECTOR_DIM }>>,
    wal_retention_source: Arc<dyn WalRetentionSource>,
    snapshot_source: Arc<dyn SnapshotSource>,
    cache_eviction_source: Arc<dyn CacheEvictionSource>,
) -> Result<(), brain_workers::WorkerError> {
    scheduler.register(
        Arc::new(ConsolidationWorker::new(Arc::new(DisabledSummarizer))),
        ops.clone(),
    )?;
    // ...
}
```

9.15:
```rust
fn register_phase8_workers(
    scheduler: &mut WorkerScheduler,
    ops: Arc<OpsContext>,
    rebuild_source: ...,
    wal_retention_source: ...,
    snapshot_source: ...,
    cache_eviction_source: ...,
    summarizer: Arc<dyn Summarizer>,            // NEW
) -> ... {
    scheduler.register(
        Arc::new(ConsolidationWorker::new(summarizer)),
        ops.clone(),
    )?;
    // ...
}
```

### 9.2 main.rs build

```rust
let summarizer: Arc<dyn Summarizer> = match build_summarizer(&cfg) {
    Ok(s) => s,
    Err(e) => {
        tracing::error!(error = %e, "summarizer construction failed");
        return ExitCode::FAILURE;
    }
};
```

The shard spawn pipes `summarizer.clone()` through to the Glommio
closure via `ShardSpawnConfig` (new field).

### 9.3 ShardSpawnConfig

```rust
pub struct ShardSpawnConfig {
    // ... existing fields ...
    pub summarizer: Arc<dyn Summarizer>,
}
```

Default factory:
```rust
impl ShardSpawnConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            // ...
            summarizer: Arc::new(DisabledSummarizer),
        }
    }
}
```

Existing tests don't need updates — they hit the default and get the
disabled summarizer.

---

## 10. Tests (`tests/summarizer.rs`)

A hand-rolled mock HTTP server lives in the test file: bind a
`tokio::net::TcpListener` on `127.0.0.1:0`, accept one connection,
read the request, return a canned JSON response.

1. **`openai_round_trips_a_summary`** — point an `OpenAiSummarizer`
   at the mock server. Call `summarize(&["hello", "world"])`. Mock
   echoes `{"choices":[{"message":{"content":"summary text"}}]}`.
   Assert the returned `Ok("summary text")`.
2. **`openai_4xx_surfaces_as_failed`** — mock returns 401. Expect
   `SummarizerError::Failed(...)` matching `/401/`.
3. **`ollama_round_trips_a_summary`** — symmetric: mock returns
   `{"response":"summary","done":true}`.
4. **`disabled_summarizer_always_disabled`** — sanity check: the
   factory with `backend == "disabled"` returns `DisabledSummarizer`
   regardless of which features are compiled in.

(Real OpenAI / Ollama integration is the operator's job; testing
against the actual services from CI would require API keys + an
Ollama install. The mock HTTP server proves wire correctness +
error mapping.)

Tests are gated `#[cfg(all(feature = "summarizer-openai",
feature = "summarizer-ollama"))]` so `cargo test` without features
still works; `just docker-verify` enables both features explicitly.

---

## 11. Risks

| Risk | Mitigation |
| ---- | ---------- |
| reqwest brings rustls + ~30 deps | Feature-gated. `default-features = false + rustls-tls` reuses the rustls already vendored for 9.9; no openssl bloat. |
| Bridge runtime adds a permanent OS thread even when summarizer is disabled | We only construct the bridge when `backend != "disabled"`. `DisabledSummarizer` has no runtime, no thread. |
| Reqwest async future awaited inside Glommio = undefined behavior | The bridge pattern eliminates this. `summarize()` returns a flume-backed future that's Glommio-safe. |
| OpenAI rate limits / 429 storm | We log + skip the cycle; the consolidation worker re-tries on its own interval (10 min). v2 adds the spec §11/09 §6 circuit breaker. |
| API key leak in logs | `OpenAiSummarizer` captures the key as `Secret<String>` (no `Debug` printing) and reads from env at startup. We never log the key. |
| Mock HTTP server is flaky if test parallelism + ephemeral ports collide | `127.0.0.1:0` is safe; each test gets a fresh listener. |
| `cargo test` without features fails because `tests/summarizer.rs` references gated types | Each test is `#[cfg(feature = "summarizer-…")]`-gated; the file compiles to zero tests when features are off. |
| `just docker-verify` doesn't enable features | Bump the verify recipe to `cargo test --workspace --all-features` for brain-server, OR add a sibling recipe. Simpler: leave default `cargo test --workspace` running zero summarizer tests, and add `cargo test -p brain-server --features summarizer-openai --features summarizer-ollama` as an explicit step. |

---

## 12. Done criteria

- [ ] `crates/brain-server/src/llm/{mod,bridge,openai,ollama,prompt,factory}.rs` ship.
- [ ] `crates/brain-server/src/config.rs` gains `SummarizerConfig` + `SummarizerBackend`.
- [ ] `register_phase8_workers` takes `Arc<dyn Summarizer>`; `main.rs` builds it once and threads through `ShardSpawnConfig`.
- [ ] `config/dev.toml` carries `[summarizer] backend = "disabled"`.
- [ ] `Cargo.toml` gains `reqwest` + `serde_json` as optional deps + two features.
- [ ] 4 mock-server integration tests pass under
  `cargo test -p brain-server --features summarizer-openai --features summarizer-ollama`.
- [ ] `just docker-verify` (no features) green workspace-wide;
  feature-tests run as an explicit sibling.
- [ ] Phase doc 9.15 marked `[x]`.

---

## 13. What 9.15 explicitly defers

- **Circuit breaker** — spec §11/09 §6's "pauses for a longer
  interval" behavior. v2 layers on the existing log+skip path.
- **Retries** — single shot per cycle; the next cycle retries.
- **Streaming responses** — we wait for the full completion.
- **Anthropic / Azure OpenAI / Cohere / etc.** — share the OpenAI
  adapter shape; v2 adds wire variants as needed.
- **Prompt customisation via TOML** — operators with a custom prompt
  use the spec default; v2 adds a `summarizer.prompt_template` field.
- **Token-level cost accounting** — v2 metric family.

---

*Implement on approval.*
