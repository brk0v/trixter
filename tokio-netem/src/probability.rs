//! Probability knobs for tokio-netem failure injectors.
//!
//! This module defines the [`Probability`] trait used by adapters such as [`crate::terminator::Terminator`]
//! and [`crate::corrupter::Corrupter`], along with dynamic handles for tweaking rates at runtime.
//!
//! ## Deterministic testing
//! - Pass a bare `f64` when you want a fixed probability without heap or atomic overhead.
//! - Use [`DynamicProbability::new`] and [`DynamicProbability::set`] for runtime-tunable knobs.
//! - Implement [`Probability`] yourself to bridge to custom configuration sources.
use std::{
    error::Error,
    fmt, io,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use rand::{rngs::SmallRng, RngCore};

/// Source of termination probability for [`Terminator`].
///
/// Implementations provide both the **floating-point probability** (0.0–1.0) and a
/// precomputed **threshold** in `[0, u64::MAX]` used to compare against random
/// `u64` draws. The threshold form avoids repeated floating-point math on hot paths.
pub trait Probability: Unpin {
    /// Returns the probability in the inclusive range `0.0..=1.0`.
    fn probability(&self) -> f64;

    /// Returns a precomputed threshold in `[0, u64::MAX]`. A random `u64` draw
    /// from the RNG triggers termination iff it is **strictly less** than this value.
    fn threshold(&self) -> u64;
}

impl Probability for f64 {
    fn probability(&self) -> f64 {
        match *self {
            x if x.is_nan() => 0.0, // handle NaN explicitly
            ..=0.0 => 0.0,
            0.0..=1.0 => *self,
            _ => 1.0,
        }
    }

    fn threshold(&self) -> u64 {
        let p = self.probability(); // already clamped to [0,1] and NaN->0
        (p * u64::MAX as f64) as u64
    }
}

/// Error returned when a probability lies outside the inclusive `0.0..=1.0` range.
#[derive(Debug)]
pub struct DynamicProbabilityError;

impl fmt::Display for DynamicProbabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "probability rate must be 0.0–1.0")
    }
}

impl Error for DynamicProbabilityError {}

/// Lock-free, shareable probability knob for runtime updates.
///
/// Holds:
/// - the raw probability as `f64` bits (to avoid tearing due to NaN payloads), and
/// - a precomputed threshold in `[0, u64::MAX]` so the hot path does only an integer compare.
///
/// Clones of `Arc<DynamicProbability>` observe updates immediately. Stores use
/// `Release` ordering and loads use `Acquire` ordering to ensure readers see
/// a consistent pair of `(rate_bits, threshold)`.
#[derive(Debug, Default)]
pub struct DynamicProbability {
    probability_rate_bits: AtomicU64,
    // precomputed threshold in [0, u64::MAX]; next_u64() < threshold triggers termination
    probability_threshold: AtomicU64,
}

impl DynamicProbability {
    /// Creates a new handle with the given `probability` in `0.0..=1.0`.
    ///
    /// Returns `Err(InvalidInput)` if the value is out of range.
    pub fn new(probability: f64) -> io::Result<Arc<Self>> {
        validate_probability_rate(probability)?;

        Ok(Arc::new(Self {
            probability_rate_bits: AtomicU64::new(probability.to_bits()),
            probability_threshold: AtomicU64::new(
                ((probability.clamp(0.0, 1.0)) * u64::MAX as f64) as u64,
            ),
        }))
    }

    /// Updates the probability at runtime.
    ///
    /// On success, all `Terminator` instances holding this `Arc` observe the new rate and
    /// threshold without locks. Returns `Err(InvalidInput)` if the value is out of range.
    pub fn set(&self, probability: f64) -> io::Result<()> {
        validate_probability_rate(probability)?;

        self.probability_rate_bits
            .store(probability.to_bits(), Ordering::Release);
        let probability = ((probability.clamp(0.0, 1.0)) * u64::MAX as f64) as u64;
        self.probability_threshold
            .store(probability, Ordering::Release);

        Ok(())
    }
}

impl Probability for DynamicProbability {
    fn probability(&self) -> f64 {
        f64::from_bits(self.probability_rate_bits.load(Ordering::Acquire))
    }

    fn threshold(&self) -> u64 {
        self.probability_threshold.load(Ordering::Acquire)
    }
}

impl Probability for Arc<DynamicProbability> {
    fn probability(&self) -> f64 {
        f64::from_bits(self.probability_rate_bits.load(Ordering::Acquire))
    }

    fn threshold(&self) -> u64 {
        self.probability_threshold.load(Ordering::Acquire)
    }
}

pub(crate) fn validate_probability_rate(probability_rate: f64) -> io::Result<()> {
    if !(0.0..=1.0).contains(&probability_rate) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            DynamicProbabilityError,
        ));
    }
    Ok(())
}

#[inline]
pub(crate) fn try_trigger<P: Probability>(
    triggered: &mut bool,
    rng: &mut SmallRng,
    prob: &mut P,
) -> bool {
    if *triggered {
        return true;
    }
    let th = prob.threshold();
    if th == 0 {
        return false;
    }
    if rng.next_u64() < th {
        *triggered = true;
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use std::io;

    fn expected_threshold(probability: f64) -> u64 {
        ((probability.clamp(0.0, 1.0)) * u64::MAX as f64) as u64
    }

    #[test]
    fn dynamic_probability_new_within_range_initializes_fields() {
        let knob = DynamicProbability::new(0.25).expect("0.25 should be accepted");

        assert_eq!(knob.probability(), 0.25);
        assert_eq!(knob.threshold(), expected_threshold(0.25));
    }

    #[test]
    fn dynamic_probability_new_rejects_out_of_range() {
        let err = DynamicProbability::new(1.1).expect_err("1.1 should be rejected");

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn dynamic_probability_set_updates_shared_state() {
        let knob = DynamicProbability::new(0.1).expect("0.1 should be accepted");

        knob.set(0.9).expect("0.9 should be accepted");

        assert_eq!(knob.probability(), 0.9);
        assert_eq!(knob.threshold(), expected_threshold(0.9));
    }

    #[test]
    fn validate_probability_rate_bounds_check() {
        assert!(validate_probability_rate(0.0).is_ok());
        assert!(validate_probability_rate(1.0).is_ok());

        let low_err = validate_probability_rate(-0.01).expect_err("negative values should error");
        assert_eq!(low_err.kind(), io::ErrorKind::InvalidInput);

        let high_err = validate_probability_rate(1.01).expect_err("values > 1.0 should error");
        assert_eq!(high_err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn try_trigger_with_zero_threshold_never_sets_flag() {
        let mut triggered = false;
        let mut rng = SmallRng::from_seed([0u8; 32]);
        let mut prob = 0.0_f64;

        assert!(!try_trigger(&mut triggered, &mut rng, &mut prob));
        assert!(!triggered);
    }

    #[test]
    fn try_trigger_sets_flag_and_short_circuits() {
        let mut triggered = false;
        let mut rng = SmallRng::from_seed([1u8; 32]);
        let mut prob = 1.0_f64;

        assert!(try_trigger(&mut triggered, &mut rng, &mut prob));
        assert!(triggered);

        // Once triggered, subsequent calls should keep reporting true without RNG influence.
        assert!(try_trigger(&mut triggered, &mut rng, &mut prob));
    }

    #[test]
    fn probability_for_f64_clamps_threshold_only() {
        let below = -5.0_f64;
        assert_eq!(below.probability(), 0.0);
        assert_eq!(below.threshold(), 0);

        let above = 2.0_f64;
        assert_eq!(above.probability(), 1.0);
        assert_eq!(above.threshold(), u64::MAX);
    }
}
