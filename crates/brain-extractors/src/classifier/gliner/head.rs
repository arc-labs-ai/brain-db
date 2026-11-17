//! GLiNER `markerV0` span-representation head + label projection.
//!
//! Architecture (verbatim from `urchade/gliner_small-v2.1`):
//!
//! - `project_start`: `Linear(512, 2048) → ReLU → Linear(2048, 512)`
//! - `project_end`  : `Linear(512, 2048) → ReLU → Linear(2048, 512)`
//! - `out_project`  : `Linear(1024, 2048) → ReLU → Linear(2048, 512)`
//! - `label_proj`   : `Linear(512, 2048) → ReLU → Linear(2048, 512)`
//!
//! The head consumes the BiLSTM output (512-d), not the raw 768-d
//! DeBERTa hidden state. Hidden expansion ratio is 4 for the 512→
//! start/end/label branches, but 2 for the 1024→ `out_project`
//! branch (its inner width matches the start/end inner width, 2048).
//! Dropout layers from training are not applied at inference.
//!
//! Pickle key naming (verified against upstream GLiNER v2.1):
//!
//! ```text
//! span_rep_layer.span_rep_layer.project_start.{0,3}.{weight,bias}
//! span_rep_layer.span_rep_layer.project_end.{0,3}.{weight,bias}
//! span_rep_layer.span_rep_layer.out_project.{0,3}.{weight,bias}
//! prompt_rep_layer.{0,3}.{weight,bias}
//! ```
//!
//! The MLP is `nn.Sequential(Linear, Dropout, ReLU, Linear)` so the
//! two `Linear`s land at sub-paths `0` and `3` (not `0` and `2`).

use candle_core::Tensor;
use candle_nn::{Linear, Module, VarBuilder};

use super::GlinerError;

/// `Linear → ReLU → Linear` block, the GLiNER MLP primitive.
#[derive(Debug)]
pub struct ProjectionLayer {
    linear1: Linear,
    linear2: Linear,
}

impl ProjectionLayer {
    /// Load from a `VarBuilder` rooted at the MLP's parent path. The
    /// two `Linear`s are read from sub-paths `0` and `3`, matching the
    /// way `nn.Sequential(Linear, Dropout, ReLU, Linear)` serialises
    /// in PyTorch.
    fn load(
        vb: VarBuilder,
        in_dim: usize,
        hidden_dim: usize,
        out_dim: usize,
    ) -> Result<Self, GlinerError> {
        let linear1 = candle_nn::linear(in_dim, hidden_dim, vb.pp("0"))?;
        let linear2 = candle_nn::linear(hidden_dim, out_dim, vb.pp("3"))?;
        Ok(Self { linear1, linear2 })
    }

    /// Construct from already-built `Linear`s — used by the unit
    /// tests to inject synthetic weights without a pickle round trip.
    #[cfg(test)]
    pub(crate) fn from_linears(linear1: Linear, linear2: Linear) -> Self {
        Self { linear1, linear2 }
    }

    /// Forward pass: `linear2(relu(linear1(x)))`.
    pub fn forward(&self, x: &Tensor) -> Result<Tensor, GlinerError> {
        let h = self.linear1.forward(x)?;
        let h = h.relu()?;
        Ok(self.linear2.forward(&h)?)
    }
}

/// Label-side projection: 768 → 3072 → 512.
#[derive(Debug)]
pub struct LabelProjection {
    proj: ProjectionLayer,
}

impl LabelProjection {
    pub(crate) fn load(
        vb: VarBuilder,
        backbone_hidden: usize,
        head_hidden: usize,
    ) -> Result<Self, GlinerError> {
        // Expansion ratio 4 — same as upstream.
        let hidden = backbone_hidden * 4;
        let proj = ProjectionLayer::load(vb, backbone_hidden, hidden, head_hidden)?;
        Ok(Self { proj })
    }

    /// Test-only constructor.
    #[cfg(test)]
    pub(crate) fn from_proj(proj: ProjectionLayer) -> Self {
        Self { proj }
    }

    /// Forward pass: `[*, backbone_hidden] -> [*, head_hidden]`.
    pub fn forward(&self, x: &Tensor) -> Result<Tensor, GlinerError> {
        self.proj.forward(x)
    }
}

/// `markerV0` span representation head.
#[derive(Debug)]
pub struct SpanMarkerHead {
    project_start: ProjectionLayer,
    project_end: ProjectionLayer,
    out_project: ProjectionLayer,
}

impl SpanMarkerHead {
    pub(crate) fn load(
        vb: VarBuilder,
        backbone_hidden: usize,
        head_hidden: usize,
    ) -> Result<Self, GlinerError> {
        // 512 → 2048 → 512 for the start/end MLPs.
        let inner = backbone_hidden * 4;
        let project_start = ProjectionLayer::load(
            vb.pp("project_start"),
            backbone_hidden,
            inner,
            backbone_hidden,
        )?;
        let project_end = ProjectionLayer::load(
            vb.pp("project_end"),
            backbone_hidden,
            inner,
            backbone_hidden,
        )?;
        // 1024 → 2048 → 512. `out_project` reuses the same inner
        // width as the start/end MLPs (`backbone_hidden * 4`), so the
        // expansion ratio is 2x relative to its 1024-d input — *not*
        // the 4x that `project_start`/`project_end` use.
        let cat_in = backbone_hidden * 2;
        let cat_hidden = backbone_hidden * 4;
        let out_project =
            ProjectionLayer::load(vb.pp("out_project"), cat_in, cat_hidden, head_hidden)?;
        Ok(Self {
            project_start,
            project_end,
            out_project,
        })
    }

    /// Test-only constructor.
    #[cfg(test)]
    pub(crate) fn from_parts(
        project_start: ProjectionLayer,
        project_end: ProjectionLayer,
        out_project: ProjectionLayer,
    ) -> Self {
        Self {
            project_start,
            project_end,
            out_project,
        }
    }

    /// Forward pass.
    ///
    /// Input `h`: `[B, num_words, backbone_hidden]`.
    /// Output:    `[B, num_words, max_width, head_hidden]`.
    ///
    /// For every word index `i` and width `k ∈ [0, max_width)`, span
    /// `(i, i+k)` is represented by concatenating the projected
    /// hidden state at `i` (start) with the projected hidden state
    /// at `i+k` (end), then pushing through `out_project ∘ ReLU`.
    /// Words near the right edge of the sequence wrap their `end`
    /// index back into bounds; the decoder filters those positions
    /// out so they never produce a span.
    pub fn forward(
        &self,
        h: &Tensor,
        max_width: usize,
        head_hidden: usize,
    ) -> Result<Tensor, GlinerError> {
        let (batch, num_words, _backbone_hidden) = h.dims3()?;
        // start_rep, end_rep: [B, num_words, backbone_hidden]
        let start_rep = self.project_start.forward(h)?;
        let end_rep = self.project_end.forward(h)?;

        // For each width offset k, gather end_rep at position i+k.
        // For width 0, end_idx == start_idx; for width k, shift the
        // end_rep tensor left by k along the seq axis, padding the
        // tail with the last valid row (the decoder discards those
        // positions anyway).
        let device = h.device();
        let mut shifted_ends: Vec<Tensor> = Vec::with_capacity(max_width);
        for k in 0..max_width {
            if k == 0 {
                shifted_ends.push(end_rep.clone());
            } else {
                let indices: Vec<u32> = (0..num_words as u32)
                    .map(|i| {
                        let j = (i as usize).saturating_add(k);
                        // Clamp to num_words - 1; decode filters
                        // out-of-range spans.
                        let clamped = j.min(num_words.saturating_sub(1));
                        clamped as u32
                    })
                    .collect();
                let idx = Tensor::from_vec(indices, num_words, device)?;
                let gathered = end_rep.index_select(&idx, 1)?;
                shifted_ends.push(gathered);
            }
        }
        // Stack along a new width axis: [B, num_words, max_width, backbone_hidden].
        let end_stack = Tensor::stack(&shifted_ends, 2)?;
        // Broadcast start_rep across the width axis.
        let start_stack =
            start_rep
                .unsqueeze(2)?
                .expand((batch, num_words, max_width, end_stack.dim(3)?))?;

        // Concat along the hidden axis: [B, num_words, max_width, 2*backbone_hidden].
        let cat = Tensor::cat(&[&start_stack, &end_stack], 3)?.contiguous()?;
        let activated = cat.relu()?;

        // out_project flattens the leading batch dims internally
        // because candle_nn::Linear broadcasts; explicit reshape
        // keeps the dtype-broadcast path simple.
        let cat_dim = activated.dim(3)?;
        let flat = activated.reshape((batch * num_words * max_width, cat_dim))?;
        let projected = self.out_project.forward(&flat)?;
        let span_rep = projected.reshape((batch, num_words, max_width, head_hidden))?;
        Ok(span_rep)
    }
}
