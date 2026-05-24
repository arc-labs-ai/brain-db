//! End-to-end scenarios for the ENCODE response renderer.
//!
//! These exercise the path the brain-shell command actually walks —
//! `EncodeRendered::new(resp).with_source(text)` then
//! `brain_explore::dispatch(&item, &ctx, &mut buf)` — rather than
//! poking `render_table` directly the way the inline unit tests do.
//! A regression that breaks the integration of the wrapper + the
//! dispatch (without breaking either method alone) gets caught here.

use std::io::Cursor;

use brain_explore::{dispatch, EncodeRendered, OutputFormat, RenderCtx, TermPolicy, Theme};
use brain_protocol::envelope::request::MemoryKindWire;
use brain_protocol::envelope::response::EncodeResponse;

/// Minimal renderable response. Helpers below override individual
/// fields per scenario so the tests stay focused on one variable
/// at a time. WireUuid is `[u8; 16]`, WireContextId is `u64`,
/// WireMemoryId is `u128` — they're transparent type aliases.
fn sample_response() -> EncodeResponse {
    EncodeResponse {
        memory_id: 0u128,
        was_deduplicated: false,
        salience: 0.70,
        auto_edges_added: 0,
        lsn: 1,
        agent_id: [0u8; 16],
        context_id: 7u64,
        kind: MemoryKindWire::Episodic,
        created_at_unix_nanos: 0,
        edges_out_count: 0,
        embedding_model_fp: [0u8; 16],
        pending_stages: Vec::new(),
        has_active_schema: false,
    }
}

/// Render through `dispatch` (not `render_table`) and return the
/// resulting bytes as a String. `policy.color = false` so the bytes
/// are stable ASCII — snapshot-friendly.
fn render(item: EncodeRendered, format: OutputFormat) -> String {
    let ctx = RenderCtx {
        policy: TermPolicy::plain(),
        theme: Theme::default(),
        format,
    };
    let mut buf = Cursor::new(Vec::new());
    dispatch(&item, &ctx, &mut buf).expect("dispatch must succeed for valid input");
    String::from_utf8(buf.into_inner()).expect("renderer must emit UTF-8")
}

// ---------------------------------------------------------------------------
// Scenario 1 — fresh encode, default table output
// ---------------------------------------------------------------------------

#[test]
fn fresh_encode_table_shows_check_id_lsn_and_content_echo() {
    let resp = sample_response();
    let item = EncodeRendered::new(resp).with_source("Alice merged the auth-rewrite branch");
    let out = render(item, OutputFormat::Table);

    // Heading
    assert!(
        out.contains("✓ ENCODED"),
        "missing fresh-encode heading: {out}"
    );
    assert!(out.contains("LSN 1"), "missing LSN line: {out}");

    // Content echo (the text the substrate received — confirmation
    // that the shell sent what the user typed).
    assert!(
        out.contains("Alice merged the auth-rewrite branch"),
        "source text not echoed: {out}"
    );

    // Metadata — labels + values in the new card layout
    assert!(out.contains("episodic"), "missing kind: {out}");
    assert!(out.contains("salience"), "missing salience label: {out}");
    assert!(out.contains("0.70"), "missing salience value: {out}");
    assert!(out.contains("context"), "missing context label: {out}");

    // What-next hint: subscribe at lsn+1 = 2
    assert!(
        out.contains("subscribe --start-lsn 2"),
        "missing next-step hint at LSN+1: {out}"
    );

    // Defensive: nil agent + zero fingerprint must NOT leak into
    // default-mode output. They're wide-mode-only. (The id row
    // legitimately shows `0x00…` zero bytes for sample memories
    // packed with shard=0/slot=0/version=0; assert on the labeled
    // rows instead of raw zeroes.)
    assert!(
        !out.lines().any(|l| l.contains("  agent  ")),
        "nil agent row leaked into default view: {out}"
    );
    assert!(
        !out.lines().any(|l| l.contains("  embedder  ")),
        "nil embedder row leaked into default view: {out}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 2 — dedup hit, default table output
// ---------------------------------------------------------------------------

#[test]
fn dedup_hit_table_shows_alt_glyph_and_no_fresh_write_signal() {
    let resp = EncodeResponse {
        was_deduplicated: true,
        lsn: 0, // dedup hit reuses the original memory; no fresh WAL record
        ..sample_response()
    };
    let item = EncodeRendered::new(resp).with_source("Alice merged the auth-rewrite branch");
    let out = render(item, OutputFormat::Table);

    // The dedup-hit signal is the heading. It must not say "ENCODED"
    // (would be a regression to the old behavior where the user got
    // no signal that the content matched).
    assert!(
        out.contains("⟳ DEDUP HIT"),
        "missing dedup-hit heading: {out}"
    );
    assert!(
        !out.contains("✓ ENCODED"),
        "must not double-render fresh badge on dedup: {out}"
    );

    // No "LSN N" — there's no fresh LSN to chain off.
    assert!(
        !out.contains("LSN 0"),
        "must not show LSN 0 for dedup hit: {out}"
    );

    // The content still echoes so the user sees what matched.
    assert!(
        out.contains("Alice merged the auth-rewrite branch"),
        "source text not echoed on dedup hit: {out}"
    );

    // Explicit "no fresh write" footer — the user expects to know
    // whether their op actually wrote anything.
    assert!(
        out.contains("no fresh write"),
        "missing 'no fresh write' footer: {out}"
    );

    // No "subscribe" hint — there's no new LSN to subscribe at.
    assert!(
        !out.contains("subscribe --start-lsn"),
        "must not show subscribe hint on dedup hit: {out}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 3 — wide mode surfaces the stub-embedder honesty signal
// ---------------------------------------------------------------------------

#[test]
fn wide_mode_surfaces_stub_embedder_warning_when_fingerprint_is_zeros() {
    let resp = sample_response();
    let item = EncodeRendered::new(resp).with_source("Alice merged the auth-rewrite branch");
    let out = render(item, OutputFormat::Wide);

    // Wide adds the agent / embedder / edges block.
    assert!(out.contains("agent"), "wide must surface agent row: {out}");
    assert!(
        out.contains("embedder"),
        "wide must surface embedder row: {out}"
    );

    // The honesty signal: server today uses NopDispatcher, so the
    // embedder fingerprint is [0; 16]. The renderer must say so
    // explicitly rather than pretending it's a real fingerprint.
    // When 9.10 wires the real CpuDispatcher this row flips to
    // "fp <short hex>" and this test will need to be updated.
    assert!(
        out.contains("stub")
            || out.contains("NopDispatcher")
            || out.contains("semantic search inactive"),
        "wide must call out the stub embedder honestly: {out}"
    );

    // Default mode is still in the output — wide ADDS, doesn't replace.
    assert!(
        out.contains("✓ ENCODED"),
        "wide must still show the heading: {out}"
    );
    assert!(
        out.contains("Alice merged the auth-rewrite branch"),
        "wide must still echo source text: {out}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 4 — JSON view stays raw (no sentinel translation)
// ---------------------------------------------------------------------------

#[test]
fn json_view_emits_raw_zero_lsn_and_raw_was_deduplicated() {
    let resp = EncodeResponse {
        was_deduplicated: true,
        lsn: 0,
        ..sample_response()
    };
    let item = EncodeRendered::new(resp);
    let out = render(item, OutputFormat::Json);

    let value: serde_json::Value = serde_json::from_str(&out).expect("json output must parse");

    // The JSON view is the machine-readable surface. The 0 sentinel
    // is a table-view concern; consumers parse the raw u64 and apply
    // the convention themselves (per F2's spec doc at 688e691).
    assert_eq!(
        value["lsn"], 0,
        "json must emit literal 0 for lsn (sentinel is a table-view concern)"
    );

    assert_eq!(
        value["was_deduplicated"], true,
        "json must preserve was_deduplicated boolean"
    );
}

// ---------------------------------------------------------------------------
// Scenario 5 — dispatch + Auto format routes to ndjson on non-tty
// ---------------------------------------------------------------------------

#[test]
fn auto_format_on_non_tty_routes_to_ndjson_one_record_per_line() {
    let resp = sample_response();
    let item = EncodeRendered::new(resp);
    // TermPolicy::plain() has stdout_is_tty = false → Auto picks ndjson.
    let out = render(item, OutputFormat::Auto);

    // Ndjson: a single JSON object per line. EncodeResponse is one
    // record so we expect exactly one line ending in a newline.
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 1, "expected single ndjson record: {out:?}");

    let value: serde_json::Value = serde_json::from_str(lines[0]).expect("ndjson line must parse");
    assert!(value.is_object(), "expected JSON object: {value}");
    assert_eq!(value["lsn"], 1);
}
