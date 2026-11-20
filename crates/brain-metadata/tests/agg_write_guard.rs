//! Guard: every write to `MEMORIES_TABLE` must go through
//! `brain_metadata::tables::agg::{put_memory, remove_memory, ..}` so the
//! aggregate counters stay exact. A direct `.insert(` / `.remove(` on a
//! table handle opened from `MEMORIES_TABLE` in non-test code fails here.
//!
//! Heuristic, std-only scan: whitespace is stripped, each
//! `let NAME = ...open_table(...MEMORIES_TABLE)` binding is tracked until
//! `NAME` is re-bound, and mutating calls on `NAME` in that span are
//! reported. Also caught: handles passed in as `&mut Table<.., MemoryMetadata>`
//! parameters, `MEMORIES_TABLE as ALIAS` imports, chained
//! `open_table(MEMORIES_TABLE)?.insert(..)` calls, and `delete_table`.
//! Each `#[cfg(test)]` item (brace-matched), `tests/`, `benches/` and
//! `examples/` are skipped — fixtures may seed rows directly.
//!
//! Not caught (text scan limits): handles bound by tuple destructuring or
//! returned from helper functions. None exist today; the runtime backstop
//! is the `agg_counters` worker's periodic verify, which repairs any drift
//! and reports it in `brain_agg_counter_drifted_buckets_total`.

use std::path::{Path, PathBuf};

const ALLOWED: &[&str] = &["brain-metadata/src/tables/agg.rs"];
const MUTATORS: &[&str] = &[
    ".insert(",
    ".remove(",
    ".retain(",
    ".retain_in(",
    ".pop_first(",
    ".pop_last(",
    ".extract_if(",
    ".extract_from_if(",
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if p.is_dir() {
            if !matches!(name, "tests" | "benches" | "examples" | "target") {
                rust_files(&p, out);
            }
        } else if name.ends_with(".rs") && name != "tests.rs" && !name.ends_with("_tests.rs") {
            out.push(p);
        }
    }
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `src` with comments, string/char literal contents, and every item
/// annotated `#[cfg(test)]` / `#[cfg(all(test, ..))]` blanked out.
fn strip_tests_and_literals(src: &str) -> String {
    // 1. Blank comments and literal contents (keep delimiters), so braces in
    //    format strings don't confuse brace matching below.
    let b: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && b.get(i + 1) == Some(&'*') {
            // Block comments nest in Rust.
            let mut depth = 0usize;
            while i + 1 < b.len() {
                if b[i] == '/' && b[i + 1] == '*' {
                    depth += 1;
                    i += 2;
                } else if b[i] == '*' && b[i + 1] == '/' {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if c == 'r'
            && matches!(b.get(i + 1), Some('"' | '#'))
            && (i == 0
                || !is_ident(b[i - 1])
                // b"…"/c"…" raw forms: br"…", cr"…"
                || (matches!(b[i - 1], 'b' | 'c') && (i == 1 || !is_ident(b[i - 2]))))
        {
            // Raw string r"..." / r#"..."#: skip to the matching close.
            let mut j = i + 1;
            let mut hashes = 0;
            while b.get(j) == Some(&'#') {
                hashes += 1;
                j += 1;
            }
            if b.get(j) != Some(&'"') {
                out.push(c);
                i += 1;
                continue;
            }
            j += 1;
            loop {
                if j >= b.len() {
                    break;
                }
                if b[j] == '"' && (1..=hashes).all(|h| b.get(j + h) == Some(&'#')) {
                    j += 1 + hashes;
                    break;
                }
                j += 1;
            }
            out.push_str("\"\"");
            i = j;
        } else if c == '"' {
            out.push('"');
            i += 1;
            while i < b.len() && b[i] != '"' {
                i += if b[i] == '\\' { 2 } else { 1 };
            }
            out.push('"');
            i += 1;
        } else if c == '\'' && b.get(i + 2) == Some(&'\'') {
            out.push_str("' '");
            i += 3;
        } else if c == '\'' && b.get(i + 1) == Some(&'\\') && b.get(i + 3) == Some(&'\'') {
            out.push_str("' '");
            i += 4;
        } else {
            out.push(c);
            i += 1;
        }
    }
    // 2. Remove each cfg(test) item: up to its matching `}` or its `;`.
    let mut s = out;
    while let Some(at) = ["#[cfg(test)]", "#[cfg(all(test"]
        .iter()
        .filter_map(|m| s.find(m))
        .min()
    {
        let rest = &s[at..];
        let brace = rest.find('{');
        // A `;` inside `[..]` / `(..)` (e.g. `[u8; 16]` in a signature) does
        // not end the item.
        let semi = top_level_semi(rest);
        let end = match (brace, semi) {
            (Some(bo), Some(se)) if se < bo => at + se + 1,
            (Some(bo), _) => {
                let mut depth = 0usize;
                let mut end = s.len();
                for (k, ch) in rest[bo..].char_indices() {
                    match ch {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = at + bo + k + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                end
            }
            (None, Some(se)) => at + se + 1,
            (None, None) => s.len(),
        };
        s.replace_range(at..end, "");
    }
    s
}

/// Drop whitespace, except one space between two identifier characters, so
/// tokens never merge (`let mut_x` stays distinct from `letmut_x`).
fn compact(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut pending_space = false;
    for c in src.chars() {
        if c.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && is_ident(c) && out.chars().next_back().is_some_and(is_ident) {
            out.push(' ');
        }
        pending_space = false;
        out.push(c);
    }
    out
}

/// The identifier ending at byte `end` of `s`, if any.
fn ident_before(s: &str, end: usize) -> &str {
    let start = s[..end].rfind(|c: char| !is_ident(c)).map_or(0, |i| i + 1);
    &s[start..end]
}

/// First `;` outside any `[..]` or `(..)`.
fn top_level_semi(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' => depth -= 1,
            ';' if depth <= 0 => return Some(i),
            '{' if depth <= 0 => return None,
            _ => {}
        }
    }
    None
}

/// Offending `(binding, mutator)` pairs in one file's source.
fn violations(src: &str) -> Vec<String> {
    let s = compact(&strip_tests_and_literals(src));
    let mut found = Vec::new();

    // Names the memories table goes by in this file: itself plus any
    // `MEMORIES_TABLE as ALIAS` import.
    let mut tables = vec!["MEMORIES_TABLE".to_owned()];
    let mut from = 0;
    while let Some(rel) = s[from..].find("MEMORIES_TABLE as ") {
        let at = from + rel + "MEMORIES_TABLE as ".len();
        from = at;
        let alias: String = s[at..].chars().take_while(|c| is_ident(*c)).collect();
        if !alias.is_empty() {
            tables.push(alias);
        }
    }
    for t in &tables {
        if s.contains(&format!("delete_table({t})")) || s.contains(&format!("delete_table(&{t})")) {
            found.push(format!("delete_table({t})"));
        }
        found.extend(handle_writes(&s, t));
    }
    found.extend(param_writes(&s));
    found
}

/// Writes through handles opened from `table` (by `let` binding or chained).
fn handle_writes(s: &str, table: &str) -> Vec<String> {
    let mut found = Vec::new();
    let needle = format!("{table})");
    let mut from = 0;
    while let Some(rel) = s[from..].find(&needle) {
        let at = from + rel;
        from = at + 1;
        // Only `open_table(` calls (optionally path-qualified).
        let head = &s[..at];
        let open = head.rfind("open_table(");
        let Some(open) = open else { continue };
        if !head[open + "open_table(".len()..]
            .chars()
            .all(|c| is_ident(c) || c == ':')
            || !(head.ends_with('(') || head.ends_with(':'))
        {
            continue;
        }
        // Chained: `open_table(T)?.insert(..)` in the same statement.
        let stmt_end = s[at..].find(';').map_or(s.len(), |e| at + e);
        let stmt = &s[at + needle.len()..stmt_end];
        for m in MUTATORS {
            if stmt.starts_with(&format!("?{m}"))
                || stmt.starts_with(&format!(".unwrap(){m}"))
                || (stmt.starts_with(".expect(") && stmt.contains(&format!("){m}")))
            {
                found.push(format!("open_table({table}){m}"));
            }
        }
        // Nearest preceding `let` binding of the handle:
        // `let [mut ]name[: Type] =` or `let Ok([mut ]name) =`.
        let Some(let_at) = head[..open].rfind("let ") else {
            continue;
        };
        if let_at > 0 && head[..let_at].chars().next_back().is_some_and(is_ident) {
            continue;
        }
        let Some(eq) = head[let_at..].find('=') else {
            continue;
        };
        let mut name = &head[let_at + "let ".len()..let_at + eq];
        name = name.strip_prefix("Ok(").unwrap_or(name);
        name = name.strip_prefix("mut ").unwrap_or(name);
        let name = name.split(':').next().unwrap_or("").trim_end_matches(')');
        if name.is_empty() || !name.chars().all(is_ident) {
            continue;
        }
        // Span: until the name is re-bound.
        let tail = &s[at..];
        let end = ["=", ":"]
            .iter()
            .flat_map(|sep| [format!("let {name}{sep}"), format!("let mut {name}{sep}")])
            .filter_map(|r| tail.find(r.as_str()))
            .min()
            .unwrap_or(tail.len());
        found.extend(mutations(&tail[..end], name));
    }
    found
}

/// Writes through handles passed in as parameters: `name: &mut
/// [redb::]Table<'_, [u8; 16], MemoryMetadata>`. Span: to the end of the
/// function (next `fn`).
fn param_writes(s: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut params: Vec<(usize, String)> = Vec::new();

    // `name: &mut [path::]Table<'_, [u8; 16], [path::]MemoryMetadata>`.
    let mut from = 0;
    while let Some(rel) = s[from..].find("MemoryMetadata>") {
        let at = from + rel;
        from = at + 1;
        let head = &s[..at];
        let Some(k) = head.rfind(",[u8;16],") else {
            continue;
        };
        if !head[k + ",[u8;16],".len()..]
            .chars()
            .all(|c| is_ident(c) || c == ':')
        {
            continue;
        }
        let Some(table_at) = head[..k].rfind("Table<") else {
            continue;
        };
        if let Some(name) = param_name(head, table_at) {
            params.push((at, name));
        }
    }

    // Type aliases for the table: `type Alias<..> = ..Table<.., MemoryMetadata>;`
    // and `name: &mut Alias<..>` parameters.
    let mut from = 0;
    while let Some(rel) = s[from..].find("type ") {
        let at = from + rel;
        from = at + 1;
        if at > 0 && s[..at].chars().next_back().is_some_and(is_ident) {
            continue;
        }
        let alias: String = s[at + "type ".len()..]
            .chars()
            .take_while(|c| is_ident(*c))
            .collect();
        let Some(end) = top_level_semi(&s[at..]) else {
            continue;
        };
        let def = &s[at..at + end];
        if alias.is_empty() || !def.contains("Table<") || !def.contains("MemoryMetadata") {
            continue;
        }
        let needle = format!(":&mut {alias}");
        let mut f = 0;
        while let Some(r) = s[f..].find(&needle) {
            let p = f + r;
            f = p + 1;
            if s[p + needle.len()..].chars().next().is_some_and(is_ident) {
                continue; // a longer type name sharing the prefix
            }
            if let Some(name) = param_name(&s[..p + needle.len()], p + needle.len() - alias.len()) {
                params.push((p, name));
            }
        }
    }

    for (at, name) in params {
        let tail = &s[at..];
        let end = find_fn(tail).unwrap_or(tail.len());
        found.extend(mutations(&tail[..end], &name));
    }
    found
}

/// The parameter name in `name:&mut [path::]<type at type_at>`.
fn param_name(head: &str, type_at: usize) -> Option<String> {
    let colon = head[..type_at].rfind(":&mut")?;
    if !head[colon + ":&mut".len()..type_at]
        .trim_start()
        .chars()
        .all(|c| is_ident(c) || c == ':')
    {
        return None;
    }
    let name = ident_before(head, colon);
    (!name.is_empty()).then(|| name.to_owned())
}

/// Offset of the next `fn` keyword in compacted source.
fn find_fn(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    (1..b.len().saturating_sub(3)).find(|&i| {
        &b[i..i + 3] == b"fn " && !is_ident(char::from(b[i - 1])) && is_ident(char::from(b[i + 3]))
    })
}

/// Mutating calls on `name` inside `span`, following `let x = &mut name;`
/// re-borrows.
fn mutations(span: &str, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    // `let [mut ]u = &mut t;` compacts to `let [mut ]u=&mut t;`.
    let needle = format!("=&mut {name};");
    let mut i = 0;
    while let Some(r) = span[i..].find(&needle) {
        let pos = i + r;
        i = pos + 1;
        let head = &span[..pos];
        let alias = ident_before(head, pos);
        let before = &head[..pos - alias.len()];
        if !alias.is_empty()
            && alias != name
            && (before.ends_with("let ") || before.ends_with("let mut "))
        {
            out.extend(mutations(&span[pos..], alias));
        }
    }
    for m in MUTATORS {
        let needle = format!("{name}{m}");
        let mut i = 0;
        while let Some(r) = span[i..].find(&needle) {
            let pos = i + r;
            i = pos + 1;
            if !span[..pos].chars().next_back().is_some_and(is_ident) {
                out.push(needle.clone());
            }
        }
    }
    out
}

#[test]
fn memories_writes_go_through_agg_helpers() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    rust_files(&crates, &mut files);
    assert!(
        files.len() > 50,
        "scan found too few files: {}",
        files.len()
    );

    let mut bad = Vec::new();
    for f in files {
        let rel = f
            .strip_prefix(&crates)
            .unwrap_or(&f)
            .to_string_lossy()
            .replace('\\', "/");
        if ALLOWED.iter().any(|a| rel.ends_with(a)) {
            continue;
        }
        let src = std::fs::read_to_string(&f).unwrap();
        for v in violations(&src) {
            bad.push(format!("{rel}: {v}"));
        }
    }
    assert!(
        bad.is_empty(),
        "direct MEMORIES_TABLE writes bypass the aggregate counters; use \
         brain_metadata::tables::agg::{{put_memory, remove_memory}} instead:\n  {}",
        bad.join("\n  ")
    );
}

#[test]
fn guard_flags_a_direct_write_and_ignores_other_tables() {
    let direct =
        "fn f(w: &W) { let mut t = w.open_table(MEMORIES_TABLE).unwrap(); t.insert(&k, &r); }";
    assert_eq!(violations(direct), vec!["t.insert(".to_owned()]);

    let rebound = "fn f(w: &W) { { let mut t = w.open_table(MEMORIES_TABLE)?; put_memory(w, &mut t, k, &r)?; } \
                   { let mut t = w.open_table(TEXTS_TABLE)?; t.insert(&k, b)?; } }";
    assert!(violations(rebound).is_empty());

    let param = "fn f(t: &mut redb::Table<'_, [u8; 16], MemoryMetadata>, k: K) { t.insert(&k, &r); }\nfn g(t: &mut T) { t.insert(1); }";
    assert_eq!(violations(param), vec!["t.insert(".to_owned()]);

    let param_ok = "fn f(t: &mut redb::Table<'_, [u8; 16], MemoryMetadata>, d: &mut D) { put_memory_deferred(t, k, &r, d)?; }";
    assert!(violations(param_ok).is_empty());

    let alias = "use x::MEMORIES_TABLE as MEM_T; fn f(w: &W) { let mut m = w.open_table(MEM_T).unwrap(); m.remove(&k); }";
    assert_eq!(violations(alias), vec!["m.remove(".to_owned()]);

    let chained = "fn f(w: &W) -> R { w.open_table(MEMORIES_TABLE)?.insert(&k, &r)?; Ok(()) }";
    assert_eq!(
        violations(chained),
        vec!["open_table(MEMORIES_TABLE).insert(".to_owned()]
    );

    let dropped = "fn f(w: &W) { w.delete_table(MEMORIES_TABLE).unwrap(); }";
    assert_eq!(
        violations(dropped),
        vec!["delete_table(MEMORIES_TABLE)".to_owned()]
    );

    // Only the cfg(test) item is skipped; code after it is still scanned,
    // and braces inside strings don't end the item early.
    let mid = "#[cfg(test)]\nfn helper() { let s = \"}}{\"; }\nfn real(w: &W) { let mut t = w.open_table(MEMORIES_TABLE).unwrap(); t.insert(&k, &r); }";
    assert_eq!(violations(mid), vec!["t.insert(".to_owned()]);

    let other_ident =
        "fn f(w: &W) { let mut t = w.open_table(OLD_MEMORIES_TABLE).unwrap(); t.insert(&k, &r); }";
    assert!(violations(other_ident).is_empty());

    let full_path = "fn f(t: &mut redb::Table<'_, [u8; 16], brain_metadata::tables::memory::MemoryMetadata>) { t.remove(&k); }";
    assert_eq!(violations(full_path), vec!["t.remove(".to_owned()]);

    let alias_type = "type Mem<'a> = redb::Table<'a, [u8; 16], MemoryMetadata>;\nfn f(m: &mut Mem<'_>) { m.insert(&k, &r); }";
    assert_eq!(violations(alias_type), vec!["m.insert(".to_owned()]);

    let reborrow = "fn f(w: &W) { let mut t = w.open_table(MEMORIES_TABLE).unwrap(); let u = &mut t; u.insert(&k, &r); }";
    assert_eq!(violations(reborrow), vec!["u.insert(".to_owned()]);

    let commented = "/* } */ fn f(w: &W) { let s = r#\"}{\"#; let mut t = w.open_table(MEMORIES_TABLE).unwrap(); t.insert(&k, &r); }";
    assert_eq!(violations(commented), vec!["t.insert(".to_owned()]);

    let test_sig = "#[cfg(test)]\nfn helper(a: [u8; 16]) { let mut t = w.open_table(MEMORIES_TABLE).unwrap(); t.insert(&a, &r); }";
    assert!(
        violations(test_sig).is_empty(),
        "cfg(test) fn with [u8; 16] is skipped whole"
    );

    let mut_prefixed = "fn f(w: &W) { let mut_tbl = w.open_table(MEMORIES_TABLE).unwrap(); mut_tbl.insert(&k, &r); }";
    assert_eq!(violations(mut_prefixed), vec!["mut_tbl.insert(".to_owned()]);

    let typed = "fn f(w: &W) { let mut t: Table<[u8; 16], MemoryMetadata> = w.open_table(MEMORIES_TABLE).unwrap(); t.insert(&k, &r); }";
    assert_eq!(violations(typed), vec!["t.insert(".to_owned()]);

    let byte_raw = "fn f(w: &W) { let p = br\"C:\\\"; let mut t = w.open_table(MEMORIES_TABLE).unwrap(); t.insert(&k, &r); }";
    assert_eq!(violations(byte_raw), vec!["t.insert(".to_owned()]);

    let nested = "/* a /* b */ \" */ fn f(w: &W) { let mut t = w.open_table(MEMORIES_TABLE).unwrap(); t.insert(&k, &r); }";
    assert_eq!(violations(nested), vec!["t.insert(".to_owned()]);

    let other_rebind = "fn f(w: &W) { let mut t = w.open_table(MEMORIES_TABLE).unwrap(); let mutt = 1; t.insert(&k, &r); }";
    assert_eq!(
        violations(other_rebind),
        vec!["t.insert(".to_owned()],
        "`let mutt` is not a rebinding of `t`"
    );

    let test_only = "fn f() {}\n#[cfg(test)]\nmod tests { fn g(w: &W) { let mut t = w.open_table(MEMORIES_TABLE).unwrap(); t.insert(&k, &r); } }";
    assert!(violations(test_only).is_empty());
}
