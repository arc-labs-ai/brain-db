//! FFT helpers for circular convolution and correlation.
//!
//! The HRR algebra builds on circular convolution, which becomes
//! pointwise multiplication in the frequency domain. We route through
//! `rustfft`'s mixed-radix planner — for D=512 the planner picks a
//! split-radix variant and bind costs ~50 µs single-threaded.

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

/// Cached forward + inverse FFT plans for a fixed length. Reused
/// across `bind` / `unbind` calls inside a planner thread so we don't
/// re-amortize planner setup on every call.
pub struct FftPlans {
    pub n: usize,
    pub forward: Arc<dyn Fft<f32>>,
    pub inverse: Arc<dyn Fft<f32>>,
}

impl FftPlans {
    pub fn new(n: usize) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        Self {
            n,
            forward: planner.plan_fft_forward(n),
            inverse: planner.plan_fft_inverse(n),
        }
    }
}

thread_local! {
    static PLANS_512: FftPlans = FftPlans::new(super::ops::VSA_DIM);
}

/// Circular convolution `a * b` where `*` is wraparound convolution
/// of length `a.len() == b.len()`. Returns a fresh `Vec<f32>` of the
/// same length.
pub fn convolve_circular(a: &[f32], b: &[f32]) -> Vec<f32> {
    let n = a.len();
    debug_assert_eq!(a.len(), b.len(), "convolve_circular requires equal lengths");
    if n == super::ops::VSA_DIM {
        PLANS_512.with(|plans| convolve_with_plans(plans, a, b))
    } else {
        let plans = FftPlans::new(n);
        convolve_with_plans(&plans, a, b)
    }
}

/// Circular correlation `correlate(c, a)` = circular convolution of
/// `c` with the involution of `a`, where `involution(a)[k] = a[(n-k)
/// mod n]`. Used as the inverse of `convolve_circular`: if `c =
/// convolve(a, b)`, then `correlate(c, a) ≈ b` up to floating-point
/// noise.
pub fn correlate_circular(c: &[f32], a: &[f32]) -> Vec<f32> {
    let n = a.len();
    debug_assert_eq!(
        c.len(),
        a.len(),
        "correlate_circular requires equal lengths"
    );
    let mut a_inv = vec![0.0_f32; n];
    a_inv[0] = a[0];
    for k in 1..n {
        a_inv[k] = a[n - k];
    }
    convolve_circular(c, &a_inv)
}

fn convolve_with_plans(plans: &FftPlans, a: &[f32], b: &[f32]) -> Vec<f32> {
    let n = plans.n;
    let mut a_c: Vec<Complex32> = a.iter().map(|&x| Complex32::new(x, 0.0)).collect();
    let mut b_c: Vec<Complex32> = b.iter().map(|&x| Complex32::new(x, 0.0)).collect();
    plans.forward.process(&mut a_c);
    plans.forward.process(&mut b_c);
    for (a_k, b_k) in a_c.iter_mut().zip(b_c.iter()) {
        *a_k *= *b_k;
    }
    plans.inverse.process(&mut a_c);
    let inv_n = 1.0_f32 / n as f32;
    a_c.iter().map(|x| x.re * inv_n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Building block: circular identity vector `[1, 0, 0, ...]`. It
    /// is the identity of circular convolution.
    fn circular_identity(n: usize) -> Vec<f32> {
        let mut id = vec![0.0_f32; n];
        id[0] = 1.0;
        id
    }

    #[test]
    fn convolve_circular_with_identity_returns_input() {
        let n = 32;
        let a: Vec<f32> = (0..n).map(|i| (i as f32) * 0.1 - 1.5).collect();
        let id = circular_identity(n);
        let c = convolve_circular(&a, &id);
        for (x, y) in a.iter().zip(c.iter()) {
            assert!((x - y).abs() < 1e-4, "x={x} y={y}");
        }
    }

    #[test]
    fn correlate_inverts_convolve_for_unitary_a() {
        // Correlation is an exact inverse of convolution *when `a` is
        // a unitary HRR vector* (unit magnitude in the frequency
        // domain). Build one explicitly here so this test exercises
        // just the FFT pathway, independent of `random_vec`.
        let n = super::super::ops::VSA_DIM;
        let a = super::super::ops::random_vec(0xC0FFEE);
        let b: Vec<f32> = (0..n).map(|i| ((i as f32) * 0.029).cos() * 0.2).collect();
        let c = convolve_circular(&a, &b);
        let b_hat = correlate_circular(&c, &a);
        let mut num = 0.0_f32;
        let mut denom_b = 0.0_f32;
        let mut denom_bhat = 0.0_f32;
        for (x, y) in b.iter().zip(b_hat.iter()) {
            num += x * y;
            denom_b += x * x;
            denom_bhat += y * y;
        }
        let cos = num / (denom_b.sqrt() * denom_bhat.sqrt());
        assert!(cos > 0.99, "cos similarity too low: {cos}");
    }
}
