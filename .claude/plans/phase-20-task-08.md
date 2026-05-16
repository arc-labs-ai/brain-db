# 20.8 — Wire opcodes 0x0124-0x0126

`EXTRACTOR_LIST` / `_DISABLE` / `_ENABLE` over the wire per
spec §28/05 §6-§7. Single-frame snapshot pattern matches §28/05's
"streaming" wording loosely (v1 fits the list in one frame; phase
23 may split into actual streaming if extractor counts demand it).

## Files written / modified

| Path | Change |
|---|---|
| `crates/brain-protocol/src/knowledge/extractor_req.rs` | New: `ExtractorListRequest`, `ExtractorDisableRequest`, `ExtractorEnableRequest`. |
| `crates/brain-protocol/src/knowledge/extractor_resp.rs` | New: `ExtractorListResponseFrame`, `ExtractorListItem`, `ExtractorDisableResponse`, `ExtractorEnableResponse`. |
| `crates/brain-protocol/src/knowledge/mod.rs` | Module + re-exports. |
| `crates/brain-protocol/src/opcode.rs` | Add 6 opcode entries (req 0x0124-0x0126, resp 0x01A4-0x01A6). |
| `crates/brain-protocol/src/request.rs` | Add 3 variants + opcode/encode/decode wiring. |
| `crates/brain-protocol/src/response.rs` | Add 3 variants + wiring. |
| `crates/brain-ops/src/ops/knowledge_extractor.rs` | New: 3 handlers. |
| `crates/brain-ops/src/ops/mod.rs` | Module declaration. |
| `crates/brain-ops/src/lib.rs` | Re-export. |
| `crates/brain-ops/src/dispatch.rs` | 3 dispatch arms. |

## Wire types

```rust
// extractor_req.rs

pub struct ExtractorListRequest {
    pub include_disabled: bool,
}

pub struct ExtractorDisableRequest {
    pub extractor_id: u32,
    pub reason: String,            // ≤ 4 KiB
    pub request_id: WireUuid,
}

pub struct ExtractorEnableRequest {
    pub extractor_id: u32,
    pub request_id: WireUuid,
}
```

```rust
// extractor_resp.rs

pub struct ExtractorListItem {
    pub extractor_id: u32,
    pub namespace: String,
    pub name: String,
    pub kind: u8,                  // 0=pattern, 1=classifier, 2=llm
    pub enabled: bool,
    pub schema_version: u32,
    pub created_at_unix_nanos: u64,
}

pub struct ExtractorListResponseFrame {
    pub items: Vec<ExtractorListItem>,
    pub total: u32,
    pub is_final: bool,            // always true in v1
}

pub struct ExtractorDisableResponse {
    pub previously_enabled: bool,
    pub disabled_at_unix_nanos: u64,
}

pub struct ExtractorEnableResponse {
    pub previously_disabled: bool,
    pub enabled_at_unix_nanos: u64,
}
```

## Opcode assignments

```
0x0124  EXTRACTOR_LIST_REQ        0x01A4  EXTRACTOR_LIST_RESP
0x0125  EXTRACTOR_DISABLE_REQ     0x01A5  EXTRACTOR_DISABLE_RESP
0x0126  EXTRACTOR_ENABLE_REQ      0x01A6  EXTRACTOR_ENABLE_RESP
```

Responses sit in `0x01A4-0x01A6`, completing the schema-and-extractor
response range from phase 19.6's `0x01A0-0x01A3`.

## Handlers (`knowledge_extractor.rs`)

```rust
pub async fn handle_extractor_list(
    req: ExtractorListRequest,
    ctx: &OpsContext,
) -> Result<ExtractorListResponseFrame, OpError>;

pub async fn handle_extractor_disable(
    req: ExtractorDisableRequest,
    ctx: &OpsContext,
) -> Result<ExtractorDisableResponse, OpError>;

pub async fn handle_extractor_enable(
    req: ExtractorEnableRequest,
    ctx: &OpsContext,
) -> Result<ExtractorEnableResponse, OpError>;
```

LIST: opens rtxn → `extractor_list(rtxn)` → filter by
`include_disabled` → project rows to `ExtractorListItem`. The
returned list is read from persisted state, not the in-memory
registry, so it reflects what's on disk (matches `SCHEMA_GET` /
`SCHEMA_LIST` behavior).

DISABLE / ENABLE: opens wtxn → `extractor_set_enabled(wtxn, id,
flag)` → captures previous state → updates the in-memory
`ExtractorRegistry` via `ctx.extractor_registry.write()` →
commits wtxn → emits no event (§28/05 §7.2 — "non-disruptive,
no event").

Errors:
- `extractor_id == 0` → `OpError::InvalidRequest`.
- Unknown id → `OpError::NotFound { what: "extractor", detail:
  "id N" }`.
- `reason.len() > 4096` on DISABLE → `OpError::InvalidRequest`.

## Tests

Unit-level helpers (pure functions): trivial map of
`ExtractorDefinition` row → `ExtractorListItem`. Handler tests
need a full `OpsContext` so wire-level integration moves to
20.9.

In-crate tests cover:
- `extractor_list_item_round_trips` — wire shape.
- `extractor_disable_response_round_trips`.
- `extractor_enable_response_round_trips`.

## Out of scope

- Admin auth (§28/05 §8) — phase 21 admin.
- Streaming `EXTRACTOR_LIST` — phase 23 if needed.
- Event emission on DISABLE/ENABLE — per spec §28/05 §7.2,
  intentionally absent.

## Single commit

`feat(protocol,ops): 20.8 — EXTRACTOR_LIST / _DISABLE / _ENABLE wire ops`

## Verification

```
just docker cargo test -p brain-protocol --lib knowledge::extractor
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets -- -D warnings
```
