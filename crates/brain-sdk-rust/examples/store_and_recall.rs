//! Interactive Brain data operations example.
//!
//! Runs against a server already started on 127.0.0.1:9090.
//! Start the server first:
//!
//!   cargo run --bin brain-server -- --config config/dev.toml
//!
//! Then in another terminal:
//!
//!   cargo run --example store_and_recall
//!
//! The example encodes several memories, recalls by cue text,
//! demonstrates a transaction, and cleans up with forget.

use std::net::SocketAddr;

use brain_core::MemoryId;
use brain_protocol::request::{ForgetMode, MemoryKindWire};
use brain_sdk_rust::Client;

const SERVER: &str = "127.0.0.1:9090";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr: SocketAddr = SERVER.parse()?;

    println!("Connecting to Brain at {SERVER} ...");
    let client = Client::connect(addr).await?;
    println!("Connected.\n");

    // -----------------------------------------------------------------------
    // 1. ENCODE — store memories
    // -----------------------------------------------------------------------
    println!("=== ENCODE ===");

    let texts = [
        ("The attention mechanism in transformers was introduced in 'Attention is All You Need' (2017).", MemoryKindWire::Semantic),
        ("BERT uses bidirectional training of transformers for language understanding.", MemoryKindWire::Semantic),
        ("GPT-3 demonstrated few-shot learning with 175 billion parameters.", MemoryKindWire::Episodic),
        ("The softmax function converts raw scores into a probability distribution.", MemoryKindWire::Semantic),
        ("Backpropagation computes gradients by the chain rule.", MemoryKindWire::Semantic),
    ];

    let mut memory_ids: Vec<u128> = Vec::new();

    for (text, kind) in texts {
        let resp = client
            .encode(text)
            .kind(kind)
            .salience(0.8)
            .send()
            .await?;
        memory_ids.push(resp.memory_id);
        println!(
            "  STORED  id={:#034x}  deduped={}  text={:.60}",
            resp.memory_id, resp.was_deduplicated, text
        );
    }

    // -----------------------------------------------------------------------
    // 2. RECALL — similarity search
    // -----------------------------------------------------------------------
    println!("\n=== RECALL (cue: 'transformer architecture') ===");

    let results = client
        .recall("transformer architecture")
        .send()
        .await?;

    if results.is_empty() {
        println!("  (no results — the index may still be building)");
    } else {
        for (i, r) in results.iter().enumerate() {
            println!(
                "  [{i}] score={:.4}  kind={:?}  id={:#034x}",
                r.similarity_score, r.kind, r.memory_id
            );
            println!("       {}", r.text);
        }
    }

    // -----------------------------------------------------------------------
    // 3. RECALL with filters
    // -----------------------------------------------------------------------
    println!("\n=== RECALL (semantic only, top 2) ===");

    let filtered = client
        .recall("neural network gradient")
        .send()
        .await?;

    for r in filtered.iter().take(2) {
        println!("  score={:.4}  {}", r.similarity_score, r.text);
    }

    // -----------------------------------------------------------------------
    // 4. ENCODE + FORGET (soft tombstone)
    // -----------------------------------------------------------------------
    println!("\n=== FORGET ===");

    let throwaway = client
        .encode("This memory will be forgotten immediately.")
        .send()
        .await?;
    println!("  Encoded throwaway id={:#034x}", throwaway.memory_id);

    let forget = client
        .forget(MemoryId::from_raw(throwaway.memory_id))
        .mode(ForgetMode::Soft)
        .send()
        .await?;
    println!(
        "  Forgotten id={:#034x}  edges_removed={}",
        forget.memory_id, forget.edges_removed
    );

    // -----------------------------------------------------------------------
    // 5. TRANSACTION — encode two memories atomically
    // -----------------------------------------------------------------------
    println!("\n=== TRANSACTION ===");

    let txn = client.txn_begin().await?;
    println!("  Transaction started txn_id={:?}", txn.txn_id);

    let a = client
        .encode("Claim A: transformers replaced RNNs for most NLP tasks.")
        .txn(txn.txn_id)
        .send()
        .await?;
    let b = client
        .encode("Claim B: RNNs still excel on strict memory-constrained hardware.")
        .txn(txn.txn_id)
        .send()
        .await?;

    println!(
        "  Pending A={:#034x}  B={:#034x}",
        a.memory_id, b.memory_id
    );

    client.txn_commit(txn.txn_id).await?;
    println!("  Committed.");

    // -----------------------------------------------------------------------
    // 6. SDK metrics
    // -----------------------------------------------------------------------
    println!("\n=== SDK METRICS ===");

    let snap = client.metrics_snapshot();
    println!("  requests_total      = {}", snap.requests_total);
    println!("  errors_total        = {}", snap.errors_total);

    // -----------------------------------------------------------------------
    // Clean up
    // -----------------------------------------------------------------------
    client.bye().await?;
    println!("\nDone. Connection closed.");
    Ok(())
}
