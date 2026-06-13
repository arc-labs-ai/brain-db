//! ClassifierExtractor — `Extractor` impl for the GLiNER classifier tier.

use std::collections::HashMap;
use std::sync::Arc;

use brain_core::ExtractorId;
use brain_core::{ExtractorKind, Memory};
use brain_protocol::schema::ExtractorTarget;

use super::model::{ClassifiedSpan, ClassifierModel};
use super::simple_label;
use crate::framework::extractor::{
    ExtractionContext, ExtractionFuture, ExtractionResult, ExtractionStatus, Extractor,
};
use crate::framework::item::{EntityMention, ExtractedItem};

/// Wires a [`ClassifierModel`] to the extraction pipeline. Carries the
/// active schema's entity-type qnames (`target_labels`) as the label
/// set to pass on every `predict()` call.
pub struct ClassifierExtractor {
    id: ExtractorId,
    name: String,
    target: ExtractorTarget,
    extractor_version: u32,
    confidence_threshold: f32,
    model: Option<Arc<dyn ClassifierModel>>,
    /// Label set passed to `ClassifierModel::predict` on every
    /// dispatch. Snapshotted at shard startup from the schema's
    /// entity-type registry. Empty → degraded (no labels = nothing
    /// to classify against).
    target_labels: Arc<Vec<String>>,
    /// Reason captured at construction time when the model couldn't
    /// load — surfaces in every degraded dispatch.
    degraded_reason: Option<String>,
}

impl ClassifierExtractor {
    /// Fully-wired extractor with a loaded model + non-empty label
    /// snapshot.
    pub fn new(
        id: ExtractorId,
        name: String,
        target: ExtractorTarget,
        extractor_version: u32,
        confidence_threshold: f32,
        model: Arc<dyn ClassifierModel>,
        target_labels: Arc<Vec<String>>,
    ) -> Self {
        Self {
            id,
            name,
            target,
            extractor_version,
            confidence_threshold,
            model: Some(model),
            target_labels,
            degraded_reason: None,
        }
    }

    /// Degraded extractor — no model loaded. Every dispatch writes a
    /// `SkippedDisabled` audit row with the captured reason.
    pub fn degraded(
        id: ExtractorId,
        name: String,
        target: ExtractorTarget,
        extractor_version: u32,
        confidence_threshold: f32,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            id,
            name,
            target,
            extractor_version,
            confidence_threshold,
            model: None,
            target_labels: Arc::new(Vec::new()),
            degraded_reason: Some(reason.into()),
        }
    }

    /// True iff a model is wired in.
    pub fn is_loaded(&self) -> bool {
        self.model.is_some()
    }

    /// Snapshot of the labels passed to every `predict()` call.
    pub fn target_labels(&self) -> &[String] {
        &self.target_labels
    }

    /// Build the (`simple_labels`, `qname_by_simple`) pair the classifier
    /// path uses to feed GLiNER plain labels and then remap them back
    /// to qnames on the way out. Pulled out of `run` so the batched
    /// path can share it without copy-paste.
    fn resolve_labels(&self) -> (Vec<String>, HashMap<String, String>) {
        let mut seen = std::collections::HashSet::new();
        let mut collision = false;
        for q in self.target_labels.iter() {
            if !seen.insert(simple_label(q.as_str())) {
                collision = true;
                break;
            }
        }
        if collision {
            tracing::warn!(
                target: "brain_extractors::classifier",
                "simple-label collision across namespaces; passing underscore-encoded qnames to GLiNER — accuracy degraded"
            );
            let simples: Vec<String> = self
                .target_labels
                .iter()
                .map(|q| q.replace(':', "_"))
                .collect();
            let map: HashMap<String, String> = self
                .target_labels
                .iter()
                .zip(simples.iter())
                .map(|(q, s)| (s.clone(), q.clone()))
                .collect();
            (simples, map)
        } else {
            let simples: Vec<String> = self
                .target_labels
                .iter()
                .map(|q| simple_label(q.as_str()).to_string())
                .collect();
            let map: HashMap<String, String> = self
                .target_labels
                .iter()
                .map(|q| (simple_label(q.as_str()).to_string(), q.clone()))
                .collect();
            (simples, map)
        }
    }

    /// Project a vector of GLiNER spans for one memory into the
    /// extractor's `ExtractedItem` output, applying confidence
    /// threshold and the simple→qname remap. Shared between the
    /// single-input `run` and batched `run_batch` paths.
    fn project_spans(
        &self,
        spans: Vec<ClassifiedSpan>,
        qname_by_label: &HashMap<String, String>,
    ) -> Vec<ExtractedItem> {
        let mut items = Vec::new();
        for mut span in spans {
            if span.confidence < self.confidence_threshold {
                continue;
            }
            match qname_by_label.get(span.label.as_str()) {
                Some(qname) => span.label = qname.clone(),
                None => continue,
            }
            // Drop non-referential spans the model tags as entities:
            // pronouns ("I", "you", "they"), bare determiners, and
            // single-character noise. These resolve to junk entities and
            // pollute the entity graph (and its Mentions edges) without
            // naming a real referent.
            if is_non_referential_span(&span.text) {
                continue;
            }
            if let Some(item) = self.project(span) {
                match item {
                    // GLiNER frequently tags conjoined names ("Alice and
                    // Carol") as a single Person span, collapsing multiple
                    // people into one entity and killing relations between
                    // them. Split those into one mention per name. The split
                    // is conservative (Person-only, ≥2 name-like parts), so
                    // the common single-name case passes through unchanged.
                    ExtractedItem::EntityMention(m) => {
                        for split in split_person_conjunction(m) {
                            items.push(ExtractedItem::EntityMention(split));
                        }
                    }
                    other => items.push(other),
                }
            }
        }
        items
    }

    pub(super) fn project(&self, span: ClassifiedSpan) -> Option<ExtractedItem> {
        match &self.target {
            ExtractorTarget::Entity { .. } | ExtractorTarget::EntityOrStatement => {
                // `span.label` is already the fully-qualified qname — the
                // run() loop remaps the model's simple label back before
                // calling us.
                Some(ExtractedItem::EntityMention(EntityMention {
                    entity_type_qname: span.label,
                    text: span.text,
                    start: span.char_start,
                    end: span.char_end,
                    confidence: span.confidence,
                    extractor_id: self.id.raw(),
                    extractor_version: self.extractor_version,
                }))
            }
            // Statement / Relation classifier targets are not the
            // classifier tier's job — extractors targeting those kinds
            // emit nothing without failing.
            _ => None,
        }
    }
}

/// True for spans that name no real referent and must not become
/// entities: personal/possessive pronouns, bare determiners, and
/// single-character or non-alphabetic noise. Conservative — it only
/// rejects an exact closed-class match (case-insensitive) or a span with
/// no alphabetic character, so real names like "Ian" or "Al" pass.
fn is_non_referential_span(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return true;
    }
    if !t.chars().any(|c| c.is_alphabetic()) {
        return true;
    }
    const STOP: &[&str] = &[
        "i",
        "you",
        "we",
        "they",
        "he",
        "she",
        "it",
        "me",
        "us",
        "him",
        "her",
        "them",
        "myself",
        "yourself",
        "himself",
        "herself",
        "itself",
        "ourselves",
        "yourselves",
        "themselves",
        "my",
        "your",
        "our",
        "their",
        "his",
        "its",
        "this",
        "that",
        "these",
        "those",
        "the",
        "a",
        "an",
        "who",
        "whom",
        "whose",
        "someone",
        "something",
        "anyone",
        "everyone",
        "nobody",
    ];
    let lower = t.to_lowercase();
    STOP.contains(&lower.as_str())
}

/// Split a Person mention whose text is a conjunction of names
/// ("Alice and Carol", "Alice, Bob, and Carol") into one mention per
/// name. Conservative on purpose:
///
/// - Person entities only — conjoined Org/Concept names ("Research and
///   Development") must stay whole, and GLiNER tags those as non-Person,
///   so scoping to Person avoids the false splits.
/// - Only splits when ≥2 non-empty, alphabetic parts result; otherwise
///   the original mention is returned unchanged (the common single-name
///   path allocates nothing extra beyond the one-element Vec).
///
/// Char offsets are best-effort: each part is located in the original
/// span text and offset from the span start; a miss falls back to the
/// whole span range.
fn split_person_conjunction(m: EntityMention) -> Vec<EntityMention> {
    if m.entity_type_qname.rsplit(':').next() != Some("Person") {
        return vec![m];
    }
    let parts: Vec<&str> = m
        .text
        .split([',', '&'])
        .flat_map(|p| p.split(" and "))
        .map(str::trim)
        .filter(|p| !p.is_empty() && p.chars().any(char::is_alphabetic))
        .collect();
    if parts.len() < 2 {
        return vec![m];
    }
    parts
        .into_iter()
        .map(|name| {
            let (start, end) = match m.text.find(name) {
                Some(byte_off) => {
                    let start = m.start + m.text[..byte_off].chars().count();
                    (start, start + name.chars().count())
                }
                None => (m.start, m.end),
            };
            EntityMention {
                entity_type_qname: m.entity_type_qname.clone(),
                text: name.to_string(),
                start,
                end,
                confidence: m.confidence,
                extractor_id: m.extractor_id,
                extractor_version: m.extractor_version,
            }
        })
        .collect()
}

impl Extractor for ClassifierExtractor {
    fn id(&self) -> ExtractorId {
        self.id
    }

    fn kind(&self) -> ExtractorKind {
        ExtractorKind::Classifier
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn extractor_version(&self) -> u32 {
        self.extractor_version
    }

    fn is_wired(&self) -> bool {
        self.model.is_some()
    }

    fn run<'a>(&'a self, ctx: &'a ExtractionContext<'a>, mem: &'a Memory) -> ExtractionFuture<'a> {
        Box::pin(async move {
            let at = ctx.now_unix_nanos;
            let text = mem.text.as_deref().unwrap_or("");

            let Some(model) = self.model.as_ref() else {
                let reason = self
                    .degraded_reason
                    .as_deref()
                    .unwrap_or("classifier model not loaded");
                return ExtractionResult::skipped(ExtractionStatus::SkippedDisabled, reason, at);
            };

            if self.target_labels.is_empty() {
                return ExtractionResult::skipped(
                    ExtractionStatus::SkippedDisabled,
                    "no entity-type labels declared by the active schema",
                    at,
                );
            }

            let (label_owned, qname_by_label) = self.resolve_labels();
            let label_refs: Vec<&str> = label_owned.iter().map(String::as_str).collect();
            let spans = match model.predict(text, &label_refs) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(
                        target: "brain_extractors::classifier",
                        error = %e,
                        "gliner predict failed"
                    );
                    return ExtractionResult::failure(e.to_string(), at, at);
                }
            };

            for s in &spans {
                tracing::debug!(
                    target: "brain_extractors::classifier",
                    label = %s.label,
                    text = %s.text,
                    score = s.confidence,
                    threshold = self.confidence_threshold,
                    accepted = s.confidence >= self.confidence_threshold,
                    "gliner span"
                );
            }

            let items = self.project_spans(spans, &qname_by_label);
            ExtractionResult::success(items, at, at)
        })
    }

    fn run_batch<'a>(
        &'a self,
        ctx: &'a ExtractionContext<'a>,
        mems: &'a [brain_core::Memory],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<ExtractionResult>> + Send + 'a>>
    {
        Box::pin(async move {
            let at = ctx.now_unix_nanos;
            if mems.is_empty() {
                return Vec::new();
            }

            // No model / no labels: every row gets the same skipped
            // result, no model work.
            let Some(model) = self.model.as_ref() else {
                let reason = self
                    .degraded_reason
                    .as_deref()
                    .unwrap_or("classifier model not loaded");
                return mems
                    .iter()
                    .map(|_| {
                        ExtractionResult::skipped(ExtractionStatus::SkippedDisabled, reason, at)
                    })
                    .collect();
            };
            if self.target_labels.is_empty() {
                return mems
                    .iter()
                    .map(|_| {
                        ExtractionResult::skipped(
                            ExtractionStatus::SkippedDisabled,
                            "no entity-type labels declared by the active schema",
                            at,
                        )
                    })
                    .collect();
            }

            let (label_owned, qname_by_label) = self.resolve_labels();
            let label_refs: Vec<&str> = label_owned.iter().map(String::as_str).collect();

            // Hold an owned String for each memory's text so the
            // `&str` refs we hand to predict_batch outlive the call.
            let texts: Vec<&str> = mems
                .iter()
                .map(|m| m.text.as_deref().unwrap_or(""))
                .collect();
            let batch_inputs: Vec<(&str, &[&str])> =
                texts.iter().map(|t| (*t, label_refs.as_slice())).collect();

            let batched = match model.predict_batch(&batch_inputs) {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(
                        target: "brain_extractors::classifier",
                        error = %e,
                        batch_size = mems.len(),
                        "gliner predict_batch failed"
                    );
                    let msg = e.to_string();
                    return mems
                        .iter()
                        .map(|_| ExtractionResult::failure(msg.clone(), at, at))
                        .collect();
                }
            };

            debug_assert_eq!(batched.len(), mems.len());
            let mut out = Vec::with_capacity(mems.len());
            for spans in batched {
                for s in &spans {
                    tracing::debug!(
                        target: "brain_extractors::classifier",
                        label = %s.label,
                        text = %s.text,
                        score = s.confidence,
                        threshold = self.confidence_threshold,
                        accepted = s.confidence >= self.confidence_threshold,
                        "gliner span"
                    );
                }
                let items = self.project_spans(spans, &qname_by_label);
                out.push(ExtractionResult::success(items, at, at));
            }
            out
        })
    }
}

#[cfg(test)]
mod conjunction_tests {
    use super::{is_non_referential_span, split_person_conjunction};
    use crate::framework::item::EntityMention;

    fn mention(qname: &str, text: &str) -> EntityMention {
        EntityMention {
            entity_type_qname: qname.to_string(),
            text: text.to_string(),
            start: 0,
            end: text.chars().count(),
            confidence: 0.9,
            extractor_id: 2,
            extractor_version: 1,
        }
    }

    fn texts(ms: Vec<EntityMention>) -> Vec<String> {
        ms.into_iter().map(|m| m.text).collect()
    }

    #[test]
    fn non_referential_spans_are_rejected() {
        // Pronouns / determiners / non-alpha noise → dropped.
        for junk in [
            "I", "you", "We", "they", "It", "the", "this", "Their", "someone", "  ", "123", "-",
        ] {
            assert!(
                is_non_referential_span(junk),
                "{junk:?} should be rejected as non-referential"
            );
        }
        // Real names — including short ones — pass.
        for name in ["Alice", "Ian", "Al", "Phoenix Project", "Caroline"] {
            assert!(
                !is_non_referential_span(name),
                "{name:?} should pass as a real referent"
            );
        }
    }

    #[test]
    fn splits_two_names() {
        let out = split_person_conjunction(mention("brain:Person", "Alice and Carol"));
        assert_eq!(texts(out), vec!["Alice", "Carol"]);
    }

    #[test]
    fn splits_oxford_list() {
        let out = split_person_conjunction(mention("brain:Person", "Alice, Bob, and Carol"));
        assert_eq!(texts(out), vec!["Alice", "Bob", "Carol"]);
    }

    #[test]
    fn keeps_single_multiword_name() {
        let out = split_person_conjunction(mention("brain:Person", "Priya Sharma"));
        assert_eq!(texts(out), vec!["Priya Sharma"]);
    }

    #[test]
    fn does_not_split_non_person() {
        // A conjoined Org/Concept name must stay whole — the split is
        // Person-scoped precisely to avoid wrecking these.
        let out = split_person_conjunction(mention("brain:Concept", "Research and Development"));
        assert_eq!(texts(out), vec!["Research and Development"]);
    }

    #[test]
    fn offsets_track_each_name() {
        let out = split_person_conjunction(mention("brain:Person", "Alice and Carol"));
        assert_eq!((out[0].start, out[0].end), (0, 5)); // "Alice"
        assert_eq!((out[1].start, out[1].end), (10, 15)); // "Carol"
    }
}
