# 13.02 Core API

The shape of the SDK's core API. Specific to the operations Brain supports.

## 1. The Client

Every SDK has a `Client` type:

```rust
// Rust
let client = Client::builder()
    .servers(["host1:9090", "host2:9090"])
    .auth(AuthMethod::Token("..."))
    .timeout(Duration::from_secs(30))
    .build()
    .await?;
```

```python
# Python
client = brain.Client(
    servers=["host1:9090", "host2:9090"],
    auth=brain.AuthToken("..."),
    timeout=30.0,
)
```

```typescript
// TypeScript
const client = new BrainClient({
  servers: ["host1:9090", "host2:9090"],
  auth: { type: "token", value: "..." },
  timeoutMs: 30000,
});
```

```go
// Go
client, err := brain.NewClient(brain.Config{
    Servers: []string{"host1:9090"},
    Auth:    brain.TokenAuth("..."),
    Timeout: 30 * time.Second,
})
```

The Client is the entry point for all operations.

## 2. Connection management

The Client maintains a connection pool:

- Multiple connections per server.
- Connection reuse across requests.
- Auto-reconnect on disconnect.
- Keep-alive frames per the wire protocol.

The user doesn't directly manage connections.

## 3. ENCODE

```rust
// Rust
let memory_id = client.encode("text content")
    .agent("agent-id")
    .context("context-name")
    .kind(MemoryKind::Episodic)
    .salience(0.8)
    .edges(vec![
        EdgeSpec::new(EdgeKind::CausedBy, prior_memory_id, 0.9),
    ])
    .send()
    .await?;
```

```python
# Python
memory_id = await client.encode(
    text="text content",
    agent_id="agent-id",
    context="context-name",
    kind=brain.Episodic,
    salience=0.8,
    edges=[brain.Edge(brain.EdgeKind.CAUSED_BY, prior_id, 0.9)],
)
```

The builder pattern (Rust) and keyword args (Python) achieve the same flexibility.

## 4. RECALL

```rust
// Rust
let results = client.recall("cue text")
    .agent("agent-id")
    .k(10)
    .filter_kind(MemoryKind::Episodic)
    .filter_min_salience(0.3)
    .include_text(true)
    .send()
    .await?;

for result in results {
    println!("{}: {}", result.score, result.text.unwrap());
}
```

```python
# Python
results = await client.recall(
    cue="cue text",
    agent_id="agent-id",
    k=10,
    filter=brain.RecallFilter(kind=brain.Episodic, min_salience=0.3),
    include_text=True,
)

for r in results:
    print(f"{r.score}: {r.text}")
```

The result is iterable; each element is a `RecallResult` with score, memory_id, optional text and metadata.

## 5. PLAN

```rust
let plan = client.plan("goal: book a flight")
    .agent("agent-id")
    .starting_state("user wants to travel")
    .max_depth(4)
    .max_results(10)
    .edge_kinds([EdgeKind::Caused, EdgeKind::FollowedBy])
    .send()
    .await?;

for path in plan.paths {
    for step in path.steps {
        println!("- {}", step.text.unwrap_or_default());
    }
}
```

PLAN returns paths; each path is a sequence of steps. The agent uses these to construct an actual plan.

## 6. REASON

```rust
let reasoning = client.reason("the user is unhappy")
    .agent("agent-id")
    .max_supporting(5)
    .max_contradicting(5)
    .send()
    .await?;

for evidence in reasoning.supporting {
    println!("Supports: {}", evidence.text.unwrap());
}
for evidence in reasoning.contradicting {
    println!("Contradicts: {}", evidence.text.unwrap());
}
println!("Confidence: {}", reasoning.confidence);
```

REASON returns evidence with confidence. The agent uses this for explainable reasoning.

## 7. FORGET

```rust
client.forget(memory_id).send().await?;

// Or batch:
client.forget_batch([id1, id2, id3]).send().await?;

// Or by filter:
client.forget()
    .agent("agent-id")
    .context("conversation_42")
    .max_age(Duration::from_days(30))
    .send()
    .await?;
```

FORGET supports single, batch, and filter modes. Each is a separate method or builder configuration.

## 8. LINK / UNLINK

```rust
client.link(source_id, EdgeKind::Caused, target_id, 0.9).send().await?;
client.unlink(source_id, EdgeKind::Caused, target_id).send().await?;
```

Direct edge manipulation.

## 9. Transactions

```rust
let txn = client.txn().begin().await?;

let m1 = txn.encode("first thing").send().await?;
let m2 = txn.encode("second thing").send().await?;
txn.link(m1, EdgeKind::FollowedBy, m2, 1.0).send().await?;

txn.commit().await?;
// or txn.abort().await?;
```

The transaction object groups operations. Commit/abort is explicit.

## 10. SUBSCRIBE

```rust
let mut stream = client.subscribe()
    .agent("agent-id")
    .events([EventKind::MemoryCreated, EventKind::MemoryForgotten])
    .start();

while let Some(event) = stream.next().await? {
    match event.kind {
        EventKind::MemoryCreated => println!("New: {}", event.memory_id),
        EventKind::MemoryForgotten => println!("Gone: {}", event.memory_id),
        _ => {}
    }
}
```

The stream is async iteration; each iteration yields an event.

## 11. Admin

```rust
let stats = client.admin().stats().send().await?;
println!("Memory count: {}", stats.memory_count);

client.admin().rebuild_ann("shard-uuid").send().await?;
client.admin().snapshot_create("snapshot-name").send().await?;
```

Admin operations are namespaced under a separate object (not on the regular Client) — they're privileged and rarely used.

## 12. The result types

For each operation, a typed result:

- `MemoryId` — opaque ID.
- `RecallResult` — score, memory_id, optional text, metadata.
- `PlanResult` — paths.
- `ReasonResult` — supporting/contradicting evidence + confidence.
- `EncodeResult` — memory_id + edge results.
- (and so on)

These are simple data types; no methods that hide work.

## 13. The error types

A unified `BrainError` (or per-language equivalent):

```rust
pub enum BrainError {
    InvalidRequest(InvalidRequestDetails),
    NotFound,
    QuotaExceeded,
    Unauthorized,
    Conflict,
    Overloaded,
    Timeout,
    NetworkError(NetworkErrorDetails),
    InternalError(String),
}

impl BrainError {
    pub fn is_retryable(&self) -> bool { ... }
    pub fn code(&self) -> ErrorCode { ... }
}
```

The user matches on the variant or checks `is_retryable`.

## 14. Defaults

For every method, sensible defaults:

| Parameter | Default |
|---|---|
| K | 10 |
| consistency | Eventual |
| timeout | 30 seconds |
| retries | 3 |
| backoff | exponential, 100ms initial |

These are configurable per-call or globally on the Client.

## 15. Builder vs keyword args

Different idioms:

- **Builder**: Rust, Java. Method-chained construction.
- **Keyword args**: Python, Ruby. Named arguments to functions.
- **Object literal**: TypeScript, JavaScript. Object passed as one argument.
- **Struct field**: Go. Struct populated then passed.

Each SDK uses its language's most natural pattern.

## 16. The "type-safe" payoff

In typed languages, the SDK leverages the type system:

- Filter rules can't be malformed (compile-time checked).
- Edge kinds are an enum.
- Memory IDs are not interchangeable with other IDs.

The compiler catches errors that would otherwise be runtime exceptions.

## 17. The "untyped" gracefully

In dynamic languages, the SDK still validates:

- Runtime checks on parameter types.
- Clear error messages (not just "TypeError: int got str").

JavaScript/Python users get type information through documentation and runtime checks.

---

*Continue to [`03_connection.md`](03_connection.md) for connection management.*
