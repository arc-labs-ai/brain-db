# 18.9b — Phase 18 exit (bench + ROADMAP + tag)

Mirrors 17.10b.

## Plan

### Step 1 — `crates/brain-metadata/benches/relation_ops.rs`

Criterion bench covering the four operations in §16/02 §2.4:

- `relation_ops::create` — Fact-like ManyToMany asymmetric.
- `relation_ops::get` — point lookup.
- `relation_ops::list_from_subject_filter` — by-from index scan.
- `relation_ops::traverse_depth_1` — minimal BFS hop.

1024-relation fixture; full-scale (1M) targets are operator-run.

### Step 2 — `Cargo.toml`

Add `[[bench]] name = "relation_ops"`.

### Step 3 — `ROADMAP.md`

Mark Phase 18 ✓ with the delivered/deferred sections, mirroring
Phase 16/17.

### Step 4 — Verify + commit

```
cargo zigbuild --target x86_64-unknown-linux-gnu -p brain-metadata --bench relation_ops
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets -- -D warnings
```

### Step 5 — Tag + merge

User-authorised tag: `phase-18-complete`. Merge feature → dev → main
non-fast-forward per session convention. No push to remote.
