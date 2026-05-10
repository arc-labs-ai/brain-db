# 13.08 Test Support

What the SDK provides for testing.

## 1. The test challenge

Testing applications that use the Brain SDK has typical issues:

- Real substrate connections require a running substrate (heavy).
- Mock all calls manually = a lot of boilerplate.
- Want to test specific scenarios (errors, slow responses, etc.).

The SDK provides:

- A **mock client** with programmable responses.
- An **in-process fake substrate** for end-to-end testing.
- Test fixtures and utilities.

## 2. The mock client

```rust
let mock = MockClient::new();
mock.on_encode(|req| {
    Ok(EncodeResult {
        memory_id: MemoryId::test(1),
        ...
    })
});

mock.on_recall(|req| {
    if req.cue_text == "hello" {
        Ok(vec![RecallResult { ... }])
    } else {
        Err(BrainError::NotFound)
    }
});

// Use mock as a regular client
let id = mock.encode("text").send().await?;
```

```python
mock = brain.testing.MockClient()
mock.expect_encode(returns=brain.MemoryId(b'\x01' * 16))
mock.expect_recall(when=lambda req: req.cue == "hello", returns=[...])

memory_id = await mock.encode("text", agent_id="test")
```

The mock is functionally identical to a real client; calls are recorded, responses are programmed.

## 3. Call recording

The mock records all calls:

```rust
let calls = mock.calls();
assert_eq!(calls.encode_count(), 1);
assert_eq!(calls.recall_count(), 0);
let last_encode = calls.last_encode().unwrap();
assert_eq!(last_encode.text, "text");
```

Tests can verify the application's interaction with the SDK.

## 4. The fake substrate

For higher-fidelity tests, an in-process fake substrate:

```rust
let fake = FakeSubstrate::new();
let client = Client::connect_to_fake(&fake).await?;

client.encode("text").send().await?;
let results = client.recall("text").send().await?;
assert_eq!(results.len(), 1);
```

The fake:
- Accepts wire-protocol calls from a real Client.
- Maintains in-memory state (memories, edges, contexts).
- Implements simplified versions of the operations.

It's not the real substrate (no HNSW, no WAL); it's just enough to test client logic end-to-end.

## 5. The fake's fidelity

The fake implements:
- ENCODE: records the memory in an in-memory store.
- RECALL: simple text similarity (or programmable).
- FORGET: removes the memory.
- LINK / UNLINK: edge tracking.
- TXN: groups operations.
- SUBSCRIBE: streams events.

The fake doesn't implement:
- Vector embedding (uses placeholder vectors).
- HNSW search (uses placeholder similarity).
- Real durability (in-memory only).

For tests of client behavior, this is enough.

## 6. The "real substrate" for integration tests

For tests that need the real substrate:

```rust
#[test]
async fn integration_test() {
    let substrate = TestSubstrate::start_in_memory().await?;
    let client = Client::connect(substrate.address()).await?;
    
    // Run real test against real substrate
    let id = client.encode("real text").send().await?;
    let results = client.recall("real text").send().await?;
    
    substrate.shutdown().await?;
}
```

`TestSubstrate` spins up a real Brain process with in-memory storage:

- No persistent files.
- Single shard.
- Auto-shutdown.

Integration tests are slower (~seconds to start) but exercise the full stack.

## 7. The fixture library

Common test fixtures:

```rust
let fixtures = brain::testing::Fixtures::new();
let agent = fixtures.agent("test-agent");
let memory = fixtures.encode("test memory", &agent);
let context = fixtures.context("test-context", &agent);
```

Saves boilerplate in tests.

## 8. The deterministic test mode

For deterministic tests:

```rust
let client = Client::builder()
    .deterministic_request_ids(seed = 42)
    .deterministic_clock(start_at = "2026-05-07T00:00:00Z")
    .build();
```

Request IDs become deterministic (based on the seed). Time-based features (timestamps) use the fake clock.

This makes tests reproducible — same input, same output, every time.

## 9. The chaos / failure injection

```rust
let mock = MockClient::new();
mock.inject_failure_rate(0.1);    // 10% of calls fail with random errors
mock.inject_latency(Duration::from_millis(100));    // All calls have 100ms delay
mock.inject_intermittent_disconnect();
```

Tests for retry / error handling logic.

## 10. The schema tests

For validating that custom code matches the wire protocol:

```rust
brain::testing::wire_schema_test! {
    #[test]
    fn test_encode_request_schema() {
        let req = EncodeRequest { ... };
        let bytes = serialize(&req);
        let parsed: EncodeRequest = deserialize(&bytes)?;
        assert_eq!(req, parsed);
    }
}
```

Ensures clients and substrates agree on the wire format.

## 11. The "test the SDK itself" tests

The SDK has its own test suite:

- Unit tests for individual functions.
- Integration tests against the fake substrate.
- End-to-end tests against a real substrate.
- Property tests for invariants.

This is the SDK's quality bar. Application authors don't run these (they're internal to the SDK).

## 12. The "shared test suite"

A canonical test suite verifies any SDK conforms to the spec:

```
tests/
├── conformance/
│   ├── encode_basic.yaml
│   ├── recall_filters.yaml
│   ├── transactions.yaml
│   └── ...
└── ...
```

Each YAML describes a scenario:

```yaml
name: encode_then_recall
steps:
  - operation: encode
    text: "hello world"
    agent_id: agent-1
    expect:
      success: true
      memory_id: not_null
  - operation: recall
    cue: "hello"
    agent_id: agent-1
    expect:
      results_count: 1
      first_score: ">0.5"
```

Each language SDK runs the same scenarios and verifies its output. Conformance is checkable.

## 13. The migration testing

For SDK upgrades:

```rust
brain::testing::backward_compat_test! {
    fn test_v1_request_works_in_v2() {
        let v1_request = build_v1_encode_request();
        let v2_response = v2_substrate.handle(v1_request);
        // Verify v2 substrate accepts and responds correctly.
    }
}
```

Ensures cross-version compatibility.

## 14. The "snapshot testing" pattern

For complex outputs:

```rust
let response = client.plan("goal").send().await?;
insta::assert_yaml_snapshot!(response);
```

Snapshot files capture expected output; tests fail if output diverges.

For text-heavy responses (PLAN, REASON), snapshot testing prevents regressions in output formatting.

## 15. The "load testing" facility

```rust
let client = Client::connect(substrate).await?;
let load = brain::testing::LoadGenerator::new()
    .ops_per_second(1000)
    .duration(Duration::from_secs(60))
    .pattern(LoadPattern::EncodeRecall { ratio: 0.7 });

let stats = load.run(&client).await?;
println!("p99 latency: {}", stats.p99_latency);
```

For benchmarking the SDK and substrate together. Used in CI for regression detection.

## 16. The "test isolation"

Each test should be isolated:

- Fresh Client per test.
- Fresh test data (or rolled back).
- No global state leaking between tests.

The SDK's test utilities support this:

```rust
let _guard = brain::testing::IsolatedTest::start();
// Test code here.
// Guard's drop cleans up.
```

## 17. The "production safety" check

A common test: make sure the test suite doesn't accidentally talk to production:

```rust
fn assert_test_environment() {
    let env = std::env::var("BRAIN_ENV").unwrap();
    assert!(env == "test" || env == "staging");
}
```

The SDK can panic if production endpoints are used in tests.

---

*Continue to [`09_versioning.md`](09_versioning.md) for SDK versioning.*
