//! Does batching actually amortise the forward pass?
//!
//! ENCODE calls `embed` once per request. If a batched forward pass costs
//! materially less than N single passes, an opportunistic batcher is worth
//! building; if it does not, the complexity buys nothing. Measure first.
use std::path::PathBuf;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::var("BRAIN_EMBED_MODEL_DIR")?;
    let handle = brain_embed::ModelHandle::load(&brain_embed::EmbedderConfig::new(PathBuf::from(dir)))?;

    let corpus: Vec<String> = (0..64)
        .map(|i| format!("Memory number {i}: a sentence of roughly realistic length for an agent note."))
        .collect();
    let _ = brain_embed::embed_text(&handle, &corpus[0])?;

    println!("{:<5} {:>26} {:>26} {:>9}", "n", "serial (per-text, rate)", "batched (per-text, rate)", "speedup");
    for n in [1usize, 4, 8, 16, 32, 64] {
        let texts: Vec<&str> = corpus[..n].iter().map(String::as_str).collect();

        let t0 = Instant::now();
        for t in &texts {
            let _ = brain_embed::embed_text(&handle, t)?;
        }
        let serial_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t1 = Instant::now();
        let _ = brain_embed::embed_batch(&handle, &texts)?;
        let batch_ms = t1.elapsed().as_secs_f64() * 1000.0;

        let s_each = serial_ms / n as f64;
        let b_each = batch_ms / n as f64;
        println!(
            "{n:<5} {:>12.1}ms {:>6.1}/s {:>12.1}ms {:>6.1}/s {:>8.1}x",
            s_each, 1000.0 / s_each, b_each, 1000.0 / b_each, s_each / b_each
        );
    }
    Ok(())
}
