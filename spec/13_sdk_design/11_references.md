# 13.11 References

References for SDK design.

## 1. SDK design literature

- **Bloch, "How to Design a Good API and Why it Matters" (2006).** [research.google/pubs/pub32713](https://research.google/pubs/pub32713/). Classic talk on API design principles.

- **Henney, "API Design Matters" (2009).** [acm.org/conferences/event](https://queue.acm.org/detail.cfm?id=1255422). ACM Queue article.

## 2. Reference SDK examples

- **AWS SDK for Rust** — [github.com/awslabs/aws-sdk-rust](https://github.com/awslabs/aws-sdk-rust). Modern Rust SDK design.

- **Stripe API libraries** — [github.com/stripe](https://github.com/stripe). Multi-language SDK with strong consistency.

- **gRPC libraries** — [grpc.io](https://grpc.io/). For comparison; Brain uses custom protocol.

## 3. Connection management

- **Caitie McCaffrey, "Reliable Connections at Twitter" (2014).** Practical connection-pooling patterns.

- **Netflix's Hystrix (now Resilience4j)** — [resilience4j.readme.io](https://resilience4j.readme.io/). Circuit-breaker patterns.

## 4. Retry patterns

- **AWS Architecture Center, "Exponential Backoff and Jitter" (2015).** [aws.amazon.com/blogs/architecture](https://aws.amazon.com/blogs/architecture/exponential-backoff-and-jitter/). The classic article on retry patterns.

- **Stripe's idempotency guide** — [stripe.com/docs/api/idempotent_requests](https://stripe.com/docs/api/idempotent_requests).

## 5. Tracing and observability

- **OpenTelemetry** — [opentelemetry.io](https://opentelemetry.io/). The standard for observability instrumentation.

- **Distributed Tracing in Practice (Sambasivan et al.)** — book covering tracing concepts.

## 6. Idiomatic language patterns

- **Effective Rust** — [effective-rust.com](https://www.lurklurk.org/effective-rust/). Rust idioms.

- **The Hitchhiker's Guide to Python** — [docs.python-guide.org](https://docs.python-guide.org/). Pythonic style.

- **TypeScript Handbook** — [typescriptlang.org/docs/handbook](https://www.typescriptlang.org/docs/handbook/intro.html).

- **Effective Go** — [go.dev/doc/effective_go](https://go.dev/doc/effective_go).

## 7. SemVer and versioning

- **Semantic Versioning specification** — [semver.org](https://semver.org/).

- **Trunk Based Development** — [trunkbaseddevelopment.com](https://trunkbaseddevelopment.com/). Versioning patterns.

## 8. Testing patterns

- **xUnit Test Patterns (Meszaros, 2007).** Comprehensive testing patterns.

- **Mock/stub/spy distinction** — Martin Fowler's article: [martinfowler.com/articles/mocksArentStubs.html](https://martinfowler.com/articles/mocksArentStubs.html).

- **Property-based testing** — [hypothesis.works](https://hypothesis.works/) (Python), [proptest](https://github.com/proptest-rs/proptest) (Rust).

## 9. Language-specific async references

- **Rust Async Book** — [rust-lang.github.io/async-book](https://rust-lang.github.io/async-book/).

- **Python asyncio** — [docs.python.org/3/library/asyncio.html](https://docs.python.org/3/library/asyncio.html).

- **Go context and concurrency** — [go.dev/blog/context](https://go.dev/blog/context).

## 10. Brain-internal references

- See [03. Wire Protocol](../03_wire_protocol/) for the protocol the SDK speaks.
- See [09. Cognitive Operations](../09_cognitive_operations/) for the semantic operations.
- See [12. Sharding + Clustering](../12_sharding_clustering/) for routing.
