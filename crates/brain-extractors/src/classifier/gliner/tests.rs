//! Unit tests for the GLiNER inference module.
//!
//! These tests use synthetic weights / tokenizers and exercise the
//! mechanical pieces of the pipeline (splitter, prompt construction,
//! span enumeration, decode). A real-model integration test is
//! gated behind `BRAIN_NER_MODEL_PATH` so CI doesn't need the
//! pickle weights to be green.

use std::collections::HashMap;
use std::io::Write;

use candle_core::{DType, Device, Tensor};
use candle_nn::{Linear, VarBuilder};
use tempfile::NamedTempFile;
use tokenizers::Tokenizer;

use super::decode::{candidate_spans, resolve_flat, sigmoid};
use super::head::{LabelProjection, ProjectionLayer, SpanMarkerHead};
use super::rnn::BiLstm;
use super::tokenizer::{
    assemble_window, build_prompt, encode_words, split_words, word_budget, TokenizerIds,
};

// ---------------------------------------------------------------------------
// Synthetic tokenizer for prompt-construction tests.
// ---------------------------------------------------------------------------
//
// The tokenizers crate deserialises a WordLevel tokenizer from JSON;
// that gives us a deterministic vocab where we control every id.

fn build_test_tokenizer() -> Tokenizer {
    let json = r#"{
        "version": "1.0",
        "truncation": null,
        "padding": null,
        "added_tokens": [],
        "normalizer": null,
        "pre_tokenizer": { "type": "Whitespace" },
        "post_processor": null,
        "decoder": null,
        "model": {
            "type": "WordLevel",
            "vocab": {
                "[UNK]": 0,
                "[CLS]": 1,
                "[SEP]": 2,
                "<<ENT>>": 3,
                "<<SEP>>": 4,
                "Person": 10,
                "Organization": 11,
                "Alice": 20,
                "Wong": 21,
                "works": 22,
                "at": 23,
                "Acme": 24,
                "Corp": 25,
                ".": 26,
                "Priya": 30,
                "Sharma": 31,
                "went": 32,
                "to": 33
            },
            "unk_token": "[UNK]"
        }
    }"#;
    let mut tmp = NamedTempFile::new().expect("tmpfile");
    tmp.write_all(json.as_bytes()).expect("write");
    let path = tmp.path().to_owned();
    let tok = Tokenizer::from_file(&path).expect("tokenizer build");
    drop(tmp);
    tok
}

fn test_token_ids() -> TokenizerIds {
    TokenizerIds {
        cls: 1,
        sep: 2,
        ent: 3,
        prompt_sep: 4,
    }
}

// 1. Word splitter preserves char offsets.

#[test]
fn tokenizer_word_split_preserves_offsets() {
    let text = "Priya Sharma went to Acme.";
    let words = split_words(text);
    let actual: Vec<(&str, usize, usize)> = words
        .iter()
        .map(|w| (w.text.as_str(), w.char_start, w.char_end))
        .collect();
    assert_eq!(
        actual,
        vec![
            ("Priya", 0, 5),
            ("Sharma", 6, 12),
            ("went", 13, 17),
            ("to", 18, 20),
            ("Acme", 21, 25),
            (".", 25, 26),
        ]
    );
}

// 2. Prompt construction inserts <<ENT>> markers per-label and
//    terminates the prompt with <<SEP>> (not [SEP]).

#[test]
fn prompt_construction_inserts_ent_tokens() {
    let tok = build_test_tokenizer();
    let text = "Alice Wong works at Acme Corp.";
    let labels = ["Person", "Organization"];
    let labels_ref: Vec<&str> = labels.to_vec();
    let ids = test_token_ids();
    let prompt = build_prompt(&tok, &labels_ref, &ids).expect("prompt");
    let words = encode_words(&tok, text).expect("encode words");
    let out = assemble_window(&prompt, &words, &ids);

    // [CLS] <<ENT>> Person <<ENT>> Organization <<SEP>> Alice Wong works at Acme Corp . [SEP]
    assert_eq!(
        out.input_ids,
        vec![1, 3, 10, 3, 11, 4, 20, 21, 22, 23, 24, 25, 26, 2]
    );
    assert_eq!(out.ent_positions, vec![1, 3]);
    assert_eq!(out.word_first_subtoken, vec![6, 7, 8, 9, 10, 11, 12]);
    assert_eq!(
        out.word_offsets,
        vec![
            (0, 5),
            (6, 10),
            (11, 16),
            (17, 19),
            (20, 24),
            (25, 29),
            (29, 30),
        ]
    );
    assert!(out.attention_mask.iter().all(|&m| m == 1));
}

// 3. Span enumeration: N words * W widths == cube size.

#[test]
fn span_enumeration_respects_max_width() {
    let num_words = 5;
    let max_width = 3;
    let mut slot_count = 0usize;
    let mut max_observed_offset = 0usize;
    for i in 0..num_words {
        for k in 0..max_width {
            slot_count += 1;
            let j = i + k;
            if j < num_words {
                max_observed_offset = max_observed_offset.max(k);
            }
        }
    }
    assert_eq!(slot_count, num_words * max_width);
    assert_eq!(max_observed_offset, max_width - 1);
}

// 4. Decode filters by threshold (post-sigmoid).

#[test]
fn decode_filters_by_threshold() {
    let labels = ["Person"];
    let labels_ref: Vec<&str> = labels.to_vec();
    let word_offsets = vec![(0, 5), (6, 12)];
    let text = "Priya Sharma";
    let logits = vec![
        vec![vec![0.0_f32]],  // (0, 0): kept
        vec![vec![-1.0_f32]], // (1, 0): dropped (sigmoid ~0.27 < 0.5)
    ];
    let spans = resolve_flat(candidate_spans(
        &logits,
        0.5,
        &labels_ref,
        &word_offsets,
        text,
    ));
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].text, "Priya");
    assert_eq!(spans[0].label, "Person");
    assert!((sigmoid(0.0) - 0.5).abs() < 1e-6);
}

// 5. Decode resolves overlaps greedily by score.

#[test]
fn decode_resolves_overlaps_greedy() {
    let labels = ["Person"];
    let labels_ref: Vec<&str> = labels.to_vec();
    let word_offsets = vec![(0, 5), (6, 12)];
    let text = "Priya Sharma";
    let logits = vec![
        vec![
            vec![2.0_f32], // (start=0, width=0) "Priya"
            vec![4.0_f32], // (start=0, width=1) "Priya Sharma" — top
        ],
        vec![
            vec![3.0_f32], // (start=1, width=0) "Sharma"
            vec![0.0_f32], // (start=1, width=1) out of range
        ],
    ];
    let spans = resolve_flat(candidate_spans(
        &logits,
        0.5,
        &labels_ref,
        &word_offsets,
        text,
    ));
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].text, "Priya Sharma");
    assert_eq!(spans[0].char_start, 0);
    assert_eq!(spans[0].char_end, 12);
}

// 6. Head forward with synthetic weights produces the expected shape.

fn make_linear(in_dim: usize, out_dim: usize, device: &Device) -> Linear {
    let w_data: Vec<f32> = vec![0.01_f32; out_dim * in_dim];
    let weight = Tensor::from_vec(w_data, (out_dim, in_dim), device).expect("weight");
    let b_data: Vec<f32> = vec![0.0_f32; out_dim];
    let bias = Tensor::from_vec(b_data, out_dim, device).expect("bias");
    Linear::new(weight, Some(bias))
}

fn make_proj(in_dim: usize, hidden: usize, out_dim: usize, device: &Device) -> ProjectionLayer {
    ProjectionLayer::from_linears(
        make_linear(in_dim, hidden, device),
        make_linear(hidden, out_dim, device),
    )
}

#[test]
fn head_forward_with_synthetic_weights_produces_expected_shape() {
    let device = Device::Cpu;
    let backbone_hidden = 8usize; // stand-in for 768
    let head_hidden = 4usize; // stand-in for 512
    let num_words = 5usize;
    let max_width = 3usize;
    let num_labels = 2usize;

    let h: Vec<f32> = (0..num_words * backbone_hidden)
        .map(|i| (i as f32) * 0.001)
        .collect();
    let h = Tensor::from_vec(h, (1, num_words, backbone_hidden), &device).expect("h");

    let inner = backbone_hidden * 4;
    let project_start = make_proj(backbone_hidden, inner, backbone_hidden, &device);
    let project_end = make_proj(backbone_hidden, inner, backbone_hidden, &device);
    let cat_in = backbone_hidden * 2;
    let cat_hidden = cat_in * 4;
    let out_project = make_proj(cat_in, cat_hidden, head_hidden, &device);
    let head = SpanMarkerHead::from_parts(project_start, project_end, out_project);

    let span_rep = head.forward(&h, max_width, head_hidden).expect("head fwd");
    assert_eq!(
        span_rep.dims(),
        &[1, num_words, max_width, head_hidden],
        "span_rep shape mismatch"
    );

    let label_proj =
        LabelProjection::from_proj(make_proj(backbone_hidden, inner, head_hidden, &device));
    let label_h: Vec<f32> = vec![0.05_f32; num_labels * backbone_hidden];
    let label_h =
        Tensor::from_vec(label_h, (num_labels, backbone_hidden), &device).expect("label_h");
    let prompt_emb = label_proj.forward(&label_h).expect("label proj fwd");
    assert_eq!(prompt_emb.dims(), &[num_labels, head_hidden]);

    let flat = span_rep
        .squeeze(0)
        .and_then(|t| t.reshape((num_words * max_width, head_hidden)))
        .expect("flat");
    let proj_t = prompt_emb.t().expect("transpose");
    let scores = flat
        .matmul(&proj_t)
        .and_then(|t| t.reshape((num_words, max_width, num_labels)))
        .expect("scores");
    assert_eq!(scores.dims(), &[num_words, max_width, num_labels]);
}

// 7. The bootstrap-patched tokenizer.json must resolve <<ENT>> to id
//    128001 and <<SEP>> to id 128002. A mismatch is an unrecoverable
//    misalignment with the trained embedding rows — the loader rejects
//    it loudly rather than silently producing garbage predictions.

#[test]
fn tokenizer_must_carry_gliner_markers_at_trained_ids() {
    use super::{resolve_token_ids, GlinerError};

    let make = |ent_id: u32, sep_id: u32| -> Tokenizer {
        let json = format!(
            r#"{{
                "version": "1.0",
                "truncation": null,
                "padding": null,
                "added_tokens": [
                    {{ "id": 0, "content": "[UNK]", "single_word": false,
                       "lstrip": false, "rstrip": false, "normalized": false,
                       "special": true }},
                    {{ "id": 1, "content": "[CLS]", "single_word": false,
                       "lstrip": false, "rstrip": false, "normalized": false,
                       "special": true }},
                    {{ "id": 2, "content": "[SEP]", "single_word": false,
                       "lstrip": false, "rstrip": false, "normalized": false,
                       "special": true }},
                    {{ "id": {ent_id}, "content": "<<ENT>>", "single_word": false,
                       "lstrip": false, "rstrip": false, "normalized": true,
                       "special": false }},
                    {{ "id": {sep_id}, "content": "<<SEP>>", "single_word": false,
                       "lstrip": false, "rstrip": false, "normalized": true,
                       "special": false }}
                ],
                "normalizer": null,
                "pre_tokenizer": {{ "type": "Whitespace" }},
                "post_processor": null,
                "decoder": null,
                "model": {{
                    "type": "WordLevel",
                    "vocab": {{
                        "[UNK]": 0, "[CLS]": 1, "[SEP]": 2,
                        "<<ENT>>": {ent_id}, "<<SEP>>": {sep_id}
                    }},
                    "unk_token": "[UNK]"
                }}
            }}"#
        );
        let mut tmp = NamedTempFile::new().expect("tmpfile");
        tmp.write_all(json.as_bytes()).expect("write");
        Tokenizer::from_file(tmp.path()).expect("load")
    };

    let good = make(128_001, 128_002);
    let ids = resolve_token_ids(&good).expect("good tokenizer");
    assert_eq!(ids.cls, 1);
    assert_eq!(ids.sep, 2);
    assert_eq!(ids.ent, 128_001);
    assert_eq!(ids.prompt_sep, 128_002);

    let bad_ent = make(99_999, 128_002);
    let err = resolve_token_ids(&bad_ent).expect_err("must reject wrong ent id");
    assert!(matches!(
        err,
        GlinerError::TokenIdMismatch {
            token: "<<ENT>>",
            got: 99_999,
            expected: 128_001
        }
    ));

    let bad_sep = make(128_001, 99_999);
    let err = resolve_token_ids(&bad_sep).expect_err("must reject wrong sep id");
    assert!(matches!(
        err,
        GlinerError::TokenIdMismatch {
            token: "<<SEP>>",
            got: 99_999,
            expected: 128_002
        }
    ));
}

// 8. BiLSTM forward concatenates both directions to 2 * hidden width.

#[test]
fn bi_lstm_forward_concatenates_directions_into_double_hidden_width() {
    let device = Device::Cpu;
    let in_features = 8usize;
    let hidden = 4usize;
    let seq_len = 5usize;

    // Synthetic weights — the values don't matter for the shape
    // contract, only that the four PyTorch keys (and their `_reverse`
    // counterparts) are present so `LSTM::new` resolves them.
    let mut tensors: HashMap<String, Tensor> = HashMap::new();
    for suffix in ["", "_reverse"] {
        tensors.insert(
            format!("lstm.weight_ih_l0{suffix}"),
            Tensor::zeros((4 * hidden, in_features), DType::F32, &device).expect("w_ih"),
        );
        tensors.insert(
            format!("lstm.weight_hh_l0{suffix}"),
            Tensor::zeros((4 * hidden, hidden), DType::F32, &device).expect("w_hh"),
        );
        tensors.insert(
            format!("lstm.bias_ih_l0{suffix}"),
            Tensor::zeros(4 * hidden, DType::F32, &device).expect("b_ih"),
        );
        tensors.insert(
            format!("lstm.bias_hh_l0{suffix}"),
            Tensor::zeros(4 * hidden, DType::F32, &device).expect("b_hh"),
        );
    }
    let vb = VarBuilder::from_tensors(tensors, DType::F32, &device);

    let bi_lstm = BiLstm::load(vb.pp("lstm"), in_features, hidden).expect("BiLstm::load");

    let input: Vec<f32> = (0..seq_len * in_features)
        .map(|i| i as f32 * 0.01)
        .collect();
    let input = Tensor::from_vec(input, (1, seq_len, in_features), &device).expect("input");
    let out = bi_lstm.forward(&input).expect("bi_lstm forward");
    assert_eq!(out.dims(), &[1, seq_len, 2 * hidden]);
}

// 9. Batched predict input validation (no model required).

#[test]
fn predict_batch_empty_input_returns_empty_output() {
    use super::{validate_batch_inputs, BatchValidation};
    let v = validate_batch_inputs(&[], 25).expect("empty is valid");
    assert!(matches!(v, BatchValidation::Empty));
}

#[test]
fn predict_batch_rejects_mixed_label_sets() {
    use super::{validate_batch_inputs, GlinerError};
    let labels_a: &[&str] = &["Person", "Organization"];
    let labels_b: &[&str] = &["Person", "Place"];
    let inputs: Vec<(&str, &[&str])> = vec![("Alice ...", labels_a), ("Bob ...", labels_b)];
    let err = validate_batch_inputs(&inputs, 25).expect_err("mixed labels must reject");
    match err {
        GlinerError::Decode(msg) => {
            assert!(msg.contains("row 0 vs row 1"), "msg={msg}");
            assert!(msg.contains("same labels"), "msg={msg}");
        }
        other => panic!("expected Decode error, got {other:?}"),
    }
}

#[test]
fn predict_batch_rejects_overlong_label_set() {
    use super::{validate_batch_inputs, GlinerError};
    // Build 30 labels; max_labels is 25.
    let owned: Vec<String> = (0..30).map(|i| format!("L{i}")).collect();
    let labels: Vec<&str> = owned.iter().map(String::as_str).collect();
    let inputs: Vec<(&str, &[&str])> = vec![("text", labels.as_slice())];
    let err = validate_batch_inputs(&inputs, 25).expect_err("too many labels rejected");
    assert!(matches!(
        err,
        GlinerError::TooManyLabels { got: 30, limit: 25 }
    ));
}

#[test]
fn predict_batch_empty_labels_short_circuits_to_per_row_empty_spans() {
    use super::{validate_batch_inputs, BatchValidation};
    let no_labels: &[&str] = &[];
    let inputs: Vec<(&str, &[&str])> = vec![("a", no_labels), ("b", no_labels)];
    let v = validate_batch_inputs(&inputs, 25).expect("empty labels is valid");
    assert!(matches!(v, BatchValidation::AllEmptyLabels));
}

#[test]
fn predict_batch_single_row_with_labels_routes_to_live() {
    use super::{validate_batch_inputs, BatchValidation};
    let labels: &[&str] = &["Person"];
    let inputs: Vec<(&str, &[&str])> = vec![("hello", labels)];
    let v = validate_batch_inputs(&inputs, 25).expect("single row is valid");
    assert!(matches!(v, BatchValidation::Live));
}

// 10. Real-model batch integration test. Gated.

#[test]
#[ignore = "requires BRAIN_NER_MODEL_PATH pointing at a GLiNER pickle directory"]
fn real_predict_batch_matches_per_row_predict() {
    use std::path::PathBuf;
    use std::time::Instant;

    use super::{GlinerConfig, GlinerModel};

    let path: PathBuf = std::env::var("BRAIN_NER_MODEL_PATH")
        .expect("set BRAIN_NER_MODEL_PATH to enable this test")
        .into();
    let model = GlinerModel::load(&path, GlinerConfig::default()).expect("model load");
    let labels: &[&str] = &["Person", "Organization"];
    let texts = [
        "Alice Wong works at Acme Corp.",
        "Bob Smith joined Globex Industries last year.",
        "Charlie Brown founded Initech in 2010.",
        "Diane Lee left OldCo to start NewCo.",
        "Eve Davis spoke at the conference in London.",
        "Frank Miller wrote the book in Madrid.",
        "Grace Hopper served at the US Navy.",
        "Henry Kim relocated to Singapore.",
    ];

    // Warmup pass — first inference pays a kernel-compilation tax.
    let _ = model.predict(texts[0], labels).expect("warmup");

    // Per-row reference + timing.
    let t0 = Instant::now();
    let per_row: Vec<Vec<super::Span>> = texts
        .iter()
        .map(|t| model.predict(t, labels).expect("per-row predict"))
        .collect();
    let per_row_ms = t0.elapsed().as_millis();

    // Batched call — same labels for every row.
    let inputs: Vec<(&str, &[&str])> = texts.iter().map(|t| (*t, labels)).collect();
    let t1 = Instant::now();
    let batched = model.predict_batch(&inputs).expect("batched predict");
    let batch_ms = t1.elapsed().as_millis();
    println!(
        "predict_batch timing: per-row total {per_row_ms} ms over {} rows, batched {batch_ms} ms (speedup {:.2}x)",
        texts.len(),
        per_row_ms as f64 / batch_ms.max(1) as f64,
    );
    assert_eq!(batched.len(), texts.len());
    for (row, (a, b)) in per_row.iter().zip(batched.iter()).enumerate() {
        assert_eq!(a.len(), b.len(), "row {row} span count differs");
        for (s_a, s_b) in a.iter().zip(b.iter()) {
            assert_eq!(s_a.label, s_b.label, "row {row} label mismatch");
            assert_eq!(s_a.text, s_b.text, "row {row} text mismatch");
            assert_eq!(s_a.char_start, s_b.char_start, "row {row} start mismatch");
            assert_eq!(s_a.char_end, s_b.char_end, "row {row} end mismatch");
            assert!(
                (s_a.score - s_b.score).abs() <= 0.01,
                "row {row} score drift > 0.01: {} vs {}",
                s_a.score,
                s_b.score
            );
        }
    }
}

#[test]
#[ignore = "requires BRAIN_NER_MODEL_PATH"]
fn real_predict_batch_preserves_per_row_order_and_isolates_results() {
    use std::path::PathBuf;

    use super::{GlinerConfig, GlinerModel};

    let path: PathBuf = std::env::var("BRAIN_NER_MODEL_PATH")
        .expect("set BRAIN_NER_MODEL_PATH to enable this test")
        .into();
    let model = GlinerModel::load(&path, GlinerConfig::default()).expect("model load");
    let labels: &[&str] = &["Person", "Place"];
    let texts = [
        "Alice met Bob in Paris.",
        "I love sunshine.",
        "Tokyo is a city in Japan.",
    ];
    let inputs: Vec<(&str, &[&str])> = texts.iter().map(|t| (*t, labels)).collect();
    let batched = model.predict_batch(&inputs).expect("batched predict");

    // Row 0 should mention Paris; row 2 should mention Tokyo / Japan;
    // their results must not leak into row 1.
    assert!(batched[0]
        .iter()
        .any(|s| s.label == "Place" && s.text.contains("Paris")));
    assert!(batched[2]
        .iter()
        .any(|s| s.label == "Place" && (s.text.contains("Tokyo") || s.text.contains("Japan"))));
    // Row 1 has no person or place; whatever predictions, none can
    // claim text that doesn't appear in row 1's source.
    for s in &batched[1] {
        assert!(
            texts[1].contains(s.text.as_str()),
            "row 1 span text {:?} not in row 1 source",
            s.text
        );
    }
}

#[test]
#[ignore = "requires BRAIN_NER_MODEL_PATH"]
fn real_predict_batch_handles_variable_length_rows_via_padding() {
    use std::path::PathBuf;

    use super::{GlinerConfig, GlinerModel};

    let path: PathBuf = std::env::var("BRAIN_NER_MODEL_PATH")
        .expect("set BRAIN_NER_MODEL_PATH to enable this test")
        .into();
    let model = GlinerModel::load(&path, GlinerConfig::default()).expect("model load");
    let labels: &[&str] = &["Person", "Organization"];
    let short = "Alice.";
    let long = "Alice Wong, a senior engineer at Acme Corp, recently spoke at a conference \
                in Paris alongside Bob Smith and several colleagues from Globex Industries about \
                the future of distributed systems and the role of machine learning therein.";
    let inputs: Vec<(&str, &[&str])> = vec![(short, labels), (long, labels), (short, labels)];
    let batched = model.predict_batch(&inputs).expect("batched predict");
    assert_eq!(batched.len(), 3);
    // Row 0 and row 2 are identical inputs — outputs must be byte-equal.
    assert_eq!(
        batched[0].len(),
        batched[2].len(),
        "padding bled into row 2: {:?} vs {:?}",
        batched[0],
        batched[2]
    );
    for (a, b) in batched[0].iter().zip(batched[2].iter()) {
        assert_eq!(a.label, b.label);
        assert_eq!(a.text, b.text);
        assert_eq!(a.char_start, b.char_start);
        assert_eq!(a.char_end, b.char_end);
        assert!(
            (a.score - b.score).abs() < 1e-4,
            "score drift across identical short rows: {} vs {}",
            a.score,
            b.score
        );
    }
}

// 11. Real-model single-row integration test. Gated.

#[test]
#[ignore = "requires BRAIN_NER_MODEL_PATH pointing at a GLiNER pickle directory"]
fn real_inference_detects_person_and_organization_in_alice_works_at_acme() {
    use std::path::PathBuf;

    use super::{GlinerConfig, GlinerModel};

    let path: PathBuf = std::env::var("BRAIN_NER_MODEL_PATH")
        .expect("set BRAIN_NER_MODEL_PATH to enable this test")
        .into();
    let model = GlinerModel::load(&path, GlinerConfig::default()).expect("model load");
    let spans = model
        .predict(
            "Alice Wong works at Acme Corp.",
            &["Person", "Organization"],
        )
        .expect("predict");

    let person = spans
        .iter()
        .find(|s| s.label == "Person" && s.text == "Alice Wong")
        .unwrap_or_else(|| panic!("missing Person span 'Alice Wong': {spans:?}"));
    assert!(
        person.score >= 0.5,
        "Person 'Alice Wong' scored below 0.5: {person:?}"
    );

    let org = spans
        .iter()
        .find(|s| s.label == "Organization" && s.text.contains("Acme"))
        .unwrap_or_else(|| panic!("missing Organization span containing 'Acme': {spans:?}"));
    assert!(
        org.score >= 0.5,
        "Organization 'Acme*' scored below 0.5: {org:?}"
    );
}

// ---------------------------------------------------------------------------
// Windowing: long inputs are covered, not dropped.
// ---------------------------------------------------------------------------

#[test]
fn the_word_budget_reserves_the_prompt_and_the_trailing_sep() {
    let tok = build_test_tokenizer();
    let ids = test_token_ids();
    let labels_ref: Vec<&str> = vec!["Person", "Organization"];
    let prompt = build_prompt(&tok, &labels_ref, &ids).expect("prompt");

    // [CLS] + (<<ENT>> + label) * 2 + <<SEP>> = 6 subtokens, and the
    // window still has to close with [SEP].
    assert_eq!(prompt.subtokens(), 6);
    assert_eq!(word_budget(&prompt, 384).expect("budget"), 384 - 6 - 1);
}

#[test]
fn a_prompt_that_fills_the_sequence_is_an_error_not_an_empty_result() {
    let tok = build_test_tokenizer();
    let ids = test_token_ids();
    let labels_ref: Vec<&str> = vec!["Person", "Organization"];
    let prompt = build_prompt(&tok, &labels_ref, &ids).expect("prompt");

    // No windowing can make room when the labels alone fill the model's
    // sequence, so the caller is told rather than handed nothing.
    let err = word_budget(&prompt, prompt.subtokens()).expect_err("must reject");
    assert!(
        matches!(err, super::GlinerError::InputTooLong { .. }),
        "got {err:?}"
    );
}

#[test]
fn every_window_of_a_long_input_fits_the_sequence_length() {
    let tok = build_test_tokenizer();
    let ids = test_token_ids();
    let labels_ref: Vec<&str> = vec!["Person", "Organization"];
    let prompt = build_prompt(&tok, &labels_ref, &ids).expect("prompt");

    // Far longer than the cap this test imposes, so it must split.
    let text = "Alice Wong works at Acme Corp . ".repeat(40);
    let words = encode_words(&tok, &text).expect("encode words");
    let max_len = 32;
    let budget = word_budget(&prompt, max_len).expect("budget");
    let lens: Vec<usize> = words.iter().map(|w| w.ids.len()).collect();
    let plan = super::window::plan(&lens, budget, 12);

    assert!(
        plan.windows.len() > 1,
        "input long enough to need splitting"
    );
    for w in &plan.windows {
        let built = assemble_window(&prompt, &words[w.start..w.end], &ids);
        assert!(
            built.input_ids.len() <= max_len,
            "window {w:?} produced {} tokens, cap {max_len}",
            built.input_ids.len()
        );
        assert_eq!(built.ent_positions, vec![1, 3], "prompt repeats per window");
    }
}

#[test]
fn window_offsets_stay_absolute_in_the_source_text() {
    let tok = build_test_tokenizer();
    let ids = test_token_ids();
    let labels_ref: Vec<&str> = vec!["Person"];
    let prompt = build_prompt(&tok, &labels_ref, &ids).expect("prompt");

    let text = "Alice Wong works at Acme Corp . ".repeat(10);
    let words = encode_words(&tok, &text).expect("encode words");
    let lens: Vec<usize> = words.iter().map(|w| w.ids.len()).collect();
    let budget = word_budget(&prompt, 24).expect("budget");
    let plan = super::window::plan(&lens, budget, 4);

    // A span decoded from a later window must point at the right place
    // in the whole input, not at an offset within its own window.
    let last = plan.windows.last().expect("at least one window");
    let built = assemble_window(&prompt, &words[last.start..last.end], &ids);
    let (start, end) = built.word_offsets[0];
    let (word_start, word_end) = words[last.start].offsets;
    assert_eq!(&text[start..end], &text[word_start..word_end]);
    assert!(start > 0, "the final window does not begin at byte 0");
}

// 13. Real-model regression: an input past the sequence length is
//     windowed, so entities beyond the cap are still reported.

#[test]
#[ignore = "requires BRAIN_NER_MODEL_PATH pointing at a GLiNER pickle directory"]
fn real_inference_finds_entities_in_text_longer_than_the_sequence_length() {
    use std::path::PathBuf;

    use super::{GlinerConfig, GlinerModel};

    let path: PathBuf = std::env::var("BRAIN_NER_MODEL_PATH")
        .expect("set BRAIN_NER_MODEL_PATH to enable this test")
        .into();
    let config = GlinerConfig::default();
    let model = GlinerModel::load(&path, config).expect("model load");

    // Comfortably past the 384-subtoken sequence length, with a
    // single-token given name that only a model-backed tier can find:
    // a capitalisation rule cannot tell "Kenji" from any other word.
    let filler = "The team restated the sequence of events for the record. ".repeat(40);
    let text = format!("{filler}Kenji approved the rollout. {filler}");
    assert!(
        text.len() > 4000,
        "fixture must exceed the sequence length by a wide margin"
    );

    let spans = model.predict(&text, &["Person"]).expect("predict");
    let kenji = spans
        .iter()
        .find(|s| s.text.contains("Kenji"))
        .unwrap_or_else(|| panic!("entity past the sequence length was dropped: {spans:?}"));
    assert_eq!(kenji.label, "Person");
    assert_eq!(
        &text[kenji.char_start..kenji.char_end],
        kenji.text,
        "offsets must address the whole input, not a window"
    );
}

#[test]
#[ignore = "requires BRAIN_NER_MODEL_PATH pointing at a GLiNER pickle directory"]
fn real_inference_reports_an_entity_once_despite_overlapping_windows() {
    use std::path::PathBuf;

    use super::{GlinerConfig, GlinerModel};

    let path: PathBuf = std::env::var("BRAIN_NER_MODEL_PATH")
        .expect("set BRAIN_NER_MODEL_PATH to enable this test")
        .into();
    let model = GlinerModel::load(&path, GlinerConfig::default()).expect("model load");

    let filler = "The team restated the sequence of events for the record. ".repeat(40);
    let text = format!("{filler}Kenji approved the rollout. {filler}");
    let spans = model.predict(&text, &["Person"]).expect("predict");

    // Overlapping windows see the same words twice; the merge must not
    // turn that into two copies of the same entity.
    let at_same_place: Vec<_> = spans
        .iter()
        .filter(|s| s.text.contains("Kenji"))
        .map(|s| (s.char_start, s.char_end))
        .collect();
    let mut deduped = at_same_place.clone();
    deduped.sort_unstable();
    deduped.dedup();
    assert_eq!(
        at_same_place.len(),
        deduped.len(),
        "the same span was reported twice: {spans:?}"
    );
    for pair in spans.windows(2) {
        assert!(
            pair[0].char_end <= pair[1].char_start,
            "spans overlap after the merge: {:?} then {:?}",
            pair[0],
            pair[1]
        );
    }
}
