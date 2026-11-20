//! Thin wrapper around `candle_transformers::models::debertav2::DebertaV2Model`
//! plus the GLiNER projection + BiLSTM that follow it.
//!
//! GLiNER v2.1's `token_rep_layer` does three things in sequence:
//!
//!   1. Run DeBERTa-v3-small → `[B, L, 768]`.
//!   2. Apply a `Linear(768, 512)` projection
//!      (`token_rep_layer.projection`) to land in head width.
//!   3. Feed the 512-d sequence through a single-layer bidirectional
//!      LSTM with `hidden_size = 256`; concatenating the two
//!      directions returns to 512-d.
//!
//! The tensor that reaches the span head is `[B, L, 512]` —
//! matching the head's `project_start.0.weight = [2048, 512]`.

use std::path::Path;

use candle_core::Tensor;
use candle_nn::{Linear, Module, VarBuilder};
use candle_transformers::models::debertav2::{Config as DebertaV2Config, DebertaV2Model};

use super::rnn::BiLstm;
use super::GlinerError;

/// BiLSTM hidden size used by GLiNER v2.1. Bidirectional concat lands
/// at `2 * BILSTM_HIDDEN = 512`, which is the head's input width.
const BILSTM_HIDDEN: usize = 256;

/// Re-export of the backbone config — we don't add fields, but
/// callers should depend on this alias so a future swap to a vendored
/// impl stays source-compatible.
pub type BackboneConfig = DebertaV2Config;

/// Read `config.json` from disk.
pub(crate) fn load_config(path: &Path) -> Result<BackboneConfig, GlinerError> {
    let bytes = std::fs::read(path).map_err(|e| {
        GlinerError::MissingFile(format!("config.json read failed: {e} ({})", path.display()))
    })?;
    serde_json::from_slice::<BackboneConfig>(&bytes)
        .map_err(|e| GlinerError::ConfigParse(format!("config.json parse: {e}")))
}

/// Wrapper that owns the DeBERTa model, the 768→512 projection, and
/// the BiLSTM. GLiNER inference uses them in two stages — DeBERTa +
/// projection over the raw subtoken stream, then word-pooling, then
/// BiLSTM over the pooled sequence — so they're exposed individually
/// rather than fused into a single forward.
pub struct GlinerBackbone {
    deberta: DebertaV2Model,
    projection: Linear,
    bi_lstm: BiLstm,
}

impl GlinerBackbone {
    /// Load the three sub-modules. `deberta_vb` is rooted at the
    /// DeBERTa model's path (`token_rep_layer.bert_layer.model`);
    /// `projection_vb` is rooted at `token_rep_layer.projection`;
    /// `rnn_vb` is rooted at `rnn`. Splitting the prefixes keeps the
    /// pickle-key layout in one place (the caller) instead of
    /// scattered through this module.
    pub(crate) fn load(
        deberta_vb: VarBuilder,
        projection_vb: VarBuilder,
        rnn_vb: VarBuilder,
        config: &BackboneConfig,
        head_hidden: usize,
    ) -> Result<Self, GlinerError> {
        let deberta = DebertaV2Model::load(deberta_vb, config)?;
        let projection = candle_nn::linear(config.hidden_size, head_hidden, projection_vb)?;
        let bi_lstm = BiLstm::load(rnn_vb.pp("lstm"), head_hidden, BILSTM_HIDDEN)?;
        Ok(Self {
            deberta,
            projection,
            bi_lstm,
        })
    }

    /// DeBERTa + 768→512 projection over the raw subtoken sequence.
    /// `input_ids` / `attention_mask` are `[B, seq_len]`; returns
    /// `[B, seq_len, head_hidden]`.
    pub fn encode_subtokens(
        &self,
        input_ids: &Tensor,
        attention_mask: &Tensor,
    ) -> Result<Tensor, GlinerError> {
        let mask = attention_mask.clone();
        let hidden = self.deberta.forward(input_ids, None, Some(mask))?;
        Ok(self.projection.forward(&hidden)?)
    }

    /// BiLSTM over a word-pooled `[B, num_words, head_hidden]` tensor.
    /// Output shape is identical (the BiLSTM concat'd directions land
    /// at `2 * BILSTM_HIDDEN = head_hidden`).
    pub fn run_rnn(&self, word_hidden: &Tensor) -> Result<Tensor, GlinerError> {
        self.bi_lstm.forward(word_hidden)
    }
}
