//! Read-only recovery of the `(namespace, space)` owners in a shard's
//! `metadata.redb`, with a per-space memory count. Scans the
//! `memories_by_space_timeline` index and aggregates its key prefix.
//!
//! Use when an space id was minted randomly and never persisted to a
//! manifest, so the only surviving copy is inside the on-disk keys — this
//! prints it back as hex so a client can reconnect / `act_as` that space.
//!
//!   cargo run -p brain-metadata --example dump_spaces -- <path/to/metadata.redb>

use std::collections::BTreeMap;

use brain_metadata::tables::memory::{MEMORIES_BY_SPACE_TIMELINE_TABLE, SPACE_TIMELINE_KEY_LEN};
use redb::{Database, ReadableDatabase, ReadableTable};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: dump_spaces <metadata.redb>");
    let db = Database::open(&path).expect("open redb");
    let rtxn = db.begin_read().expect("read txn");
    let t = rtxn
        .open_table(MEMORIES_BY_SPACE_TIMELINE_TABLE)
        .expect("open timeline table");

    // key = namespace(4, big-endian) + space(16) + created_at(8) + context(8) + memory_id(16)
    let mut counts: BTreeMap<(u32, [u8; 16]), u64> = BTreeMap::new();
    for entry in t.iter().expect("iter timeline") {
        let (k, _) = entry.expect("row");
        let key = k.value();
        if key.len() != SPACE_TIMELINE_KEY_LEN {
            continue;
        }
        let mut ns = [0u8; 4];
        ns.copy_from_slice(&key[0..4]);
        let mut space = [0u8; 16];
        space.copy_from_slice(&key[4..20]);
        *counts.entry((u32::from_be_bytes(ns), space)).or_default() += 1;
    }

    println!("== (namespace, space) owners : memory count ==");
    if counts.is_empty() {
        println!("  (no timeline rows)");
        return;
    }
    for ((ns, space), n) in &counts {
        let hex: String = space.iter().map(|b| format!("{b:02x}")).collect();
        println!("  namespace=0x{ns:08x}  space={hex}  memories={n}");
    }
}
