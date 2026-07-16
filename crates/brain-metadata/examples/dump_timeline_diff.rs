//! Diagnostic: for each timeline index key, reconstruct the key from the
//! memory row's own fields (exactly as `encode_cursor` does) and report any
//! component that differs. A mismatch means a resume cursor minted from a
//! row will not equal that row's real index key — the descending-pagination
//! duplicate bug.
//!
//!   cargo run -p brain-metadata --example dump_timeline_diff -- <metadata.redb> [agent_hex]

use brain_metadata::tables::memory::{
    agent_timeline_key, AGENT_TIMELINE_KEY_LEN, MEMORIES_BY_AGENT_TIMELINE_TABLE, MEMORIES_TABLE,
};
use redb::{Database, ReadableDatabase, ReadableTable};

fn be64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(b);
    u64::from_be_bytes(a)
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: <metadata.redb> [agent_hex]");
    let agent_filter: Option<[u8; 16]> = std::env::args().nth(2).map(|h| {
        let mut a = [0u8; 16];
        for (i, s) in a.iter_mut().enumerate() {
            *s = u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap();
        }
        a
    });

    let db = Database::open(&path).expect("open redb");
    let rtxn = db.begin_read().expect("read txn");
    let tt = rtxn.open_table(MEMORIES_BY_AGENT_TIMELINE_TABLE).expect("timeline");
    let mt = rtxn.open_table(MEMORIES_TABLE).expect("memories");

    let mut checked = 0u64;
    let mut mism_created = 0u64;
    let mut mism_context = 0u64;
    let mut mism_memid = 0u64;
    let mut mism_ns = 0u64;
    let mut mism_agent = 0u64;
    let mut examples = 0;

    for entry in tt.iter().expect("iter") {
        let (k, _) = entry.expect("row");
        let key = k.value();
        if key.len() != AGENT_TIMELINE_KEY_LEN {
            continue;
        }
        let mut agent = [0u8; 16];
        agent.copy_from_slice(&key[4..20]);
        if let Some(f) = agent_filter {
            if agent != f {
                continue;
            }
        }
        let mut id = [0u8; 16];
        id.copy_from_slice(&key[36..52]);
        let Some(row) = mt.get(&id).expect("get").map(|g| g.value()) else {
            continue;
        };
        let recon = agent_timeline_key(
            row.namespace_id,
            row.agent_id_bytes,
            row.created_at_unix_nanos,
            row.context_id,
            row.memory_id_bytes,
        );
        checked += 1;
        if recon != key {
            let key_ns = be64(&[0, 0, 0, 0, key[0], key[1], key[2], key[3]]);
            let key_created = be64(&key[20..28]);
            let key_ctx = be64(&key[28..36]);
            if recon[0..4] != key[0..4] {
                mism_ns += 1;
            }
            if recon[4..20] != key[4..20] {
                mism_agent += 1;
            }
            if recon[20..28] != key[20..28] {
                mism_created += 1;
            }
            if recon[28..36] != key[28..36] {
                mism_context += 1;
            }
            if recon[36..52] != key[36..52] {
                mism_memid += 1;
            }
            if examples < 5 {
                examples += 1;
                println!(
                    "MISMATCH: key(ns={key_ns} created={key_created} ctx={key_ctx}) vs row(ns={} created={} ctx={})",
                    row.namespace_id, row.created_at_unix_nanos, row.context_id
                );
            }
        }
    }

    println!("\n== checked {checked} rows ==");
    println!("mismatched: namespace={mism_ns} agent={mism_agent} created_at={mism_created} context={mism_context} memory_id={mism_memid}");
}
