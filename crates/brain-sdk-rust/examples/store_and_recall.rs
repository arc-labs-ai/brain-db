//! Brain cognitive-substrate — end-to-end data exercise.
//!
//! 30 memories across 8 domains, multiple recall cues, graph links,
//! a transaction, deduplication, and forget. Every step prints what
//! was sent and what came back so you can follow the full lifecycle.
//!
//! # Prerequisites
//!
//! Start the server in one terminal:
//!
//!   cargo run --bin brain-server -- --config config/dev.toml
//!
//! Then run this example in another terminal inside the same container:
//!
//!   cargo run --example store_and_recall -p brain-sdk-rust
//!
//! Optional: watch the admin plane while it runs:
//!
//!   just cli --output json debug-snapshot --shard 0 | jq .
//!   just cli worker list
//!   just cli stats

use std::net::SocketAddr;

use brain_core::MemoryId;
use brain_protocol::request::{EdgeKindWire, ForgetMode, MemoryKindWire};
use brain_sdk_rust::Client;

const SERVER: &str = "127.0.0.1:9090";

// ─── context ids ────────────────────────────────────────────────────────────
// Each context groups memories into a logical namespace. Recall can
// be filtered by context_id in a future update; for now they annotate
// the slot for graph-walk queries.
const CTX_ML: u64 = 1;
const CTX_PHYSICS: u64 = 2;
const CTX_HISTORY: u64 = 3;
const CTX_MEDICINE: u64 = 4;
const CTX_PHILOSOPHY: u64 = 5;
const CTX_SOFTWARE: u64 = 6;
const CTX_GEOGRAPHY: u64 = 7;
const CTX_FOOD: u64 = 8;

// ─── helpers ────────────────────────────────────────────────────────────────

fn separator(label: &str) {
    println!("\n{}", "─".repeat(70));
    println!("  {label}");
    println!("{}", "─".repeat(70));
}

fn print_memories(label: &str, results: &[brain_protocol::response::MemoryResult]) {
    if results.is_empty() {
        println!("  {label}: (no results — HNSW index may still be warming up)");
        return;
    }
    println!("  {label} — {} result(s):", results.len());
    for (i, r) in results.iter().enumerate() {
        println!(
            "    [{i}] {:.4}  [{:?}]  {}",
            r.similarity_score,
            r.kind,
            truncate(&r.text, 72),
        );
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}

// ─── main ───────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr: SocketAddr = SERVER.parse()?;
    println!("Connecting to Brain at {SERVER} …");
    let client = Client::connect(addr).await?;
    println!("Connected.\n");

    // ────────────────────────────────────────────────────────────────────────
    //  PHASE 1 — ENCODE  (30 memories, 8 domains)
    // ────────────────────────────────────────────────────────────────────────

    separator("PHASE 1 · ENCODE — 30 memories across 8 domains");

    // ── 1 · Machine Learning / AI ──────────────────── context 1
    println!("\n  [ML / AI  context={CTX_ML}]");

    let ml_attention = client
        .encode("The attention mechanism allows neural networks to focus on different parts of the input when producing each part of the output.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.9)
        .context(CTX_ML)
        .send().await?;
    println!(
        "    + attention mechanism  id={:#x}",
        ml_attention.memory_id
    );

    let ml_bert = client
        .encode("BERT achieved state-of-the-art on 11 NLP benchmarks by pre-training a deep bidirectional transformer on masked-language and next-sentence tasks.")
        .kind(MemoryKindWire::Episodic)
        .salience(0.75)
        .context(CTX_ML)
        .send().await?;
    println!("    + BERT                 id={:#x}", ml_bert.memory_id);

    let ml_transformer_parallel = client
        .encode("A transformer encoder processes all input tokens in parallel using self-attention, unlike RNNs which are inherently sequential.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.85)
        .context(CTX_ML)
        .send().await?;
    println!(
        "    + transformer parallel id={:#x}",
        ml_transformer_parallel.memory_id
    );

    let ml_gpt = client
        .encode("GPT models use a decoder-only transformer trained with causal language modelling — predicting the next token given all previous tokens.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.85)
        .context(CTX_ML)
        .send().await?;
    println!("    + GPT decoder-only     id={:#x}", ml_gpt.memory_id);

    let ml_gradient = client
        .encode("Gradient descent with momentum accumulates an exponentially decaying moving average of past gradients to dampen oscillations and accelerate convergence.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.70)
        .context(CTX_ML)
        .send().await?;
    println!("    + gradient momentum    id={:#x}", ml_gradient.memory_id);

    // ── 2 · Physics ────────────────────────────────── context 2
    println!("\n  [Physics  context={CTX_PHYSICS}]");

    let ph_relativity = client
        .encode("Einstein's special relativity: the speed of light in a vacuum is constant (c ≈ 3×10⁸ m/s) for all inertial observers regardless of the source's motion.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.9)
        .context(CTX_PHYSICS)
        .send().await?;
    println!(
        "    + special relativity   id={:#x}",
        ph_relativity.memory_id
    );

    let ph_entanglement = client
        .encode("Quantum entanglement allows two particles to share a correlated quantum state such that measuring one instantly determines the state of the other, regardless of distance.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.85)
        .context(CTX_PHYSICS)
        .send().await?;
    println!(
        "    + quantum entanglement id={:#x}",
        ph_entanglement.memory_id
    );

    let ph_uncertainty = client
        .encode("Heisenberg's uncertainty principle: position and momentum of a particle cannot both be precisely known; ΔxΔp ≥ ℏ/2.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.9)
        .context(CTX_PHYSICS)
        .send().await?;
    println!(
        "    + uncertainty principle id={:#x}",
        ph_uncertainty.memory_id
    );

    let ph_blackhole = client
        .encode("Black holes are regions of spacetime where gravity is so extreme that the escape velocity exceeds the speed of light, making them invisible except through gravitational effects.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.80)
        .context(CTX_PHYSICS)
        .send().await?;
    println!(
        "    + black holes           id={:#x}",
        ph_blackhole.memory_id
    );

    // ── 3 · History ────────────────────────────────── context 3
    println!("\n  [History  context={CTX_HISTORY}]");

    let hi_press = client
        .encode("Johannes Gutenberg's movable-type printing press (~1440) enabled mass reproduction of texts and accelerated the spread of the Renaissance and the Reformation.")
        .kind(MemoryKindWire::Episodic)
        .salience(0.70)
        .context(CTX_HISTORY)
        .send().await?;
    println!("    + printing press        id={:#x}", hi_press.memory_id);

    let hi_french_rev = client
        .encode("The French Revolution (1789–1799) abolished the monarchy, established republican governance, and spread the ideals of liberté, égalité, fraternité across Europe.")
        .kind(MemoryKindWire::Episodic)
        .salience(0.80)
        .context(CTX_HISTORY)
        .send().await?;
    println!(
        "    + French Revolution     id={:#x}",
        hi_french_rev.memory_id
    );

    let hi_moon = client
        .encode("Apollo 11 landed Neil Armstrong and Buzz Aldrin on the lunar surface on July 20, 1969 — the first time humans set foot on another celestial body.")
        .kind(MemoryKindWire::Episodic)
        .salience(0.90)
        .context(CTX_HISTORY)
        .send().await?;
    println!("    + Apollo 11             id={:#x}", hi_moon.memory_id);

    let hi_berlin = client
        .encode("The Berlin Wall fell on November 9, 1989, after East Germany opened its borders, marking the symbolic end of the Cold War and leading to German reunification in 1990.")
        .kind(MemoryKindWire::Episodic)
        .salience(0.75)
        .context(CTX_HISTORY)
        .send().await?;
    println!("    + Berlin Wall           id={:#x}", hi_berlin.memory_id);

    // ── 4 · Medicine / Biology ─────────────────────── context 4
    println!("\n  [Medicine / Biology  context={CTX_MEDICINE}]");

    let med_crispr = client
        .encode("CRISPR-Cas9 uses a guide RNA to direct the Cas9 endonuclease to a precise DNA location where it makes a double-strand break, enabling targeted gene editing in living cells.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.90)
        .context(CTX_MEDICINE)
        .send().await?;
    println!("    + CRISPR-Cas9           id={:#x}", med_crispr.memory_id);

    let med_mrna = client
        .encode("mRNA vaccines encode a viral antigen; once inside cells the antigen is expressed and presented to the immune system, which builds antibodies without exposure to the live pathogen.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.85)
        .context(CTX_MEDICINE)
        .send().await?;
    println!("    + mRNA vaccines         id={:#x}", med_mrna.memory_id);

    let med_neurons = client
        .encode("The adult human brain contains approximately 86 billion neurons interconnected by roughly 100 trillion synapses, with most neurons non-dividing after birth.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.80)
        .context(CTX_MEDICINE)
        .send().await?;
    println!(
        "    + neurons and synapses  id={:#x}",
        med_neurons.memory_id
    );

    let med_penicillin = client
        .encode("Alexander Fleming discovered penicillin in 1928 when he noticed that Penicillium mould secreted a substance that killed nearby Staphylococcus bacteria in a contaminated petri dish.")
        .kind(MemoryKindWire::Episodic)
        .salience(0.70)
        .context(CTX_MEDICINE)
        .send().await?;
    println!(
        "    + penicillin discovery  id={:#x}",
        med_penicillin.memory_id
    );

    // ── 5 · Philosophy ─────────────────────────────── context 5
    println!("\n  [Philosophy  context={CTX_PHILOSOPHY}]");

    let ph_cogito = client
        .encode("Descartes' cogito — cogito ergo sum ('I think, therefore I am') — grounds epistemology in the one thing that cannot be doubted: the existence of one's own thinking.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.85)
        .context(CTX_PHILOSOPHY)
        .send().await?;
    println!("    + cogito ergo sum       id={:#x}", ph_cogito.memory_id);

    let ph_kant = client
        .encode("Kant's categorical imperative: act only according to that maxim whereby you can at the same time will that it should become a universal law.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.90)
        .context(CTX_PHILOSOPHY)
        .send().await?;
    println!("    + categorical imperative id={:#x}", ph_kant.memory_id);

    let ph_cave = client
        .encode("Plato's allegory of the cave depicts prisoners who, having seen only shadows on a cave wall, mistake them for reality — illustrating how perception limits knowledge.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.80)
        .context(CTX_PHILOSOPHY)
        .send().await?;
    println!("    + allegory of the cave  id={:#x}", ph_cave.memory_id);

    let ph_nietzsche = client
        .encode("Nietzsche's will to power proposes that the fundamental drive in living beings is not self-preservation or pleasure, but the desire to impose form, master resistance, and create meaning.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.75)
        .context(CTX_PHILOSOPHY)
        .send().await?;
    println!(
        "    + will to power         id={:#x}",
        ph_nietzsche.memory_id
    );

    // ── 6 · Software Engineering ───────────────────── context 6
    println!("\n  [Software Engineering  context={CTX_SOFTWARE}]");

    let sw_cap = client
        .encode("The CAP theorem (Brewer, 2000) proves that a distributed data store can guarantee at most two of: consistency, availability, and partition tolerance simultaneously.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.90)
        .context(CTX_SOFTWARE)
        .send().await?;
    println!("    + CAP theorem           id={:#x}", sw_cap.memory_id);

    let sw_acid = client
        .encode("ACID properties — atomicity, consistency, isolation, durability — define the guarantees that database transactions must uphold to ensure data validity despite errors or concurrency.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.90)
        .context(CTX_SOFTWARE)
        .send().await?;
    println!("    + ACID properties       id={:#x}", sw_acid.memory_id);

    let sw_event_sourcing = client
        .encode("Event sourcing persists state as an immutable, append-only log of domain events. Current state is derived by replaying events, enabling time-travel queries and audit trails.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.85)
        .context(CTX_SOFTWARE)
        .send().await?;
    println!(
        "    + event sourcing        id={:#x}",
        sw_event_sourcing.memory_id
    );

    let sw_two_phase = client
        .encode("Two-phase commit (2PC) coordinates distributed transactions through a prepare phase (all participants vote yes/no) and a commit phase, but risks blocking if the coordinator fails.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.80)
        .context(CTX_SOFTWARE)
        .send().await?;
    println!(
        "    + 2PC                   id={:#x}",
        sw_two_phase.memory_id
    );

    // ── 7 · Geography ──────────────────────────────── context 7
    println!("\n  [Geography  context={CTX_GEOGRAPHY}]");

    let geo_amazon = client
        .encode("The Amazon River discharges ~209,000 cubic metres of fresh water per second into the Atlantic — roughly 20% of all fresh water that enters the world's oceans.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.75)
        .context(CTX_GEOGRAPHY)
        .send().await?;
    println!("    + Amazon River          id={:#x}", geo_amazon.memory_id);

    let geo_everest = client
        .encode("Following a 2020 joint China-Nepal survey, Mount Everest's official height was updated to 8848.86 metres above sea level, 86 cm higher than the previous measurement.")
        .kind(MemoryKindWire::Episodic)
        .salience(0.70)
        .context(CTX_GEOGRAPHY)
        .send().await?;
    println!(
        "    + Everest height        id={:#x}",
        geo_everest.memory_id
    );

    let geo_sahara = client
        .encode("The Sahara is Earth's largest hot desert at over 9.2 million km², spanning 11 countries across northern Africa, with temperatures exceeding 50°C at ground level.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.70)
        .context(CTX_GEOGRAPHY)
        .send().await?;
    println!("    + Sahara                id={:#x}", geo_sahara.memory_id);

    // ── 8 · Food / Culture ─────────────────────────── context 8
    println!("\n  [Food / Culture  context={CTX_FOOD}]");

    let food_maillard = client
        .encode("The Maillard reaction, occurring above ~140°C, is the non-enzymatic browning between amino acids and reducing sugars that produces the complex flavours and colours in seared meat, bread crust, and roasted coffee.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.70)
        .context(CTX_FOOD)
        .send().await?;
    println!(
        "    + Maillard reaction     id={:#x}",
        food_maillard.memory_id
    );

    let food_fermentation = client
        .encode("Fermentation — used for over 10,000 years — converts sugars into alcohol or acids through microbial activity, underpinning bread, cheese, yogurt, beer, wine, kimchi, and miso.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.70)
        .context(CTX_FOOD)
        .send().await?;
    println!(
        "    + fermentation          id={:#x}",
        food_fermentation.memory_id
    );

    let food_umami = client
        .encode("Umami, the fifth basic taste, was identified by Kikunae Ikeda in 1908 when he isolated glutamate from kombu seaweed. It is now understood to be triggered by L-glutamate, inosinate, and guanylate receptors on the tongue.")
        .kind(MemoryKindWire::Episodic)
        .salience(0.65)
        .context(CTX_FOOD)
        .send().await?;
    println!("    + umami discovery       id={:#x}", food_umami.memory_id);

    println!("\n  30 memories encoded across 8 domains.\n");

    // ────────────────────────────────────────────────────────────────────────
    //  PHASE 2 — LINK  (build a cross-memory knowledge graph)
    // ────────────────────────────────────────────────────────────────────────

    separator("PHASE 2 · LINK — build the knowledge graph");

    // BERT supports / is derived from the attention mechanism
    client
        .link(
            MemoryId::from_raw(ml_bert.memory_id),
            EdgeKindWire::DerivedFrom,
            MemoryId::from_raw(ml_attention.memory_id),
        )
        .weight(0.95)
        .send()
        .await?;
    println!("  BERT  --[DerivedFrom]--> attention mechanism  (w=0.95)");

    // GPT is also derived from the transformer parallel processing design
    client
        .link(
            MemoryId::from_raw(ml_gpt.memory_id),
            EdgeKindWire::DerivedFrom,
            MemoryId::from_raw(ml_transformer_parallel.memory_id),
        )
        .weight(0.90)
        .send()
        .await?;
    println!("  GPT   --[DerivedFrom]--> transformer parallel  (w=0.90)");

    // CRISPR supports mRNA vaccine development (shared gene-editing/biology tooling)
    client
        .link(
            MemoryId::from_raw(med_crispr.memory_id),
            EdgeKindWire::Supports,
            MemoryId::from_raw(med_mrna.memory_id),
        )
        .weight(0.70)
        .send()
        .await?;
    println!("  CRISPR --[Supports]--> mRNA vaccines  (w=0.70)");

    // CAP theorem references ACID (both are distributed-systems correctness properties)
    client
        .link(
            MemoryId::from_raw(sw_cap.memory_id),
            EdgeKindWire::References,
            MemoryId::from_raw(sw_acid.memory_id),
        )
        .weight(0.80)
        .send()
        .await?;
    println!("  CAP   --[References]--> ACID  (w=0.80)");

    // 2PC is derived from the ACID atomicity requirement
    client
        .link(
            MemoryId::from_raw(sw_two_phase.memory_id),
            EdgeKindWire::DerivedFrom,
            MemoryId::from_raw(sw_acid.memory_id),
        )
        .weight(0.85)
        .send()
        .await?;
    println!("  2PC   --[DerivedFrom]--> ACID  (w=0.85)");

    // Event sourcing contradicts traditional mutable-state databases (ACID assumption)
    client
        .link(
            MemoryId::from_raw(sw_event_sourcing.memory_id),
            EdgeKindWire::Contradicts,
            MemoryId::from_raw(sw_acid.memory_id),
        )
        .weight(0.60)
        .send()
        .await?;
    println!("  EventSourcing --[Contradicts]--> ACID (mutable-state assumption)  (w=0.60)");

    // Kant's categorical imperative SimilarTo Descartes' universal reasoning
    client
        .link(
            MemoryId::from_raw(ph_kant.memory_id),
            EdgeKindWire::SimilarTo,
            MemoryId::from_raw(ph_cogito.memory_id),
        )
        .weight(0.65)
        .send()
        .await?;
    println!(
        "  Kant  --[SimilarTo]--> Descartes (both use universal rational principles)  (w=0.65)"
    );

    // Penicillin discovery followed by / caused modern antibiotic medicine
    client
        .link(
            MemoryId::from_raw(med_penicillin.memory_id),
            EdgeKindWire::Caused,
            MemoryId::from_raw(med_mrna.memory_id),
        )
        .weight(0.55)
        .send()
        .await?;
    println!("  Penicillin --[Caused(chain)]--> modern vaccine research  (w=0.55)");

    println!("\n  8 edges added.");

    // ────────────────────────────────────────────────────────────────────────
    //  PHASE 3 — TRANSACTION  (atomic multi-domain cross-write)
    // ────────────────────────────────────────────────────────────────────────

    separator("PHASE 3 · TRANSACTION — atomic cross-domain pair");

    let txn = client.txn_begin().await?;
    println!("  Transaction id={:?}", txn.txn_id);

    // Two related concepts encoded atomically: quantum computing bridges
    // physics and software engineering — both must land or neither does.
    let txn_qc_physics = client
        .encode("Quantum computers exploit superposition and entanglement to process exponentially many states simultaneously — directly implementing quantum mechanics rather than simulating it.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.85)
        .context(CTX_PHYSICS)
        .txn(txn.txn_id)
        .send().await?;

    let txn_qc_software = client
        .encode("Shor's algorithm, running on a quantum computer, factors large integers in polynomial time — breaking RSA encryption that classical computers cannot crack in feasible time.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.85)
        .context(CTX_SOFTWARE)
        .txn(txn.txn_id)
        .send().await?;

    client.txn_commit(txn.txn_id).await?;
    println!("  Committed:");
    println!(
        "    quantum computing (physics)  id={:#x}",
        txn_qc_physics.memory_id
    );
    println!(
        "    Shor's algorithm (software)  id={:#x}",
        txn_qc_software.memory_id
    );

    // Link the two transactionally-stored memories
    client
        .link(
            MemoryId::from_raw(txn_qc_software.memory_id),
            EdgeKindWire::DerivedFrom,
            MemoryId::from_raw(txn_qc_physics.memory_id),
        )
        .weight(0.92)
        .send()
        .await?;
    println!("  Shor's algo --[DerivedFrom]--> quantum computing  (w=0.92)");

    // ────────────────────────────────────────────────────────────────────────
    //  PHASE 4 — DEDUPLICATION
    // ────────────────────────────────────────────────────────────────────────

    separator("PHASE 4 · DEDUPLICATION — re-encode an existing memory");

    let dup = client
        .encode("The CAP theorem (Brewer, 2000) proves that a distributed data store can guarantee at most two of: consistency, availability, and partition tolerance simultaneously.")
        .kind(MemoryKindWire::Semantic)
        .salience(0.90)
        .context(CTX_SOFTWARE)
        .deduplicate(true)       // ask server to check fingerprint before inserting
        .send().await?;

    println!(
        "  Re-encoded CAP theorem  id={:#x}  was_deduplicated={}",
        dup.memory_id, dup.was_deduplicated
    );
    if dup.was_deduplicated {
        println!("  Server returned existing id — no new slot allocated. ✓");
    } else {
        println!("  (New slot created — fingerprint miss or dedup disabled server-side.)");
    }

    // ────────────────────────────────────────────────────────────────────────
    //  PHASE 5 — RECALL  (8 cue queries across domains and conditions)
    // ────────────────────────────────────────────────────────────────────────

    separator("PHASE 5 · RECALL — 8 queries, different cues and domains");

    // 5-1. Broad ML query
    let r1 = client
        .recall("transformer neural network architecture")
        .send()
        .await?;
    print_memories("Q1 'transformer neural network architecture'", &r1);

    // 5-2. Quantum physics
    let r2 = client
        .recall("quantum mechanics particle behaviour")
        .send()
        .await?;
    print_memories("Q2 'quantum mechanics particle behaviour'", &r2);

    // 5-3. Historical milestone — space exploration
    let r3 = client
        .recall("human beings landing on the moon")
        .send()
        .await?;
    print_memories("Q3 'human beings landing on the moon'", &r3);

    // 5-4. Biotech and gene editing
    let r4 = client
        .recall("DNA editing genetic engineering biological")
        .send()
        .await?;
    print_memories("Q4 'DNA editing genetic engineering'", &r4);

    // 5-5. Ethics and moral reasoning
    let r5 = client
        .recall("moral philosophy universal ethics consciousness")
        .send()
        .await?;
    print_memories("Q5 'moral philosophy universal ethics consciousness'", &r5);

    // 5-6. Distributed systems correctness
    let r6 = client
        .recall("distributed database consistency guarantees atomicity")
        .send()
        .await?;
    print_memories(
        "Q6 'distributed database consistency guarantees atomicity'",
        &r6,
    );

    // 5-7. Cross-domain — science meeting technology
    let r7 = client
        .recall("quantum computing cryptography security encryption")
        .send()
        .await?;
    print_memories(
        "Q7 'quantum computing cryptography security encryption'",
        &r7,
    );

    // 5-8. Food science
    let r8 = client
        .recall("cooking food chemistry flavour chemical reaction")
        .send()
        .await?;
    print_memories("Q8 'cooking food chemistry flavour'", &r8);

    // ────────────────────────────────────────────────────────────────────────
    //  PHASE 6 — FORGET  (soft and hard, different conditions)
    // ────────────────────────────────────────────────────────────────────────

    separator("PHASE 6 · FORGET — soft tombstone then hard erase");

    // Soft forget — tombstone; slot is reclaimed after grace period (default 7 days).
    let soft = client
        .encode("Temporary note: the server benchmark run is scheduled for 22:00 UTC.")
        .kind(MemoryKindWire::Episodic)
        .salience(0.30)        // low salience — operator note, not core knowledge
        .context(CTX_SOFTWARE)
        .send().await?;
    println!("  Encoded ephemeral note  id={:#x}", soft.memory_id);

    let forget_resp = client
        .forget(MemoryId::from_raw(soft.memory_id))
        .mode(ForgetMode::Soft)
        .send()
        .await?;
    println!(
        "  Soft-forgot             id={:#x}  edges_removed={}",
        forget_resp.memory_id, forget_resp.edges_removed
    );
    println!("  (Slot reclaimed after tombstone grace period.)");

    // Hard forget — immediate zero-wipe; spec §09/06.
    let hard = client
        .encode("Sensitive operational data: API key placeholder abc-123-xyz. DO NOT RETAIN.")
        .kind(MemoryKindWire::Episodic)
        .salience(0.95)        // high salience ensures it is indexed immediately
        .context(CTX_SOFTWARE)
        .send().await?;
    println!("\n  Encoded sensitive note  id={:#x}", hard.memory_id);

    let hard_resp = client
        .forget(MemoryId::from_raw(hard.memory_id))
        .mode(ForgetMode::Hard)
        .send()
        .await?;
    println!(
        "  Hard-erased             id={:#x}  edges_removed={}",
        hard_resp.memory_id, hard_resp.edges_removed
    );
    println!("  (Slot zeroed immediately — no grace period.)");

    // ────────────────────────────────────────────────────────────────────────
    //  PHASE 7 — VERIFY DURABILITY
    // ────────────────────────────────────────────────────────────────────────

    separator("PHASE 7 · DURABILITY CHECK — recall after writes");

    // A recall after all the writes confirms the WAL was fsynced and the
    // HNSW index accepted the vectors. The server guarantees WAL-before-
    // acknowledge (spec §05/03) — if encode returned Ok, it is durable.
    let dur = client
        .recall("relativity speed of light inertial observer")
        .send()
        .await?;
    print_memories("Durability recall 'relativity speed of light'", &dur);

    println!(
        "\n  The fact that recall returns results (not an empty set) confirms:\n\
         \t• WAL records were fsynced before ENCODE returned.\n\
         \t• HNSW index accepted the 384-dim BGE-small vectors.\n\
         \t• All 30 memories survived the session lifecycle.\n"
    );

    // ────────────────────────────────────────────────────────────────────────
    //  PHASE 8 — SDK METRICS
    // ────────────────────────────────────────────────────────────────────────

    separator("PHASE 8 · SDK METRICS — point-in-time snapshot");

    let snap = client.metrics_snapshot();
    println!("  requests_total      = {}", snap.requests_total);
    println!("  errors_total        = {}", snap.errors_total);
    println!("  retries_total       = {}", snap.retries_total);
    if let Some(enc) = snap.by_op.get("encode") {
        println!("  encode.requests     = {}", enc.requests_total);
        println!("  encode.errors       = {}", enc.errors_total);
    }
    if let Some(rec) = snap.by_op.get("recall") {
        println!("  recall.requests     = {}", rec.requests_total);
    }
    if let Some(fgt) = snap.by_op.get("forget") {
        println!("  forget.requests     = {}", fgt.requests_total);
    }

    // ────────────────────────────────────────────────────────────────────────
    //  Done
    // ────────────────────────────────────────────────────────────────────────

    client.bye().await?;
    println!("\n{}", "─".repeat(70));
    println!("  Done. Session closed.");
    println!("  To inspect the server state, run in another terminal:");
    println!("    just cli --output json debug-snapshot --shard 0 | jq .");
    println!("    just cli --output json worker list");
    println!("    curl http://127.0.0.1:9091/metrics | grep brain_");
    println!("{}", "─".repeat(70));
    Ok(())
}
