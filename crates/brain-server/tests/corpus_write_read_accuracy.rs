//! Live full-pipeline corpus write→read — the end-to-end accuracy gate.
//!
//! Boots a REAL single-shard server (real BGE embedder + pattern + GLiNER
//! classifier + gpt-4o-mini LLM extractor tiers, **rerank OFF** on purpose:
//! grounded memory must be accurate without a cross-encoder), ENCODEs 10
//! deliberately-hard memories over the wire, waits for the async extraction
//! workers to drain, then — over the SAME agent connection — issues a battery
//! of RECALL reads and grades them against an accuracy-first invariant:
//!
//! Recall runs behind a smart router: it retrieves over the unified path
//! (semantic + lexical + graph → RRF → rerank), and when a relation is
//! named exactly in the cue it narrows to the precise source memories.
//! The router returns ONLY memories, tagged with the shape it chose:
//! `Single` (one memory), `Many` (a set), or `None` (nothing). There is
//! no client recall "mode" and no retrieval lane ("episodic", "grounded")
//! exposed on the wire — those are internal mechanics.
//!
//!   * PRECISION is the hard gate (must be 100%): when the router commits
//!     to a `Single` memory for a precise cue, that memory MUST contain the
//!     expected value — a `Single` that doesn't is the router confidently
//!     pointing at the wrong memory, the one failure a memory DB must never
//!     make.
//!   * RECALL is a floor: of the 11 precise cases, >=9 must surface the
//!     expected value somewhere in the returned memory texts (<=2
//!     LLM-variance misses tolerated, all precision-safe).
//!   * Open/associative cues must return memories (the router never goes
//!     `None` when the corpus holds relevant memories).
//!
//! Honest-abstention precision ("the system says I don't know when it
//! genuinely cannot answer") is a synthesis-layer property graded by
//! brain-eval's committed_precision metric — it is not observable at this
//! raw-router gate, which boots the server without an answer synthesizer.
//!
//! The data dir is left on disk for inspection (redb tables, HNSW, tantivy,
//! WAL, arena, config).
//!
//! The corpus is chosen to exercise every write axis at once:
//!   - all six statement kinds (Fact / Preference / Event / Attribute /
//!     Relation / Directive),
//!   - object axis: entity-object minting vs literal value (#1),
//!   - entity-subject Event time vs Event-without-time → Fact downgrade (#2),
//!   - subject shapes: typed entity, coined possessive, source-memory
//!     (first-person / directive), and a non-referential pronoun (drop-count),
//!   - i18n: accented Latin, CJK, emoji, NFC composed/decomposed,
//!   - temporal forms: ISO date, month+year, bare year, relative phrase,
//!   - length: one-clause directives through a multi-clause biography.
//!
//! Gated: runs only when `BRAIN__LLM__API_KEY` + `BRAIN_EMBED_MODEL_DIR` are set
//! (CI skips). The GLiNER model is auto-discovered from the XDG model dir.
//! Run inside the devcontainer:
//!
//! ```text
//! set -a; . /root/.brain-secrets.env; set +a
//! BRAIN_EMBED_MODEL_DIR=/root/.local/share/brain/models/bge-small-en-v1.5 \
//! BRAIN_CORPUS_DATA_DIR=/tmp/brain-corpus-read \
//! cargo test -p brain-server --test corpus_write_read_accuracy -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use brain_embed::Dispatcher;
use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::{EncodeRequest, RecallRequest, RequestBody};
use brain_protocol::envelope::response::{AnswerKindWire, RecallResponseFrame, ResponseBody};
use brain_protocol::Frame;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[allow(dead_code)]
#[path = "../src/admin/mod.rs"]
mod admin;
#[allow(dead_code)]
#[path = "../src/network/auth.rs"]
mod auth;
#[allow(dead_code)]
#[path = "../src/config/mod.rs"]
mod config;
#[allow(dead_code)]
#[path = "../src/network/connection.rs"]
mod connection;
#[path = "../src/network/dispatch.rs"]
mod dispatch;
#[path = "../src/metrics/mod.rs"]
mod metrics;
#[allow(dead_code)]
#[path = "../src/network/routing.rs"]
mod routing;
#[allow(dead_code)]
#[path = "../src/shard/mod.rs"]
mod shard;
#[path = "../src/network/subscribe.rs"]
mod subscribe;
#[allow(dead_code)]
#[path = "../src/bootstrap/tls.rs"]
mod tls;

mod support_harness;

use support_harness::start_full_pipeline_in;

const FLAG_EOS: u8 = 1 << 7;

/// The 10 hard memories — see the module doc for the axes each exercises.
const CORPUS: &[&str] = &[
    // 1. Accented person; founding Event (month+year); CEO relation/role;
    //    location; org as an entity-object.
    "Dr. Elena Fernández founded NeuraCorp in Berlin in March 2019 and is its chief executive.",
    // 2. Medical-domain Facts; disease as an entity-object.
    "Metformin lowers blood glucose and is the first-line treatment for type 2 diabetes.",
    // 3. Coined possessive subject ("Maria's daughter"); Preference (loves)
    //    + Fact (vegetarian).
    "Maria's daughter is vegetarian and loves classic science-fiction films.",
    // 4. Source-memory subject; two Directives; negation.
    "Always reply to me concisely and never use emojis.",
    // 5. CJK subject; accented place; Event with an ISO date; attend relation.
    "李明 visited São Paulo on 2023-07-12 to attend the Web Summit.",
    // 6. Long multi-clause biography: several Events (bare years), attributes
    //    (languages, nationality), an employer relation, location.
    "Aisha Okonkwo, a Nigerian-born astrophysicist, earned her doctorate at MIT in 2015, \
     joined the European Southern Observatory in Chile two years later, speaks Yoruba, \
     English and Spanish fluently, and was awarded the Breakthrough Prize in 2022.",
    // 7. Numeric/literal value-objects (height, weight) must stay Values, not
    //    minted entities; completion Event (bare year).
    "The Eiffel Tower stands 330 meters tall, weighs 10,100 tonnes, and was completed in 1889.",
    // 8. First-person source-memory subject; Preferences (prefer/dislike) +
    //    health Fact (allergy); negation.
    "I prefer dark-roast coffee, I dislike crowded restaurants, and I am allergic to peanuts.",
    // 9. Relation-heavy geography: capital-of + rail-link between entities.
    "Tokyo is the capital of Japan and is linked to Osaka by the Shinkansen high-speed railway.",
    // 10. Edge case: non-referential pronoun subject (drop-count path); relative
    //     temporal ("last spring"); emoji robustness; accented place; a number.
    "She relocated to Zürich last spring and now leads a team of 12 engineers at a fintech startup 🚀.",
];

// ---------------------------------------------------------------------------
// Wire helpers (copied from tests/e2e.rs).
// ---------------------------------------------------------------------------

async fn read_one_frame<S>(stream: &mut S) -> Result<Frame, String>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut header = [0u8; brain_protocol::HEADER_SIZE];
    stream
        .read_exact(&mut header)
        .await
        .map_err(|e| format!("header read: {e}"))?;
    let payload_len = u32::from_be_bytes([0, header[16], header[17], header[18]]) as usize;
    let mut buf = Vec::with_capacity(brain_protocol::HEADER_SIZE + payload_len);
    buf.extend_from_slice(&header);
    if payload_len > 0 {
        buf.resize(brain_protocol::HEADER_SIZE + payload_len, 0);
        stream
            .read_exact(&mut buf[brain_protocol::HEADER_SIZE..])
            .await
            .map_err(|e| format!("payload read: {e}"))?;
    }
    let (frame, rest) = Frame::decode_with_max(&buf, brain_protocol::MAX_PAYLOAD_BYTES as u32)
        .map_err(|e| format!("decode: {e}"))?;
    debug_assert!(rest.is_empty());
    Ok(frame)
}

async fn send_frame(client: &mut TcpStream, frame: Frame) {
    client.write_all(&frame.encode()).await.expect("send");
    client.flush().await.expect("flush");
}

async fn complete_handshake(client: &mut TcpStream, agent_id: [u8; 16]) {
    let hello = HelloPayload {
        client_id: "corpus-e2e".into(),
        supported_versions: vec![brain_protocol::VERSION],
        capabilities: HelloCapabilities {
            streaming: true,
            compression_zstd: false,
            server_push: false,
        },
        client_session_token: None,
    };
    send_frame(
        client,
        Frame::new(
            Opcode::Hello.as_u16(),
            FLAG_EOS,
            0,
            RequestBody::Hello(hello).encode(),
        ),
    )
    .await;
    let welcome = read_one_frame(client).await.expect("WELCOME");
    assert_eq!(welcome.header.opcode_u16(), Opcode::Welcome.as_u16());

    let auth = AuthPayload {
        method: AuthMethod::None,
        agent_id,
        credentials: AuthCredentials::None,
    };
    send_frame(
        client,
        Frame::new(
            Opcode::Auth.as_u16(),
            FLAG_EOS,
            0,
            RequestBody::Auth(auth).encode(),
        ),
    )
    .await;
    let auth_ok = read_one_frame(client).await.expect("AUTH_OK");
    assert_eq!(auth_ok.header.opcode_u16(), Opcode::AuthOk.as_u16());
}

async fn encode_round_trip(client: &mut TcpStream, stream_id: u32, text: &str) -> (u16, Option<u128>) {
    let req = EncodeRequest {
        text: text.into(),
        context_id: 0,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        occurred_at_unix_nanos: None,
    };
    send_frame(
        client,
        Frame::new(
            Opcode::EncodeReq.as_u16(),
            FLAG_EOS,
            stream_id,
            RequestBody::Encode(req).encode(),
        ),
    )
    .await;
    let resp = read_one_frame(client).await.expect("ENCODE response");
    let opcode = resp.header.opcode_u16();
    let memory_id = if opcode == Opcode::EncodeResp.as_u16() {
        match ResponseBody::decode(Opcode::EncodeResp, &resp.payload).ok() {
            Some(ResponseBody::Encode(r)) => Some(r.memory_id),
            _ => None,
        }
    } else {
        None
    };
    (opcode, memory_id)
}

// ---------------------------------------------------------------------------
// Read-phase wire helper: RECALL.
// ---------------------------------------------------------------------------

/// Issue one RECALL over the wire and decode the (single, final) response
/// frame. The router returns one unary frame; this asserts the response is
/// a `RECALL_RESP` and returns the decoded frame.
async fn recall(client: &mut TcpStream, stream_id: u32, cue: &str) -> RecallResponseFrame {
    let req = RecallRequest {
        cue_text: cue.into(),
        // Empty subject: Brain resolves subject + relation from the cue
        // alone (the read-path mandate). First-person cues bind to the
        // caller's agent self-entity — which is why the read MUST run on
        // the same agent the corpus was written under.
        subject_name: String::new(),
        max_results: 10,
        confidence_threshold: 0.0,
        context_filter: None,
        age_bound_unix_nanos: None,
        as_of_record_time_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        include_edges: false,
        include_graph: false,
        include_text: true,
        request_id: Some(*uuid::Uuid::now_v7().as_bytes()),
        txn_id: None,
        // Empty + include_other_agents=false ⇒ server scopes to the
        // calling connection's agent (the [7;16] write agent).
        agent_filter: Vec::new(),
        include_other_agents: false,
    };
    send_frame(
        client,
        Frame::new(
            Opcode::RecallReq.as_u16(),
            FLAG_EOS,
            stream_id,
            RequestBody::Recall(req).encode(),
        ),
    )
    .await;
    let resp = read_one_frame(client).await.expect("RECALL response");
    assert_eq!(
        resp.header.opcode_u16(),
        Opcode::RecallResp.as_u16(),
        "expected RECALL_RESP for cue {cue:?}"
    );
    match ResponseBody::decode(Opcode::RecallResp, &resp.payload).expect("decode RECALL_RESP") {
        ResponseBody::Recall(r) => r,
        other => panic!("expected Recall body, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Real embedder + admin-metrics polling.
// ---------------------------------------------------------------------------

fn build_real_dispatcher(model_dir: PathBuf) -> Arc<dyn Dispatcher> {
    let cfg = brain_embed::EmbedderConfig::new(model_dir);
    let handle = brain_embed::ModelHandle::load(&cfg).expect("load BGE embed model");
    let cpu = brain_embed::CpuDispatcher::new(handle);
    Arc::new(brain_embed::CachingDispatcher::new(cpu, 4096))
}

/// Sum of `brain_extractor_items_written_total` across kinds, scraped from the
/// admin `/metrics` endpoint. `None` on a scrape error.
async fn scrape_items_written(admin_addr: std::net::SocketAddr) -> Option<u64> {
    let mut s = TcpStream::connect(admin_addr).await.ok()?;
    s.write_all(b"GET /metrics HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .await
        .ok()?;
    let mut body = String::new();
    s.read_to_string(&mut body).await.ok()?;
    let mut total = 0u64;
    for line in body.lines() {
        if line.starts_with("brain_extractor_items_written_total{") {
            if let Some(n) = line.rsplit(' ').next().and_then(|v| v.parse::<u64>().ok()) {
                total += n;
            }
        }
    }
    Some(total)
}

async fn scrape_lines(admin_addr: std::net::SocketAddr, prefix: &str) -> Vec<String> {
    let Ok(mut s) = TcpStream::connect(admin_addr).await else {
        return Vec::new();
    };
    if s
        .write_all(b"GET /metrics HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .await
        .is_err()
    {
        return Vec::new();
    }
    let mut body = String::new();
    let _ = s.read_to_string(&mut body).await;
    body.lines()
        .filter(|l| l.starts_with(prefix) && !l.ends_with(" 0"))
        .map(|l| l.to_string())
        .collect()
}

#[tokio::test]
#[ignore = "live: requires BRAIN__LLM__API_KEY + BRAIN_EMBED_MODEL_DIR; writes a real corpus to BRAIN_CORPUS_DATA_DIR then reads it back"]
async fn corpus_write_then_read_is_accurate() {
    let Ok(openai_key) = std::env::var("BRAIN__LLM__API_KEY") else {
        eprintln!("skip: BRAIN__LLM__API_KEY unset");
        return;
    };
    let Ok(model_dir) = std::env::var("BRAIN_EMBED_MODEL_DIR") else {
        eprintln!("skip: BRAIN_EMBED_MODEL_DIR unset");
        return;
    };
    let data_dir = PathBuf::from(
        std::env::var("BRAIN_CORPUS_DATA_DIR").unwrap_or_else(|_| "/tmp/brain-corpus-data".into()),
    );
    // Fresh data dir so the inspection reflects exactly this run.
    let _ = std::fs::remove_dir_all(&data_dir);
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    println!("== booting real-tier server (BGE + GLiNER + gpt-4o-mini) ==");
    let dispatcher = build_real_dispatcher(PathBuf::from(model_dir));
    let server = start_full_pipeline_in(&data_dir, dispatcher, Some(openai_key)).await;
    println!("   data_plane={}  admin={}", server.data_plane_addr, server.admin_addr);
    println!("   data_dir={}", data_dir.display());

    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client, [7u8; 16]).await;

    println!("== ENCODE {} memories ==", CORPUS.len());
    let mut ids = Vec::new();
    for (i, text) in CORPUS.iter().enumerate() {
        let stream_id = (i as u32) * 2 + 1; // odd, non-zero
        let (op, mem) = encode_round_trip(&mut client, stream_id, text).await;
        assert_eq!(op, Opcode::EncodeResp.as_u16(), "m{} ENCODE must succeed: {text}", i + 1);
        let id = mem.expect("memory_id");
        let preview: String = text.chars().take(60).collect();
        println!("   m{} id={:032x}  {}", i + 1, id, preview);
        ids.push(id);
    }
    assert_eq!(ids.len(), CORPUS.len());

    // Wait for the async extraction workers to drain: poll items_written until
    // it is > 0 and unchanged across two consecutive polls (or time out).
    println!("== waiting for extraction to drain ==");
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut last = 0u64;
    let mut stable = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(4)).await;
        let cur = scrape_items_written(server.admin_addr).await.unwrap_or(0);
        println!("   items_written_total = {cur}");
        if cur > 0 && cur == last {
            stable += 1;
            if stable >= 2 {
                break;
            }
        } else {
            stable = 0;
        }
        last = cur;
        if Instant::now() >= deadline {
            println!("   (deadline reached)");
            break;
        }
    }

    println!("== extractor metrics ==");
    for l in scrape_lines(server.admin_addr, "brain_extractor_items_written_total").await {
        println!("   {l}");
    }
    let dropped = scrape_lines(server.admin_addr, "brain_extractor_apply_dropped_total").await;
    if dropped.is_empty() {
        println!("   apply_dropped_total: (none)");
    } else {
        for l in dropped {
            println!("   {l}");
        }
    }

    let total = scrape_items_written(server.admin_addr).await.unwrap_or(0);
    assert!(total > 0, "extraction produced no typed-graph items");

    // -----------------------------------------------------------------
    // READ PHASE — same agent connection (writes are owned by [7;16];
    // first-person recall resolves the caller's agent self-entity, so the
    // read MUST stay on this connection / agent).
    // -----------------------------------------------------------------
    read_phase(&mut client).await;

    println!("== done. data dir preserved at {} ==", data_dir.display());
    server.stop().await;
}

/// One read case: the cue and the grading rule. There is no recall mode —
/// the server runs one unified path behind a smart router that returns
/// memories tagged `Single` / `Many` / `None`.
struct ReadCase {
    cue: &'static str,
    grade: Grade,
}

/// What a case asserts under the memory-centric router model.
enum Grade {
    /// PRECISE: the expected value must appear in the returned memory texts.
    /// PRECISION rule: when the router commits to `Single` (one memory as
    /// THE answer) for this cue, that memory MUST contain the expected
    /// value — a `Single` that does not is the router confidently pointing
    /// at the wrong memory (hard fail). A `Many` result is a set of
    /// candidate memories, graded on recall only.
    Precise { expect: &'static str },
    /// OPEN: the router must return memories (not `None`).
    Open,
}

async fn read_phase(client: &mut TcpStream) {
    let cases: &[ReadCase] = &[
        // ---- PRECISE — cases 1..=11 ----
        ReadCase { cue: "what am I allergic to", grade: Grade::Precise { expect: "peanuts" } },
        ReadCase { cue: "what kind of coffee do I prefer", grade: Grade::Precise { expect: "dark" } },
        ReadCase { cue: "what do I dislike", grade: Grade::Precise { expect: "crowded restaurants" } },
        // Memory #4 carries TWO directives ("reply concisely" + "never use
        // emojis"). Either is a real, correct directive (never a
        // hallucination), so this case accepts whichever surfaces. See
        // DIRECTIVE_ALTS.
        ReadCase { cue: "what is my directive for replying", grade: Grade::Precise { expect: "concise" } },
        ReadCase { cue: "what is Elena Fernández's position", grade: Grade::Precise { expect: "chief executive" } },
        ReadCase { cue: "how tall is the Eiffel Tower", grade: Grade::Precise { expect: "330" } },
        ReadCase { cue: "when was the Eiffel Tower completed", grade: Grade::Precise { expect: "1889" } },
        ReadCase { cue: "where does Elena Fernández work", grade: Grade::Precise { expect: "NeuraCorp" } },
        ReadCase { cue: "what is Tokyo the capital of", grade: Grade::Precise { expect: "Japan" } },
        ReadCase { cue: "what did 李明 visit", grade: Grade::Precise { expect: "Paulo" } },
        ReadCase { cue: "what languages does Aisha Okonkwo speak", grade: Grade::Precise { expect: "Yoruba" } },
        // ---- OPEN / ASSOCIATIVE — cases 12..=13 ----
        ReadCase { cue: "diabetes treatment", grade: Grade::Open },
        ReadCase { cue: "the Web Summit trip", grade: Grade::Open },
    ];

    // Case #11 ("languages") is satisfied by ANY of these — the question
    // matches a Set; we accept the first member present.
    const LANGUAGE_ALTS: &[&str] = &["Yoruba", "English", "Spanish"];
    // Case #4 ("directive for replying") — memory #4 stores two genuine
    // directives; either is a correct (non-fabricated) answer.
    const DIRECTIVE_ALTS: &[&str] = &["concise", "emoji"];

    println!("\n== READ PHASE: {} cases ==", cases.len());
    println!(
        "{:<3} {:<10} {:<28} {:>8}  result",
        "#", "answer", "top value", "lat_ms"
    );

    let mut precision_violations = 0usize; // committed Single pointing at the wrong memory
    let mut recall_hits = 0usize; // precise cases whose value appeared in the memories
    let mut precise_total = 0usize;
    let mut open_failures = 0usize;
    let mut precise_latencies_ms: Vec<f64> = Vec::new();

    // Stream ids: odd, non-zero, monotonically increasing.
    let mut sid: u32 = 1;

    for (idx, case) in cases.iter().enumerate() {
        let n = idx + 1;
        let recall_sid = sid;
        sid += 2;

        let started = Instant::now();
        let frame = recall(client, recall_sid, case.cue).await;
        let latency_ms = started.elapsed().as_secs_f64() * 1000.0;

        // The router returns ONLY memories. `Single` means it committed to
        // one memory as THE answer; `Many` is a candidate set; `None` is an
        // empty result.
        let committed_single = matches!(frame.answer_kind, AnswerKindWire::Single);
        let memory_texts: Vec<String> = frame.memories.iter().map(|m| m.text.clone()).collect();

        let answer = format!("{:?}", frame.answer_kind);
        let top: String = memory_texts.first().cloned().unwrap_or_default();
        let top_disp: String = top.chars().take(26).collect();

        let result = match &case.grade {
            Grade::Precise { expect } => {
                precise_total += 1;
                precise_latencies_ms.push(latency_ms);
                let needles: Vec<String> = match n {
                    4 => DIRECTIVE_ALTS.iter().map(|s| s.to_lowercase()).collect(),
                    11 => LANGUAGE_ALTS.iter().map(|s| s.to_lowercase()).collect(),
                    _ => vec![expect.to_lowercase()],
                };
                let in_memories = memory_texts
                    .iter()
                    .any(|r| needles.iter().any(|nd| r.to_lowercase().contains(nd)));
                if committed_single && !in_memories {
                    // The router committed to ONE memory as the answer, and
                    // it does not contain the expected value → confidently
                    // pointing at the wrong memory: the one failure a memory
                    // DB must never make.
                    precision_violations += 1;
                    "FAIL(confident-wrong)"
                } else if in_memories {
                    recall_hits += 1;
                    "PASS"
                } else {
                    // Value not among the returned memories — a RECALL miss,
                    // precision-safe (the router did not over-commit).
                    "MISS(no-recall)"
                }
            }
            Grade::Open => {
                if frame.memories.is_empty() {
                    open_failures += 1;
                    "FAIL(open)"
                } else {
                    "PASS(memories)"
                }
            }
        };

        println!(
            "{:<3} {:<10} {:<28} {:>8.2}  {}",
            n, answer, top_disp, latency_ms, result
        );
    }

    // Summary. Precision is graded over the precise cases (the
    // accuracy-critical set); open cases don't bear on it.
    let precision_pct = if precise_total == 0 {
        100.0
    } else {
        100.0 * (precise_total - precision_violations) as f64 / precise_total as f64
    };
    let p50 = percentile(&precise_latencies_ms, 0.50);
    let p99 = percentile(&precise_latencies_ms, 0.99);
    println!("\n== SUMMARY ==");
    println!(
        "precision: {precision_pct:.1}%  ({precision_violations} confident-wrong Single answers across {precise_total} precise cases)"
    );
    println!("recall:    {recall_hits}/{precise_total} precise cases surfaced the expected value");
    println!("open:      {} associative cases, {open_failures} failures", 2);
    println!("latency (precise reads): p50={p50:.2}ms  p99={p99:.2}ms");

    // ---- THE GATE ----
    // Precision is the hard invariant: when the router commits to a single
    // memory, it is NEVER the wrong one.
    assert_eq!(
        precision_violations, 0,
        "PRECISION violated: {precision_violations} confident-wrong Single answers (must be 0)"
    );
    assert_eq!(open_failures, 0, "open/associative cues must return memories");
    assert!(
        recall_hits >= 9,
        "RECALL floor: only {recall_hits}/{precise_total} precise cases surfaced the expected value (need >=9)"
    );
}

/// Nearest-rank percentile over a copy-sorted sample. Empty ⇒ 0.0.
fn percentile(samples: &[f64], q: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut s = samples.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = (q * (s.len() as f64 - 1.0)).round() as usize;
    s[rank.min(s.len() - 1)]
}
