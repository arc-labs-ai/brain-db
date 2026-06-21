//! User-declared statement-kind registry ops.
//!
//! The six built-in kinds resolve via `brain_core::StatementKind::
//! builtin_behavior` and are never stored. This module interns
//! **user-declared** kinds (bytes `>= 6`) and resolves a stored byte back
//! to its [`KindBehavior`] on the read path. Mirrors the predicate-intern
//! pattern in [`super::predicate`].

use brain_core::{KindBehavior, KindCardinality, StatementKind, TemporalModel};
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::tables::kind::{KindDefinition, KINDS_BY_BYTE_TABLE, KINDS_TABLE};

#[derive(thiserror::Error, Debug)]
pub enum KindOpError {
    #[error("redb storage: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb table: {0}")]
    Table(#[from] redb::TableError),
    #[error("redb commit: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("kind registry full: no free Custom byte (cap 254)")]
    Capacity,
    #[error("kind {0:?} redeclared with conflicting behavior")]
    Conflict(String),
}

fn qname(namespace: &str, name: &str) -> String {
    format!("{namespace}:{name}")
}

/// Intern a user-declared kind, returning its `StatementKind::Custom`
/// discriminant. Idempotent: re-declaring with identical behavior returns
/// the existing byte; re-declaring with different behavior is a conflict
/// (matching the all-or-nothing schema-merge semantics).
#[allow(clippy::too_many_arguments)]
pub fn kind_intern(
    wtxn: &WriteTransaction,
    namespace: &str,
    name: &str,
    cardinality: KindCardinality,
    temporal: TemporalModel,
    polarity: bool,
    hint: &str,
    schema_version: u32,
    now_unix_nanos: u64,
) -> Result<StatementKind, KindOpError> {
    let q = qname(namespace, name);

    // Idempotency / conflict probe.
    let existing: Option<KindDefinition> = {
        let t = wtxn.open_table(KINDS_TABLE)?;
        let got = t.get(q.as_str())?.map(|g| g.value());
        got
    };
    if let Some(existing) = existing {
        let same = existing.cardinality == cardinality.as_u8()
            && existing.temporal == temporal.as_u8()
            && existing.polarity == polarity;
        if !same {
            return Err(KindOpError::Conflict(q));
        }
        return Ok(StatementKind::from_u8(existing.byte_id));
    }

    // Allocate the next free Custom byte (>= FIRST_CUSTOM_BYTE).
    let next_byte: u8 = {
        let t = wtxn.open_table(KINDS_BY_BYTE_TABLE)?;
        let mut max: u8 = StatementKind::FIRST_CUSTOM_BYTE - 1;
        for entry in t.iter()? {
            let (k, _) = entry?;
            let b = k.value();
            if b > max {
                max = b;
            }
        }
        max.checked_add(1).ok_or(KindOpError::Capacity)?
    };

    let row = KindDefinition {
        byte_id: next_byte,
        namespace: namespace.to_string(),
        name: name.to_string(),
        cardinality: cardinality.as_u8(),
        temporal: temporal.as_u8(),
        polarity,
        hint: hint.to_string(),
        schema_version,
        created_at_unix_nanos: now_unix_nanos,
    };
    {
        let mut t = wtxn.open_table(KINDS_TABLE)?;
        t.insert(q.as_str(), &row)?;
    }
    {
        let mut idx = wtxn.open_table(KINDS_BY_BYTE_TABLE)?;
        idx.insert(next_byte, q.as_str())?;
    }
    Ok(StatementKind::Custom(next_byte))
}

/// Decode a stored [`KindDefinition`] row into a [`KindBehavior`].
fn behavior_of(row: &KindDefinition) -> KindBehavior {
    KindBehavior {
        cardinality: KindCardinality::from_u8(row.cardinality).unwrap_or(KindCardinality::Set),
        temporal: TemporalModel::from_u8(row.temporal).unwrap_or(TemporalModel::Atemporal),
        polarity: row.polarity,
    }
}

/// Resolve the behavior of any kind. Built-ins use the core const;
/// `Custom` bytes resolve via the registry. An unregistered Custom byte
/// degrades to Fact-like behavior (set / atemporal) rather than failing —
/// the read path never panics on a missing declaration.
pub fn kind_behavior(
    rtxn: &ReadTransaction,
    kind: StatementKind,
) -> Result<KindBehavior, KindOpError> {
    if let Some(b) = kind.builtin_behavior() {
        return Ok(b);
    }
    let StatementKind::Custom(byte) = kind else {
        // builtin_behavior covered every non-Custom variant.
        return Ok(KindBehavior::new(
            KindCardinality::Set,
            TemporalModel::Atemporal,
            false,
        ));
    };
    let qn: Option<String> = {
        let idx = rtxn.open_table(KINDS_BY_BYTE_TABLE)?;
        idx.get(byte)?.map(|g| g.value().to_string())
    };
    let Some(qn) = qn else {
        return Ok(KindBehavior::new(
            KindCardinality::Set,
            TemporalModel::Atemporal,
            false,
        ));
    };
    let row: Option<KindDefinition> = {
        let t = rtxn.open_table(KINDS_TABLE)?;
        t.get(qn.as_str())?.map(|g| g.value())
    };
    Ok(row.map(|r| behavior_of(&r)).unwrap_or_else(|| {
        KindBehavior::new(KindCardinality::Set, TemporalModel::Atemporal, false)
    }))
}

/// Whether a new `(subject, predicate)` assertion of this kind supersedes
/// the prior current value — i.e. the kind is single-valued and not an
/// event. Resolves built-ins via the core const and `Custom` via the
/// registry. Write-txn variant for the `statement_create` hot path.
pub fn kind_supersedes_w(
    wtxn: &WriteTransaction,
    kind: StatementKind,
) -> Result<bool, KindOpError> {
    if let Some(b) = kind.builtin_behavior() {
        return Ok(b.supersedes());
    }
    let StatementKind::Custom(byte) = kind else {
        return Ok(false);
    };
    let qn: Option<String> = {
        let idx = wtxn.open_table(KINDS_BY_BYTE_TABLE)?;
        let got = idx.get(byte)?.map(|g| g.value().to_string());
        got
    };
    let Some(qn) = qn else {
        return Ok(false);
    };
    let row: Option<KindDefinition> = {
        let t = wtxn.open_table(KINDS_TABLE)?;
        let got = t.get(qn.as_str())?.map(|g| g.value());
        got
    };
    Ok(row.map(|r| behavior_of(&r).supersedes()).unwrap_or(false))
}

/// All user-declared kinds, sorted by qname for deterministic output.
pub fn kind_list(rtxn: &ReadTransaction) -> Result<Vec<KindDefinition>, KindOpError> {
    let t = rtxn.open_table(KINDS_TABLE)?;
    let mut out = Vec::new();
    for entry in t.iter()? {
        let (_, v) = entry?;
        out.push(v.value());
    }
    out.sort_by(|a, b| {
        (a.namespace.as_str(), a.name.as_str()).cmp(&(b.namespace.as_str(), b.name.as_str()))
    });
    Ok(out)
}

/// Render the active kind taxonomy as a classifier-prompt block: the six
/// built-in kinds (with their fixed behavior + one-line guidance) followed
/// by every user-declared kind and its `hint`. Substituted into the LLM
/// extractor prompt's `{DECLARED_KINDS}` placeholder so the classifier
/// only ever picks a kind that exists.
pub fn render_declared_kinds_block(rtxn: &ReadTransaction) -> Result<String, KindOpError> {
    let mut out = String::new();
    for (name, card, temporal, guide) in BUILTIN_KIND_GUIDE {
        out.push_str("- ");
        out.push_str(name);
        out.push_str(" (");
        out.push_str(card);
        out.push_str(", ");
        out.push_str(temporal);
        out.push_str("): ");
        out.push_str(guide);
        out.push('\n');
    }
    for k in kind_list(rtxn)? {
        let card = if k.cardinality == KindCardinality::Single.as_u8() {
            "single"
        } else {
            "set"
        };
        let temporal = match TemporalModel::from_u8(k.temporal) {
            Some(TemporalModel::State) => "state",
            Some(TemporalModel::Event) => "event",
            _ => "atemporal",
        };
        out.push_str("- ");
        out.push_str(&k.namespace);
        out.push(':');
        out.push_str(&k.name);
        out.push_str(" (");
        out.push_str(card);
        out.push_str(", ");
        out.push_str(temporal);
        out.push(')');
        if !k.hint.is_empty() {
            out.push_str(": ");
            out.push_str(&k.hint);
        }
        out.push('\n');
    }
    Ok(out)
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use redb::ReadableDatabase;

    const NOW: u64 = 1_700_000_000_000_000_000;

    #[test]
    fn intern_allocates_custom_byte_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let wtxn = db.begin_write().unwrap();

        let k = kind_intern(
            &wtxn,
            "acme",
            "investment",
            KindCardinality::Set,
            TemporalModel::Event,
            false,
            "an entity funded another",
            1,
            NOW,
        )
        .unwrap();
        // First custom kind gets the first custom byte.
        assert_eq!(k, StatementKind::Custom(StatementKind::FIRST_CUSTOM_BYTE));

        // Re-declaring with identical behavior returns the same byte.
        let again = kind_intern(
            &wtxn,
            "acme",
            "investment",
            KindCardinality::Set,
            TemporalModel::Event,
            false,
            "an entity funded another",
            1,
            NOW,
        )
        .unwrap();
        assert_eq!(again, k);

        // A second distinct kind gets the next byte.
        let k2 = kind_intern(
            &wtxn,
            "acme",
            "sponsorship",
            KindCardinality::Single,
            TemporalModel::State,
            false,
            "",
            1,
            NOW,
        )
        .unwrap();
        assert_eq!(
            k2,
            StatementKind::Custom(StatementKind::FIRST_CUSTOM_BYTE + 1)
        );
    }

    #[test]
    fn intern_conflicts_on_divergent_redeclaration() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let wtxn = db.begin_write().unwrap();
        kind_intern(
            &wtxn,
            "acme",
            "deal",
            KindCardinality::Set,
            TemporalModel::State,
            false,
            "",
            1,
            NOW,
        )
        .unwrap();
        let err = kind_intern(
            &wtxn,
            "acme",
            "deal",
            KindCardinality::Single,
            TemporalModel::State,
            false,
            "",
            1,
            NOW,
        );
        assert!(matches!(err, Err(KindOpError::Conflict(_))));
    }

    #[test]
    fn supersedes_is_kind_derived() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let wtxn = db.begin_write().unwrap();

        // Built-ins: Attribute/Directive supersede; Relation/Event/Fact/Pref do not.
        assert!(kind_supersedes_w(&wtxn, StatementKind::Attribute).unwrap());
        assert!(kind_supersedes_w(&wtxn, StatementKind::Directive).unwrap());
        assert!(!kind_supersedes_w(&wtxn, StatementKind::Relation).unwrap());
        assert!(!kind_supersedes_w(&wtxn, StatementKind::Event).unwrap());
        assert!(!kind_supersedes_w(&wtxn, StatementKind::Fact).unwrap());
        assert!(!kind_supersedes_w(&wtxn, StatementKind::Preference).unwrap());

        // A custom single/state kind supersedes; a custom set kind does not.
        let single = kind_intern(
            &wtxn,
            "acme",
            "current_role",
            KindCardinality::Single,
            TemporalModel::State,
            false,
            "",
            1,
            NOW,
        )
        .unwrap();
        let set = kind_intern(
            &wtxn,
            "acme",
            "tag",
            KindCardinality::Set,
            TemporalModel::State,
            false,
            "",
            1,
            NOW,
        )
        .unwrap();
        assert!(kind_supersedes_w(&wtxn, single).unwrap());
        assert!(!kind_supersedes_w(&wtxn, set).unwrap());

        // An unregistered custom byte degrades to non-superseding.
        assert!(!kind_supersedes_w(&wtxn, StatementKind::Custom(200)).unwrap());
    }

    #[test]
    fn render_block_lists_builtins_then_custom() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        {
            let wtxn = db.begin_write().unwrap();
            kind_intern(
                &wtxn,
                "acme",
                "investment",
                KindCardinality::Set,
                TemporalModel::Event,
                false,
                "funded another",
                1,
                NOW,
            )
            .unwrap();
            wtxn.commit().unwrap();
        }
        let rtxn = db.begin_read().unwrap();
        let block = render_declared_kinds_block(&rtxn).unwrap();
        // All six built-ins plus the custom kind appear.
        for name in [
            "Attribute",
            "Relation",
            "Preference",
            "Event",
            "Directive",
            "Fact",
        ] {
            assert!(block.contains(name), "missing builtin {name}");
        }
        assert!(block.contains("acme:investment"));
        assert!(block.contains("funded another"));
    }
}

/// One-line classifier guidance per built-in kind: (name, cardinality,
/// temporal, what-belongs-here).
const BUILTIN_KIND_GUIDE: [(&str, &str, &str, &str); 6] = [
    (
        "Attribute",
        "single",
        "state",
        "an entity has a property with a value (role, age, location, name) — the current value",
    ),
    (
        "Relation",
        "set",
        "state",
        "an entity is linked to another entity (works_at, knows, manages, married_to)",
    ),
    (
        "Preference",
        "set",
        "state",
        "an entity likes/wants/avoids something, with +/- polarity",
    ),
    (
        "Event",
        "set",
        "event",
        "an entity did something at a time (traveled, bought, met, donated) — append-only",
    ),
    (
        "Directive",
        "single",
        "state",
        "how the agent should behave for/about the subject (response style, standing instruction)",
    ),
    (
        "Fact",
        "set",
        "atemporal",
        "a general subject-predicate-object that fits no other kind — the catch-all",
    ),
];
