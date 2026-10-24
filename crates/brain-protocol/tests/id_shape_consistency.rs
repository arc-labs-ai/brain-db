//! One id, one shape.
//!
//! `memory_id` was carried two ways at once: `WireMemoryId` (a `u128`) on
//! ENCODE, RECALL, FORGET, LINK, SUBSCRIBE and the admin ops, and
//! `[u8; 16]` on MEMORY_INSPECT, MEMORY_LIST and the write-trace artifacts.
//! Chaining the two — take the id ENCODE returned, inspect it — was refused by
//! the server with `invalid length 9, expected a byte array of length 16`.
//!
//! Nothing caught it for a long time, and the reasons are worth stating because
//! they say what this test is for:
//!
//! * **The corpus cannot.** It round-trips one message at a time, so it proves
//!   every message encodes to the right bytes and never that two messages agree
//!   about a value passed between them.
//! * **The HTTP tier hides it.** `brain-edge` renders every id as a decimal
//!   string, so the JSON contract and all three HTTP SDKs looked consistent
//!   while the wire underneath was not.
//! * **The tests that did chain the verbs papered over it.** They converted,
//!   and in Rust the conversion is a plausible-looking `to_be_bytes()` rather
//!   than an error — `brain-ops/tests/memory_list.rs` even carried a helper
//!   whose doc comment read "`memory_id` bytes → the u128 wire id used by
//!   ENCODE responses". The inconsistency was written down as a fact about the
//!   protocol instead of read as a defect in it.
//!
//! So this checks the property directly, at the declaration: group every
//! `*_id` / `*_ids` field in the protocol by name, resolve the type aliases,
//! and require each name to be carried in exactly one shape.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Field names allowed to carry more than one shape, each with the reason.
///
/// Keep this list short and argued. An entry here is a claim that two fields
/// share a name while meaning different things, which is worth a second look
/// every time one is added.
const DELIBERATE: &[(&str, &str)] = &[(
    "space_id",
    "Two different values share the name. `ActAs::space_id` and `SpaceView::space_id` \
     are the human-readable structured space STRING a caller writes; \
     `AuthOkPayload::space_id` and friends are the 16-byte storage id the server \
     derives from it with UUIDv5. Renaming one is the real fix; until then they are \
     distinguishable by type, which is why this is tolerable rather than good.",
)];

/// Type aliases that are the same wire shape, normalised so an alias and its
/// underlying type do not read as a disagreement.
fn canonical(ty: &str) -> String {
    let mut t = ty.trim().to_string();
    // Unwrap the containers; what matters is the element shape.
    loop {
        let unwrapped = t
            .strip_prefix("Option<")
            .or_else(|| t.strip_prefix("Vec<"))
            .or_else(|| t.strip_prefix("SmallVec<"))
            .and_then(|rest| rest.strip_suffix('>'))
            .map(str::to_string);
        match unwrapped {
            Some(inner) => t = inner.trim().to_string(),
            None => break,
        }
    }
    match t.as_str() {
        "WireUuid" | "[u8; 16]" => "16 bytes".to_string(),
        "WireSessionId" | "u64" => "u64".to_string(),
        "WireMemoryId" | "u128" => "u128".to_string(),
        other => other.to_string(),
    }
}

#[test]
fn every_id_field_is_carried_in_exactly_one_shape() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut shapes: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();

    for file in rust_files(&src) {
        let text = std::fs::read_to_string(&file).expect("read source");
        let name = file
            .strip_prefix(&src)
            .unwrap_or(&file)
            .display()
            .to_string();
        let mut current_struct = String::new();
        for (lineno, line) in text.lines().enumerate() {
            if let Some(rest) = line.strip_prefix("pub struct ") {
                current_struct = rest
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or("")
                    .to_string();
            }
            let Some((field, ty)) = pub_field(line) else {
                continue;
            };
            if !(field.ends_with("_id") || field.ends_with("_ids")) {
                continue;
            }
            // `*_ids` is the plural of `*_id`; a list of ids must be the same
            // shape as a single one or the pair is the same defect.
            let key = field.strip_suffix('s').unwrap_or(&field).to_string();
            shapes
                .entry(key)
                .or_default()
                .entry(canonical(&ty))
                .or_default()
                .push(format!("{name}:{} {current_struct}", lineno + 1));
        }
    }

    assert!(
        shapes.len() > 20,
        "only found {} id fields; the parser is broken, not the protocol",
        shapes.len()
    );

    let allowed: BTreeSet<&str> = DELIBERATE.iter().map(|(name, _)| *name).collect();
    let mut failures = String::new();
    for (field, by_shape) in &shapes {
        if by_shape.len() < 2 || allowed.contains(field.as_str()) {
            continue;
        }
        failures.push_str(&format!("\n`{field}` is carried in {} shapes:\n", by_shape.len()));
        for (shape, sites) in by_shape {
            failures.push_str(&format!("    {shape}\n"));
            for site in sites.iter().take(6) {
                failures.push_str(&format!("        {site}\n"));
            }
            if sites.len() > 6 {
                failures.push_str(&format!("        … and {} more\n", sites.len() - 6));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "an id is carried in more than one wire shape, so a value one op returns \
         cannot be passed to another:\n{failures}\nPick one shape. If the two really \
         are different values that happen to share a name, rename one — or add it to \
         DELIBERATE in this file with the argument."
    );
}

/// `    pub <name>: <type>,` → `(name, type)`.
fn pub_field(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("pub ")?;
    let (name, after) = rest.split_once(':')?;
    let name = name.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let ty = after.trim().strip_suffix(',')?;
    Some((name.to_string(), ty.to_string()))
}

fn rust_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out.sort();
    out
}
