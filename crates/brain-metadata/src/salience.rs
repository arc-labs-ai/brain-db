//! Salience decay, computed when read.
//!
//! A memory stores an *anchor*: the salience it had at
//! `salience_anchor_at_unix_nanos` (its creation, or its last boost /
//! explicit update). Its salience at any later time decays *towards* a
//! floor:
//!
//! ```text
//! salience(now) = floor + (anchor − floor) × 2^(−(now − anchor_at) / half_life(kind))
//! floor         = min(SALIENCE_FLOOR, salience_initial, anchor)
//! ```
//!
//! so nothing rewrites rows as time passes and every reader sees the exact
//! value. The part above the floor halves every half-life; the value never
//! goes below the floor and approaches it smoothly (as Mem0's decay
//! dampens idle memories towards a floor rather than clamping them).
//! Decay never raises a value: a memory created (or set) below
//! [`SALIENCE_FLOOR`] stays where it is.
//!
//! Because the curve is a constant plus a multiple of `2^(−t/h)`,
//! aggregates sum it exactly in *forward* form (Cormode et al., "Forward
//! Decay", ICDE 2009): Σ floor is a constant, and [`forward_weight`]
//! measures each row's excess from the fixed [`LANDMARK_UNIX_NANOS`], so it
//! never changes as time passes; [`forward_scale`] turns a sum of weights
//! back into the excess sum at `now`. No row ever has to be revisited.

use brain_core::MemoryKind;

pub const EPISODIC_HALF_LIFE_DAYS: f64 = 30.0;
pub const SEMANTIC_HALF_LIFE_DAYS: f64 = 365.0;
pub const CONSOLIDATED_HALF_LIFE_DAYS: f64 = 90.0;

/// Decay approaches, and never goes below, this.
pub const SALIENCE_FLOOR: f32 = 0.05;

pub const NANOS_PER_DAY: f64 = 86_400.0 * 1_000_000_000.0;

/// Fixed origin for forward-decay weights: 2024-01-01T00:00:00Z.
///
/// ponytail: no re-basing. Weights grow as 2^(t / half_life), so f64
/// overflows ~84 years after this for the 30-day half-life (≈2108); move
/// the landmark (rescale the sums once) well before then.
pub const LANDMARK_UNIX_NANOS: u64 = 1_704_067_200 * 1_000_000_000;

/// Wall-clock now, unix nanos (0 if the clock is before 1970).
#[must_use]
pub fn now_unix_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// Half-life by kind, in days.
#[must_use]
pub fn half_life_days(kind: MemoryKind) -> f64 {
    match kind {
        MemoryKind::Episodic => EPISODIC_HALF_LIFE_DAYS,
        MemoryKind::Semantic => SEMANTIC_HALF_LIFE_DAYS,
        MemoryKind::Consolidated => CONSOLIDATED_HALF_LIFE_DAYS,
    }
}

/// Half-life in nanoseconds for a stored kind byte. An unknown byte
/// (corrupt row) decays as episodic rather than failing the read.
#[must_use]
pub fn half_life_nanos(kind: u8) -> f64 {
    let kind = crate::tables::memory::memory_kind_from_u8(kind).unwrap_or(MemoryKind::Episodic);
    half_life_days(kind) * NANOS_PER_DAY
}

/// The level a row decays towards.
#[must_use]
pub fn floor_for(salience_initial: f32, anchor: f32) -> f32 {
    SALIENCE_FLOOR.min(anchor).min(salience_initial).max(0.0)
}

/// Salience at `now` of a row anchored at `anchor` at `anchor_at`.
/// `now` before `anchor_at` (clock step-back) reads as the anchor.
#[must_use]
pub fn salience_at(anchor: f32, anchor_at: u64, kind: u8, salience_initial: f32, now: u64) -> f32 {
    let floor = f64::from(floor_for(salience_initial, anchor));
    let age = now.saturating_sub(anchor_at) as f64;
    (floor + (f64::from(anchor) - floor) * (-age / half_life_nanos(kind)).exp2()) as f32
}

/// The row's forward-decay weight: its excess over the floor measured
/// from the landmark, `(anchor − floor) × 2^((anchor_at − L) / half_life)`.
/// Constant over time.
#[must_use]
pub fn forward_weight(anchor: f32, anchor_at: u64, kind: u8, salience_initial: f32) -> f64 {
    let excess = f64::from(anchor) - f64::from(floor_for(salience_initial, anchor));
    let since = anchor_at as f64 - LANDMARK_UNIX_NANOS as f64;
    excess * (since / half_life_nanos(kind)).exp2()
}

/// Multiplier turning a sum of [`forward_weight`]s (one kind) into the
/// sum of those rows' excess over their floors at `now`.
#[must_use]
pub fn forward_scale(kind: u8, now: u64) -> f64 {
    let since = now as f64 - LANDMARK_UNIX_NANOS as f64;
    (-since / half_life_nanos(kind)).exp2()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400 * 1_000_000_000;
    const T0: u64 = LANDMARK_UNIX_NANOS + 400 * DAY;

    #[test]
    fn excess_halves_per_half_life_towards_the_floor() {
        let ep = 0u8; // episodic
        let s = |days: u64| salience_at(0.85, T0, ep, 0.85, T0 + days * DAY);
        assert!((s(0) - 0.85).abs() < 1e-6);
        assert!((s(30) - 0.45).abs() < 1e-6, "0.05 + 0.8/2");
        assert!((s(60) - 0.25).abs() < 1e-6, "0.05 + 0.8/4");
        assert!(s(10_000) >= SALIENCE_FLOOR && s(10_000) - SALIENCE_FLOOR < 1e-7);
        // Clock step-back reads as the anchor.
        assert!((salience_at(0.85, T0, ep, 0.85, T0 - DAY) - 0.85).abs() < 1e-6);
    }

    #[test]
    fn below_floor_rows_stay_where_they_are() {
        assert!((salience_at(0.02, T0, 0, 0.02, T0 + 1000 * DAY) - 0.02).abs() < 1e-7);
        // Set below the floor after creation: stays, not raised.
        assert!((salience_at(0.01, T0, 0, 0.9, T0 + 1000 * DAY) - 0.01).abs() < 1e-7);
        assert_eq!(forward_weight(0.01, T0, 0, 0.9), 0.0);
        // Zero stays zero; a zero-initial row set above zero decays to ~0.
        assert_eq!(salience_at(0.0, T0, 0, 0.0, T0 + DAY), 0.0);
        assert!(salience_at(0.5, T0, 0, 0.0, T0 + 3000 * DAY) < 1e-9);
    }

    #[test]
    fn forward_form_equals_backward_form() {
        for (kind, anchor, initial, at) in [
            (0u8, 0.9f32, 0.9f32, T0),
            (1, 0.4, 0.3, T0 - 900 * DAY),
            (2, 1.0, 0.02, T0),
        ] {
            for days in [0u64, 1, 17, 200, 5000] {
                let now = at + days * DAY;
                let floor = f64::from(floor_for(initial, anchor));
                let fwd =
                    floor + forward_weight(anchor, at, kind, initial) * forward_scale(kind, now);
                let back = f64::from(salience_at(anchor, at, kind, initial, now));
                assert!((fwd - back).abs() < 1e-6, "{kind} {days}: {fwd} vs {back}");
            }
        }
    }
}
