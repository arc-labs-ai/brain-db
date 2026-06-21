//! GLiNER v2.1 zero-shot NER inference.
//!
//! Implements [`urchade/gliner_small-v2.1`](https://huggingface.co/urchade/gliner_small-v2.1)
//! end-to-end: a DeBERTa-v3-small backbone (hidden = 768) plus a
//! `markerV0` span-representation head and a label-projection MLP,
//! followed by sigmoid + greedy flat-NER decoding.
//!
//! The module is fully self-contained: callers supply text + labels,
//! receive `Vec<Span>`. Labels are passed verbatim — the upstream
//! reference does not lowercase them, and casing affects the
//! tokenisation of label tokens.
//!
//! ## Why a separate module
//!
//! GLiNER is *not* a BIO token classifier: its head emits a
//! `[num_words, max_width, num_labels]` score tensor that requires
//! a different decode (flat-NER, not BIO collapse) and an entirely
//! different input format (label markers prepended to the sentence).
//! The module owns its own backbone / head / tokenizer wiring and
//! exposes a small [`GlinerModel::predict`] surface; the
//! [`crate::classifier::GlinerClassifier`] adapter wraps it for the
//! extractor pipeline.
//!
//! ## Bootstrapping
//!
//! GLiNER v2.1 ships PyTorch pickle weights (`pytorch_model.bin`).
//! This module loads the pickle directly via candle's `PthTensors`
//! — no conversion step, no torch dependency. The `[ENT]` special
//! token is added to the DeBERTa-v3 tokenizer at load time, not by
//! the bootstrap script.

mod backbone;
mod decode;
mod head;
mod rnn;
mod tokenizer;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;

pub use self::backbone::{BackboneConfig, GlinerBackbone};
pub use self::head::{LabelProjection, ProjectionLayer, SpanMarkerHead};
pub use self::tokenizer::{split_words, TokenizedInput, TokenizerIds, WordOffset};

/// A detected entity span.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    /// One of the labels passed to [`GlinerModel::predict`], verbatim.
    pub label: String,
    /// The text covered by the span (sliced from the original input).
    pub text: String,
    /// Inclusive character offset of the span start, in the original text.
    pub char_start: usize,
    /// Exclusive character offset of the span end, in the original text.
    pub char_end: usize,
    /// Post-sigmoid confidence in `[0, 1]`.
    pub score: f32,
}

/// Runtime config for [`GlinerModel`]. All fields except `device` /
/// `dtype` are fixed by the gliner_config.json that ships with
/// `urchade/gliner_small-v2.1`; they are surfaced as fields anyway
/// so call sites can override (e.g., raise `threshold` for high-
/// precision use, drop `max_width` for short queries).
#[derive(Debug, Clone)]
pub struct GlinerConfig {
    /// Maximum span width, in WORDS (not subtokens). 12 for v2.1.
    pub max_width: usize,
    /// Maximum input length, in tokenizer subtokens including
    /// special tokens and label markers. 384 for v2.1.
    pub max_len: usize,
    /// Head hidden size — the projection / einsum dimension.
    /// 512 for v2.1.
    pub hidden_size: usize,
    /// Backbone hidden size. 768 for DeBERTa-v3-small.
    pub backbone_hidden_size: usize,
    /// Probability threshold (post-sigmoid). 0.5 default.
    pub threshold: f32,
    /// Cap on the number of labels accepted per `predict()` call.
    /// Upstream tooling treats > 25 as a configuration smell.
    pub max_labels: usize,
    /// Device for inference.
    pub device: Device,
    /// Tensor dtype. F16 recommended on GPU; F32 on CPU is the
    /// path that exercises every kernel without dtype edge cases.
    pub dtype: DType,
}

impl Default for GlinerConfig {
    fn default() -> Self {
        Self {
            max_width: 12,
            max_len: 384,
            hidden_size: 512,
            backbone_hidden_size: 768,
            threshold: 0.5,
            max_labels: 25,
            device: Device::Cpu,
            dtype: DType::F32,
        }
    }
}

/// Errors surfaced by the GLiNER inference path.
#[derive(thiserror::Error, Debug)]
pub enum GlinerError {
    /// A required file (pytorch_model.bin / tokenizer.json /
    /// gliner_config.json / config.json) was not present in the
    /// model directory passed to [`GlinerModel::load`].
    #[error("model file missing: {0}")]
    MissingFile(String),
    /// `candle_core::pickle::PthTensors::new` rejected the pickle.
    #[error("pickle load: {0}")]
    PickleLoad(String),
    /// A required pickle key was not present, or the backbone /
    /// head expected a tensor that wasn't in the file.
    #[error("config parse: {0}")]
    ConfigParse(String),
    /// `tokenizers::Tokenizer::from_file` or
    /// `Tokenizer::encode` returned an error.
    #[error("tokenizer: {0}")]
    Tokenizer(String),
    /// Any candle-side failure (forward pass, tensor build, dtype
    /// mismatch, etc).
    #[error("candle: {0}")]
    Candle(#[from] candle_core::Error),
    /// `predict()` was called with text whose total subtoken count
    /// (after prompt construction) exceeded `max_len`.
    #[error("input exceeds max_len: {got} > {max}")]
    InputTooLong {
        /// Total subtoken count, including special / marker tokens.
        got: usize,
        /// The configured cap (`GlinerConfig::max_len`).
        max: usize,
    },
    /// `predict()` was called with more than `max_labels` labels.
    #[error("too many labels: {got} > {limit}")]
    TooManyLabels {
        /// Number of labels passed.
        got: usize,
        /// The configured cap (`GlinerConfig::max_labels`).
        limit: usize,
    },
    /// A required marker token (`[CLS]` / `[SEP]` / `<<ENT>>` /
    /// `<<SEP>>`) was not resolvable in the loaded tokenizer. The
    /// bootstrap script is responsible for patching `<<ENT>>` and
    /// `<<SEP>>` into `tokenizer.json`; if either is absent the model
    /// directory was prepared by a different (or no) bootstrap.
    #[error("tokenizer is missing required token: {0}")]
    MissingToken(&'static str),
    /// `<<ENT>>` / `<<SEP>>` were present but resolved to an id that
    /// does not match what the v2.1 weights were trained against.
    /// Loading must abort: pooling at the wrong row silently returns
    /// nonsense scores.
    #[error("tokenizer id mismatch for {token}: got {got}, expected {expected}")]
    TokenIdMismatch {
        /// The token whose id was wrong.
        token: &'static str,
        /// What the tokenizer resolved.
        got: u32,
        /// What the trained embedding matrix demands.
        expected: u32,
    },
    /// Generic decode-side failure (shape mismatch, NaN, etc).
    #[error("decode failed: {0}")]
    Decode(String),
}

/// A fully-loaded GLiNER inference model.
pub struct GlinerModel {
    backbone: GlinerBackbone,
    head: SpanMarkerHead,
    label_proj: LabelProjection,
    tokenizer: Arc<tokenizers::Tokenizer>,
    token_ids: TokenizerIds,
    config: GlinerConfig,
}

/// GLiNER v2.1 trained against the DeBERTa-v3-small tokenizer extended
/// with two regular added tokens, in this exact order:
///
/// ```text
///   <<ENT>> @ id 128001
///   <<SEP>> @ id 128002
/// ```
///
/// The bootstrap script patches `tokenizer.json` so these IDs are
/// already wired in at load time; we just look them up. Drift between
/// what the bootstrap wrote and what's on disk would silently misalign
/// every label lookup with a wrong embedding row, so the IDs are
/// asserted on load.
const EXPECTED_ENT_ID: u32 = 128_001;
const EXPECTED_PROMPT_SEP_ID: u32 = 128_002;

impl GlinerModel {
    /// Load from a directory containing:
    ///
    /// - `pytorch_model.bin` — GLiNER pickle weights (loaded
    ///   directly via candle's `PthTensors`; no conversion step).
    /// - `tokenizer.json` — DeBERTa-v3 tokenizer pre-patched by
    ///   `.devcontainer/bootstrap-model.sh` with `<<ENT>>` @128001 and
    ///   `<<SEP>>` @128002.
    /// - `config.json` — DeBERTa-v2 / v3 backbone config.
    pub fn load(dir: &Path, config: GlinerConfig) -> Result<Self, GlinerError> {
        let pickle_path = dir.join("pytorch_model.bin");
        require_file(&pickle_path)?;
        let tokenizer_path = dir.join("tokenizer.json");
        require_file(&tokenizer_path)?;
        let backbone_config_path = dir.join("config.json");
        require_file(&backbone_config_path)?;

        let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| GlinerError::Tokenizer(e.to_string()))?;
        let token_ids = resolve_token_ids(&tokenizer)?;

        let mut backbone_config = backbone::load_config(&backbone_config_path)?;

        // The DeBERTa-v3-small `config.json` declares a vocab of 128100,
        // but GLiNER's pickled embedding matrix is sized for the trained
        // vocab (128003 + `[ENT]` = 128004). Override the config so the
        // embedding layer's shape check passes against the pickle.
        let pth_probe = candle_core::pickle::PthTensors::new(&pickle_path, None)
            .map_err(|e| GlinerError::PickleLoad(e.to_string()))?;
        let embed_key = "token_rep_layer.bert_layer.model.embeddings.word_embeddings.weight";
        let pickle_vocab = pth_probe
            .tensor_infos()
            .get(embed_key)
            .ok_or_else(|| GlinerError::PickleLoad(format!("missing tensor: {embed_key}")))?
            .layout
            .dims()[0];
        drop(pth_probe);
        backbone_config.vocab_size = pickle_vocab;

        let vb = VarBuilder::from_pth(&pickle_path, config.dtype, &config.device)
            .map_err(|e| GlinerError::PickleLoad(e.to_string()))?;

        // Pickle key layout (verified against urchade/gliner_small-v2.1):
        //
        //   backbone   : DeBERTa-v2 under `token_rep_layer.bert_layer.model.`
        //   projection : `token_rep_layer.projection.{weight,bias}` (768 → 512)
        //   rnn        : `rnn.lstm.{weight,bias}_{ih,hh}_l0[_reverse]`
        //   head       : `span_rep_layer.span_rep_layer.{project_start,project_end,out_project}.{0,3}.…`
        //   label      : `prompt_rep_layer.{0,3}.…`
        //
        // The 768 → 512 projection happens before the BiLSTM, so the
        // BiLSTM itself is 512 → 256 fwd + 256 bwd. Both the span head
        // and the label projection take `hidden_size`-dimensional
        // (512-d) inputs.
        let backbone = GlinerBackbone::load(
            vb.pp("token_rep_layer.bert_layer.model"),
            vb.pp("token_rep_layer.projection"),
            vb.pp("rnn"),
            &backbone_config,
            config.hidden_size,
        )?;
        let head = SpanMarkerHead::load(
            vb.pp("span_rep_layer.span_rep_layer"),
            config.hidden_size,
            config.hidden_size,
        )?;
        let label_proj = LabelProjection::load(
            vb.pp("prompt_rep_layer"),
            config.hidden_size,
            config.hidden_size,
        )?;

        Ok(Self {
            backbone,
            head,
            label_proj,
            tokenizer: Arc::new(tokenizer),
            token_ids,
            config,
        })
    }

    /// The active runtime config.
    pub fn config(&self) -> &GlinerConfig {
        &self.config
    }

    /// Run zero-shot NER over `text` with the given `labels`.
    ///
    /// - Labels are passed to the tokenizer verbatim (case-sensitive).
    /// - Returns spans sorted by `char_start` ascending; overlaps are
    ///   resolved greedily by score before sort.
    pub fn predict(&self, text: &str, labels: &[&str]) -> Result<Vec<Span>, GlinerError> {
        if labels.len() > self.config.max_labels {
            return Err(GlinerError::TooManyLabels {
                got: labels.len(),
                limit: self.config.max_labels,
            });
        }
        if labels.is_empty() {
            return Ok(Vec::new());
        }

        let tokenized = tokenizer::tokenize(
            self.tokenizer.as_ref(),
            text,
            labels,
            &self.token_ids,
            self.config.max_len,
        )?;

        if tokenized.word_first_subtoken.is_empty() {
            return Ok(Vec::new());
        }

        let device = &self.config.device;
        let seq_len = tokenized.input_ids.len();
        let input_ids = Tensor::from_vec(tokenized.input_ids.clone(), (1, seq_len), device)?;
        let attention_mask =
            Tensor::from_vec(tokenized.attention_mask.clone(), (1, seq_len), device)?;

        // Stage 1: DeBERTa over subtokens, then the 768→512 projection.
        // Output `[1, seq_len, hidden_size]` with one row per subtoken
        // (NOT per word — flair pools to words in upstream's path, we
        // do that explicitly below via index_select on the recorded
        // first-subtoken positions).
        let subtoken_hidden = self
            .backbone
            .encode_subtokens(&input_ids, &attention_mask)?;
        let subtoken_hidden = subtoken_hidden.to_dtype(self.config.dtype)?;
        let subtoken_2d = subtoken_hidden.squeeze(0)?; // [seq_len, hidden_size]

        // Label embeddings: pool the projected hidden state at every
        // <<ENT>> marker position (one per label), then push through
        // the label MLP. The label vectors never see the BiLSTM —
        // upstream's `compute_score_eval` applies `prompt_rep_layer`
        // directly to the BERT-side embedding of the markers.
        let ent_positions = Tensor::from_vec(
            tokenized.ent_positions.clone(),
            tokenized.ent_positions.len(),
            device,
        )?;
        let label_hidden = subtoken_2d.index_select(&ent_positions, 0)?;
        let prompt_emb = self.label_proj.forward(&label_hidden)?;

        // Word embeddings: pool to one vector per WORD (first-subtoken
        // pooling) on the projected hidden state, then run the BiLSTM
        // over the word-pooled sequence — matching upstream where the
        // RNN's input length is `num_words`, not `num_subtokens`.
        let word_indices = Tensor::from_vec(
            tokenized.word_first_subtoken.clone(),
            tokenized.word_first_subtoken.len(),
            device,
        )?;
        let word_hidden = subtoken_2d.index_select(&word_indices, 0)?;
        let word_hidden_3d = word_hidden.unsqueeze(0)?;
        let word_hidden_rnn = self.backbone.run_rnn(&word_hidden_3d)?;

        let span_rep = self.head.forward(
            &word_hidden_rnn,
            self.config.max_width,
            self.config.hidden_size,
        )?;
        // span_rep: [1, num_words, max_width, hidden]

        // Score: einsum BLKD, BCD -> BLKC.
        let num_words = tokenized.word_first_subtoken.len();
        let max_width = self
            .config
            .hidden_size_aware_max_width(num_words, self.config.max_width);
        let scores = score_spans(&span_rep, &prompt_emb, num_words, max_width, labels.len())?;

        let logits_3d = scores.to_dtype(DType::F32)?.to_vec3::<f32>()?;
        let spans = decode::decode_spans(
            &logits_3d,
            self.config.threshold,
            labels,
            &tokenized.word_offsets,
            text,
        );
        Ok(spans)
    }

    /// Run zero-shot NER over a batch of `(text, labels)` pairs in a
    /// single backbone forward pass.
    ///
    /// All rows in `inputs` MUST share the same `labels` slice — the
    /// batched path pools the label-marker positions row-wise from the
    /// SAME prompt structure, so a mixed label set would break that
    /// invariant. Returns `Err(GlinerError::Decode)` with a descriptive
    /// message when this is violated.
    ///
    /// Rows whose total token length exceeds `max_len` after prompt
    /// construction produce an empty `Vec<Span>` for that slot (logged
    /// via `tracing::warn!`); they do not abort the entire batch. The
    /// output `Vec` has the same length as `inputs` with one
    /// `Vec<Span>` per input, in input order.
    ///
    /// An empty `inputs` returns `Ok(vec![])` without any model work.
    pub fn predict_batch(&self, inputs: &[(&str, &[&str])]) -> Result<Vec<Vec<Span>>, GlinerError> {
        match validate_batch_inputs(inputs, self.config.max_labels)? {
            BatchValidation::Empty => return Ok(Vec::new()),
            BatchValidation::AllEmptyLabels => return Ok(vec![Vec::new(); inputs.len()]),
            BatchValidation::Live => {}
        }

        // Stage 1 — per-row tokenize. Rows that overflow `max_len` are
        // recorded as `None` and slotted as empty in the output without
        // consuming a row in the batched backbone pass.
        let mut tokenized_rows: Vec<Option<tokenizer::TokenizedInput>> =
            Vec::with_capacity(inputs.len());
        for (text, labels) in inputs {
            match tokenizer::tokenize(
                self.tokenizer.as_ref(),
                text,
                labels,
                &self.token_ids,
                self.config.max_len,
            ) {
                Ok(tok) => tokenized_rows.push(Some(tok)),
                Err(GlinerError::InputTooLong { got, max }) => {
                    tracing::warn!(
                        target: "brain_extractors::gliner",
                        text_chars = text.chars().count(),
                        subtokens = got,
                        max = max,
                        "input exceeds max_len; skipping in batch and returning empty spans",
                    );
                    tokenized_rows.push(None);
                }
                Err(e) => return Err(e),
            }
        }

        // If every row was over-long, short-circuit. Otherwise build the
        // padded batch from just the live rows.
        let live_indices: Vec<usize> = tokenized_rows
            .iter()
            .enumerate()
            .filter_map(|(i, t)| t.as_ref().map(|_| i))
            .collect();
        if live_indices.is_empty() {
            return Ok(vec![Vec::new(); inputs.len()]);
        }

        let max_seq_len = live_indices
            .iter()
            .map(|i| {
                tokenized_rows[*i]
                    .as_ref()
                    .expect("invariant: live_indices only holds indices of Some rows")
                    .input_ids
                    .len()
            })
            .max()
            .expect("invariant: live_indices is non-empty, checked above");

        // Pad live rows to max_seq_len. DeBERTa-v3 pad_token_id == 0
        // (validated against the upstream config); the attention mask
        // gates pad positions out of the backbone's attention.
        const PAD_TOKEN_ID: u32 = 0;
        let batch_size = live_indices.len();
        let mut flat_ids: Vec<u32> = Vec::with_capacity(batch_size * max_seq_len);
        let mut flat_mask: Vec<u32> = Vec::with_capacity(batch_size * max_seq_len);
        for &row_idx in &live_indices {
            let row = tokenized_rows[row_idx]
                .as_ref()
                .expect("invariant: live_indices only holds indices of Some rows");
            flat_ids.extend_from_slice(&row.input_ids);
            flat_mask.extend_from_slice(&row.attention_mask);
            let pad = max_seq_len - row.input_ids.len();
            flat_ids.extend(std::iter::repeat_n(PAD_TOKEN_ID, pad));
            flat_mask.extend(std::iter::repeat_n(0u32, pad));
        }

        let device = &self.config.device;
        let input_ids = Tensor::from_vec(flat_ids, (batch_size, max_seq_len), device)?;
        let attention_mask = Tensor::from_vec(flat_mask, (batch_size, max_seq_len), device)?;

        // Single backbone forward over the whole batch — the GEMM-heavy
        // win we're after. Output `[B, max_seq_len, hidden]`.
        let subtoken_hidden = self
            .backbone
            .encode_subtokens(&input_ids, &attention_mask)?;
        let subtoken_hidden = subtoken_hidden.to_dtype(self.config.dtype)?;

        // Per-row decode. Each row has its own ent_positions / word
        // indices / word_offsets, so we slice the [B, L, H] tensor at
        // its row dim and run the (cheap, post-backbone) pieces per row.
        let mut out: Vec<Vec<Span>> = (0..inputs.len()).map(|_| Vec::new()).collect();
        for (live_pos, &row_idx) in live_indices.iter().enumerate() {
            let row = tokenized_rows[row_idx]
                .as_ref()
                .expect("invariant: live_indices only holds indices of Some rows");
            if row.word_first_subtoken.is_empty() {
                continue;
            }
            // [max_seq_len, hidden] — drop the batch axis for this row.
            let row_hidden = subtoken_hidden.get(live_pos)?;

            let ent_positions =
                Tensor::from_vec(row.ent_positions.clone(), row.ent_positions.len(), device)?;
            let label_hidden = row_hidden.index_select(&ent_positions, 0)?;
            let prompt_emb = self.label_proj.forward(&label_hidden)?;

            let word_indices = Tensor::from_vec(
                row.word_first_subtoken.clone(),
                row.word_first_subtoken.len(),
                device,
            )?;
            let word_hidden = row_hidden.index_select(&word_indices, 0)?;
            let word_hidden_3d = word_hidden.unsqueeze(0)?;
            let word_hidden_rnn = self.backbone.run_rnn(&word_hidden_3d)?;

            let span_rep = self.head.forward(
                &word_hidden_rnn,
                self.config.max_width,
                self.config.hidden_size,
            )?;

            let num_words = row.word_first_subtoken.len();
            let max_width = self
                .config
                .hidden_size_aware_max_width(num_words, self.config.max_width);
            let labels = inputs[row_idx].1;
            let scores = score_spans(&span_rep, &prompt_emb, num_words, max_width, labels.len())?;
            let logits_3d = scores.to_dtype(DType::F32)?.to_vec3::<f32>()?;
            let text = inputs[row_idx].0;
            let spans = decode::decode_spans(
                &logits_3d,
                self.config.threshold,
                labels,
                &row.word_offsets,
                text,
            );
            out[row_idx] = spans;
        }

        Ok(out)
    }
}

/// Compute `scores = einsum("BLKD,BCD->BLKC")` with B=1.
///
/// candle does not ship a generic einsum; we reshape and matmul:
/// `[num_words * max_width, hidden] @ [hidden, num_labels]`.
fn score_spans(
    span_rep: &Tensor,
    prompt_emb: &Tensor,
    num_words: usize,
    max_width: usize,
    num_labels: usize,
) -> Result<Tensor, GlinerError> {
    let hidden = prompt_emb.dim(prompt_emb.rank() - 1)?;
    // span_rep:    [1, num_words, max_width, hidden] -> [num_words*max_width, hidden]
    let flat = span_rep
        .squeeze(0)?
        .reshape((num_words * max_width, hidden))?;
    // prompt_emb:  [num_labels, hidden] -> [hidden, num_labels]
    let proj = prompt_emb.t()?;
    let out = flat.matmul(&proj)?; // [num_words*max_width, num_labels]
    let out = out.reshape((num_words, max_width, num_labels))?;
    Ok(out)
}

/// Outcome of [`validate_batch_inputs`].
#[derive(Debug)]
pub(crate) enum BatchValidation {
    /// `inputs.is_empty()`. Caller returns `Ok(vec![])` without
    /// touching the model.
    Empty,
    /// All rows share an empty label set — every row's output is
    /// trivially an empty `Vec<Span>` without inference.
    AllEmptyLabels,
    /// Rows are well-formed and have a non-empty label set; proceed
    /// with the batched forward pass.
    Live,
}

/// Pure-logic validation for [`GlinerModel::predict_batch`]. Lifted
/// out of the method body so tests can exercise the early-return /
/// rejection paths without constructing a full model.
pub(crate) fn validate_batch_inputs(
    inputs: &[(&str, &[&str])],
    max_labels: usize,
) -> Result<BatchValidation, GlinerError> {
    if inputs.is_empty() {
        return Ok(BatchValidation::Empty);
    }
    let first_labels = inputs[0].1;
    if first_labels.len() > max_labels {
        return Err(GlinerError::TooManyLabels {
            got: first_labels.len(),
            limit: max_labels,
        });
    }
    for (row_idx, (_, labels)) in inputs.iter().enumerate().skip(1) {
        if labels.len() != first_labels.len()
            || labels.iter().zip(first_labels.iter()).any(|(a, b)| a != b)
        {
            return Err(GlinerError::Decode(format!(
                "predict_batch requires all rows share the same labels (row 0 vs row {row_idx} differ)"
            )));
        }
    }
    if first_labels.is_empty() {
        return Ok(BatchValidation::AllEmptyLabels);
    }
    Ok(BatchValidation::Live)
}

fn require_file(p: &Path) -> Result<(), GlinerError> {
    if !p.exists() {
        return Err(GlinerError::MissingFile(p.display().to_string()));
    }
    Ok(())
}

/// Resolve the four token ids GLiNER inference needs and assert the
/// two GLiNER-specific markers land at the IDs the trained embedding
/// rows expect. Drift here would point pooling at the wrong rows of
/// the word_embeddings matrix and silently corrupt every prediction.
fn resolve_token_ids(tokenizer: &tokenizers::Tokenizer) -> Result<TokenizerIds, GlinerError> {
    let cls = tokenizer
        .token_to_id("[CLS]")
        .ok_or(GlinerError::MissingToken("[CLS]"))?;
    let sep = tokenizer
        .token_to_id("[SEP]")
        .ok_or(GlinerError::MissingToken("[SEP]"))?;
    let ent = tokenizer
        .token_to_id("<<ENT>>")
        .ok_or(GlinerError::MissingToken("<<ENT>>"))?;
    let prompt_sep = tokenizer
        .token_to_id("<<SEP>>")
        .ok_or(GlinerError::MissingToken("<<SEP>>"))?;

    if ent != EXPECTED_ENT_ID {
        return Err(GlinerError::TokenIdMismatch {
            token: "<<ENT>>",
            got: ent,
            expected: EXPECTED_ENT_ID,
        });
    }
    if prompt_sep != EXPECTED_PROMPT_SEP_ID {
        return Err(GlinerError::TokenIdMismatch {
            token: "<<SEP>>",
            got: prompt_sep,
            expected: EXPECTED_PROMPT_SEP_ID,
        });
    }

    Ok(TokenizerIds {
        cls,
        sep,
        ent,
        prompt_sep,
    })
}

impl GlinerConfig {
    /// Reserved for future model variants where `max_width` may be
    /// shrunk per-batch to fit `num_words` (no point enumerating
    /// widths that overflow the sentence). For now: returns the
    /// configured `max_width` unchanged so behaviour matches the
    /// reference exactly — out-of-bounds widths are filtered during
    /// decode.
    fn hidden_size_aware_max_width(&self, _num_words: usize, max_width: usize) -> usize {
        max_width
    }
}

/// Convenience alias: returns the on-disk paths that
/// [`GlinerModel::load`] expects in a downloaded model directory.
pub fn expected_paths(dir: &Path) -> [PathBuf; 3] {
    [
        dir.join("pytorch_model.bin"),
        dir.join("tokenizer.json"),
        dir.join("config.json"),
    ]
}
