# 13.06 Idiomatic Language Specifics

How the SDK adapts to each language's idioms.

## 1. Rust SDK

### Builder pattern

```rust
let memory_id = client
    .encode("text content")
    .agent("agent-id")
    .context("context-name")
    .kind(MemoryKind::Episodic)
    .send()
    .await?;
```

Each chained method returns `self` (or a builder). `.send()` finalizes and dispatches.

### Result types

```rust
pub type BrainResult<T> = Result<T, BrainError>;
```

All operations return `BrainResult`. The user uses `?` for short-circuit error handling.

### Async via tokio

The Rust SDK uses tokio (most popular Rust async runtime). For users who need other runtimes, an alternative `brain-async-std` SDK can be provided.

### Cargo features

```toml
[dependencies]
brain-sdk = { version = "1.0", features = ["tls", "tracing"] }
```

Optional features: `tls`, `tracing`, `metrics`, `compression`. Users opt in.

### Type-safe IDs

```rust
pub struct MemoryId([u8; 16]);
pub struct AgentId([u8; 16]);
pub struct ContextId([u8; 16]);

// Can't accidentally pass an AgentId where a MemoryId is expected.
```

The compiler catches mistakes.

### Examples

```rust
use brain::Client;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::connect("localhost:9090").await?;

    let id = client
        .encode("hello world")
        .agent("agent-001")
        .send()
        .await?;

    let results = client
        .recall("hello")
        .agent("agent-001")
        .k(5)
        .send()
        .await?;

    for r in &results {
        println!("{}: {:.2}", r.memory_id, r.score);
    }

    Ok(())
}
```

## 2. Python SDK

### Async-first with sync wrappers

```python
import brain

client = brain.Client("localhost:9090")

# Async usage
async def main():
    memory_id = await client.encode(
        text="hello",
        agent_id="agent-001",
    )

# Sync usage (uses asyncio.run internally)
sync_client = brain.SyncClient("localhost:9090")
memory_id = sync_client.encode(text="hello", agent_id="agent-001")
```

The async client is preferred. The sync client exists for scripts that don't want async.

### Keyword arguments

```python
results = await client.recall(
    cue="hello",
    agent_id="agent-001",
    k=10,
    filter=brain.RecallFilter(
        kind=brain.MemoryKind.EPISODIC,
        min_salience=0.3,
    ),
    include_text=True,
)
```

Pythonic — keyword args make the call self-documenting.

### Type hints

```python
async def encode(
    self,
    text: str,
    agent_id: str,
    context: str = "_default",
    kind: brain.MemoryKind = brain.MemoryKind.EPISODIC,
    salience: float = 1.0,
    edges: list[brain.EdgeSpec] = None,
    request_id: brain.RequestId = None,
) -> brain.MemoryId: ...
```

Type-hinted (PEP 484+) for IDE support and static analysis.

### Errors as exceptions

```python
try:
    await client.encode(...)
except brain.QuotaExceeded as e:
    print(f"Quota: {e.limit}")
except brain.OverloadedError as e:
    if e.retryable:
        await asyncio.sleep(e.retry_after)
        # retry
```

Exception hierarchy. Pythonic.

### Async iterators for streams

```python
async for event in client.subscribe(agent_id="agent-001"):
    process(event)
```

Standard `async for` syntax.

### Pip distribution

```
pip install brain-sdk
```

Standard PyPI package.

## 3. TypeScript SDK

### Object literal config

```typescript
import { BrainClient } from "@brain/sdk";

const client = new BrainClient({
  servers: ["localhost:9090"],
  auth: { type: "token", value: "..." },
  timeoutMs: 30000,
});

const memoryId = await client.encode({
  text: "hello",
  agentId: "agent-001",
  context: "conversation",
});

const results = await client.recall({
  cue: "hello",
  agentId: "agent-001",
  k: 10,
});
```

Object literals match TypeScript / JavaScript ergonomics.

### Promises

All operations return Promises. Standard `await` works.

### Type interfaces

```typescript
interface RecallOptions {
  cue: string;
  agentId: string;
  k?: number;
  filter?: RecallFilter;
  includeText?: boolean;
}

interface RecallResult {
  memoryId: MemoryId;
  score: number;
  text?: string;
  metadata?: MemoryMetadata;
}
```

Strong typing via interfaces.

### Errors as classes

```typescript
try {
  await client.encode(...);
} catch (err) {
  if (err instanceof QuotaExceededError) {
    // ...
  }
}
```

Error class hierarchy.

### Streams via AsyncIterator

```typescript
for await (const event of client.subscribe({ agentId: "..." })) {
  process(event);
}
```

Modern async iterator syntax.

### npm distribution

```
npm install @brain/sdk
```

## 4. Go SDK

### Struct config

```go
client, err := brain.NewClient(brain.Config{
    Servers: []string{"localhost:9090"},
    Auth:    brain.TokenAuth("..."),
    Timeout: 30 * time.Second,
})

memoryID, err := client.Encode(ctx, brain.EncodeRequest{
    Text:    "hello",
    AgentID: "agent-001",
    Context: "conversation",
})

results, err := client.Recall(ctx, brain.RecallRequest{
    Cue:     "hello",
    AgentID: "agent-001",
    K:       10,
})
```

Go style: struct configs, errors as values, contexts for cancellation.

### Error checking

Standard Go pattern:

```go
result, err := client.Encode(...)
if err != nil {
    return err
}
```

### Cancellation via context

```go
ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
defer cancel()

result, err := client.Recall(ctx, ...)
```

The SDK respects context cancellation.

### Streams via channels

```go
stream, err := client.Subscribe(ctx, brain.SubscribeRequest{...})
if err != nil {
    return err
}

for event := range stream {
    process(event)
}
```

The stream returns a channel.

### Module distribution

```go
go get github.com/brain-substrate/brain-go
```

## 5. The "consistency across SDKs"

Across languages, the same operation produces the same wire-protocol calls. The substrate doesn't know which language called it.

What differs:
- Method names (snake_case vs camelCase vs PascalCase).
- Async patterns (await vs goroutines vs Promises).
- Error mechanisms (Result vs exceptions vs error returns).
- Distribution (Cargo vs pip vs npm vs Go modules).

What stays the same:
- The behavior.
- The semantics.
- The performance characteristics.

## 6. The "switching languages" experience

A user familiar with the Python SDK should be able to learn the Rust SDK quickly:

- Method names map (encode, recall, plan, etc.).
- Parameters are similar.
- Result shapes are the same.

This consistency makes Brain language-agnostic.

## 7. The "minimal SDK" rule

A minimal SDK in any language:

- Connection management.
- Five primitives + LINK / UNLINK.
- Error handling.
- Builder/config types.

This is achievable in 1500-2500 lines per language.

A "full" SDK adds: transactions, subscribe, admin, observability, advanced retries — about 3000-5000 lines.

---

*Continue to [`07_observability.md`](07_observability.md) for SDK observability.*
