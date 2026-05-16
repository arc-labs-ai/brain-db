//! BIO label decoding for token-classification NER.
//!
//! Standard CONLL-2003 scheme:
//! - `O` — outside any entity.
//! - `B-X` — beginning of an X-typed span.
//! - `I-X` — inside an X-typed span.
//!
//! Span decoding collapses consecutive `B-X I-X I-X...` (or even a
//! lone `B-X`) into one detection with label `X`.

use std::path::Path;

use crate::extractor::ExtractorError;

/// One BIO-decoded span across a sequence of (token_label, span,
/// confidence) tuples.
#[derive(Debug, Clone, PartialEq)]
pub struct BioSpan {
    pub label: String,
    pub start_token: usize,
    pub end_token: usize, // exclusive
    /// Mean per-token confidence across the span (caller may
    /// override with their own aggregation).
    pub confidence: f32,
}

/// Read `labels.txt` — one label per non-empty line.
pub fn load_labels_file(path: &Path) -> Result<Vec<String>, ExtractorError> {
    let raw = std::fs::read_to_string(path).map_err(|e| ExtractorError::OutputDecodeFailed {
        reason: format!("labels.txt read failed: {e}"),
    })?;
    let labels: Vec<String> = raw
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();
    if labels.is_empty() {
        return Err(ExtractorError::OutputDecodeFailed {
            reason: "labels.txt is empty".into(),
        });
    }
    Ok(labels)
}

/// Decode a BIO-tagged sequence into entity spans.
///
/// `labels` is the per-token label string (e.g., `"B-PER"`,
/// `"I-PER"`, `"O"`). `confidences` is the per-token max softmax
/// probability. Both slices must be the same length.
pub fn decode_bio(labels: &[&str], confidences: &[f32]) -> Vec<BioSpan> {
    assert_eq!(
        labels.len(),
        confidences.len(),
        "decode_bio: labels and confidences must align"
    );
    let mut out = Vec::new();
    let mut cur_label: Option<String> = None;
    let mut cur_start: usize = 0;
    let mut cur_conf_sum: f32 = 0.0;
    let mut cur_count: usize = 0;

    for (i, lab) in labels.iter().enumerate() {
        let (tag, body) = split_bio(lab);
        match tag {
            BioTag::B => {
                if let Some(lbl) = cur_label.take() {
                    out.push(BioSpan {
                        label: lbl,
                        start_token: cur_start,
                        end_token: i,
                        confidence: if cur_count == 0 {
                            0.0
                        } else {
                            cur_conf_sum / cur_count as f32
                        },
                    });
                }
                cur_label = Some(body.to_string());
                cur_start = i;
                cur_conf_sum = confidences[i];
                cur_count = 1;
            }
            BioTag::I => {
                if let Some(ref lbl) = cur_label {
                    if lbl == body {
                        // Continue the current span.
                        cur_conf_sum += confidences[i];
                        cur_count += 1;
                    } else {
                        // I-X with mismatched body or no open span:
                        // treat as a new B-X (common CONLL practice;
                        // tagger output errors heal here).
                        out.push(BioSpan {
                            label: lbl.clone(),
                            start_token: cur_start,
                            end_token: i,
                            confidence: cur_conf_sum / cur_count as f32,
                        });
                        cur_label = Some(body.to_string());
                        cur_start = i;
                        cur_conf_sum = confidences[i];
                        cur_count = 1;
                    }
                } else {
                    // Stray I-X — treat as B-X.
                    cur_label = Some(body.to_string());
                    cur_start = i;
                    cur_conf_sum = confidences[i];
                    cur_count = 1;
                }
            }
            BioTag::O => {
                if let Some(lbl) = cur_label.take() {
                    out.push(BioSpan {
                        label: lbl,
                        start_token: cur_start,
                        end_token: i,
                        confidence: if cur_count == 0 {
                            0.0
                        } else {
                            cur_conf_sum / cur_count as f32
                        },
                    });
                }
                cur_conf_sum = 0.0;
                cur_count = 0;
            }
        }
    }
    // Flush a trailing open span.
    if let Some(lbl) = cur_label.take() {
        out.push(BioSpan {
            label: lbl,
            start_token: cur_start,
            end_token: labels.len(),
            confidence: if cur_count == 0 {
                0.0
            } else {
                cur_conf_sum / cur_count as f32
            },
        });
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BioTag {
    B,
    I,
    O,
}

fn split_bio(lab: &str) -> (BioTag, &str) {
    if lab == "O" {
        return (BioTag::O, "");
    }
    if let Some(rest) = lab.strip_prefix("B-") {
        return (BioTag::B, rest);
    }
    if let Some(rest) = lab.strip_prefix("I-") {
        return (BioTag::I, rest);
    }
    // Some checkpoints emit bare labels (e.g., "PER"); treat them
    // as B-PER for span purposes.
    (BioTag::B, lab)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bio_decoder_collapses_per_span() {
        let labels = vec!["B-PER", "I-PER", "I-PER"];
        let confs = vec![0.9, 0.8, 0.85];
        let spans = decode_bio(&labels, &confs);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].label, "PER");
        assert_eq!(spans[0].start_token, 0);
        assert_eq!(spans[0].end_token, 3);
        // Mean: (0.9 + 0.8 + 0.85) / 3 = 0.85
        assert!((spans[0].confidence - 0.85).abs() < 1e-5);
    }

    #[test]
    fn bio_decoder_handles_o_label() {
        let labels = vec!["B-PER", "O", "B-PER"];
        let confs = vec![0.9, 0.5, 0.95];
        let spans = decode_bio(&labels, &confs);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].label, "PER");
        assert_eq!((spans[0].start_token, spans[0].end_token), (0, 1));
        assert_eq!((spans[1].start_token, spans[1].end_token), (2, 3));
    }

    #[test]
    fn bio_decoder_handles_bare_b_tag() {
        let labels = vec!["O", "B-PER", "O"];
        let confs = vec![0.5, 0.9, 0.5];
        let spans = decode_bio(&labels, &confs);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].label, "PER");
        assert_eq!((spans[0].start_token, spans[0].end_token), (1, 2));
    }

    #[test]
    fn bio_decoder_promotes_stray_i_to_b() {
        // No prior B-X — a stray I-PER is treated as a fresh span.
        let labels = vec!["O", "I-PER", "I-PER"];
        let confs = vec![0.5, 0.9, 0.85];
        let spans = decode_bio(&labels, &confs);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].label, "PER");
        assert_eq!((spans[0].start_token, spans[0].end_token), (1, 3));
    }

    #[test]
    fn bio_decoder_handles_label_switch_mid_span() {
        // I-LOC mid-PER-span splits into two adjacent spans.
        let labels = vec!["B-PER", "I-PER", "I-LOC"];
        let confs = vec![0.9, 0.8, 0.7];
        let spans = decode_bio(&labels, &confs);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].label, "PER");
        assert_eq!((spans[0].start_token, spans[0].end_token), (0, 2));
        assert_eq!(spans[1].label, "LOC");
        assert_eq!((spans[1].start_token, spans[1].end_token), (2, 3));
    }

    #[test]
    fn bio_decoder_handles_trailing_span() {
        // Span extends to the end of the sequence.
        let labels = vec!["O", "B-PER", "I-PER"];
        let confs = vec![0.5, 0.9, 0.85];
        let spans = decode_bio(&labels, &confs);
        assert_eq!(spans.len(), 1);
        assert_eq!((spans[0].start_token, spans[0].end_token), (1, 3));
    }

    #[test]
    fn labels_load_from_text_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("labels.txt");
        std::fs::write(&p, "O\nB-PER\nI-PER\n\nB-ORG\nI-ORG\n").unwrap();
        let labels = load_labels_file(&p).unwrap();
        assert_eq!(labels, vec!["O", "B-PER", "I-PER", "B-ORG", "I-ORG"]);
    }

    #[test]
    fn labels_load_empty_errors() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("labels.txt");
        std::fs::write(&p, "\n\n").unwrap();
        let err = load_labels_file(&p).unwrap_err();
        assert!(matches!(err, ExtractorError::OutputDecodeFailed { .. }));
    }
}
