//! A small deterministic generator (`SplitMix64`): bootstrap resamples and
//! toolset rolls must give the same numbers on every platform and build,
//! which a seeded `SplitMix64` does in a few lines.

use sha2::{Digest, Sha256};

/// `SplitMix64`.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator seeded with `seed`.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// A generator seeded from `seed` and `label` (a task id), so each
    /// task rolls independently yet reproducibly.
    #[must_use]
    pub fn derived(seed: u64, label: &str) -> Self {
        let mut hash = Sha256::new();
        hash.update(seed.to_le_bytes());
        hash.update(label.as_bytes());
        let digest = hash.finalize();
        let mut first = [0_u8; 8];
        first.copy_from_slice(&digest[..8]);
        Self(u64::from_le_bytes(first))
    }

    /// The next 64 random bits.
    pub const fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform number in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        // The top 53 bits make an exact f64 fraction.
        #[allow(clippy::cast_precision_loss)]
        let top = (self.next_u64() >> 11) as f64;
        top / 9_007_199_254_740_992.0
    }

    /// A uniform index below `n` (`n > 0`).
    pub fn below(&mut self, n: usize) -> usize {
        let n = u64::try_from(n).unwrap_or(u64::MAX).max(1);
        usize::try_from(self.next_u64() % n).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::Rng;

    #[test]
    fn the_same_seed_gives_the_same_numbers() {
        let a: Vec<u64> = (0..4)
            .map({
                let mut r = Rng::new(7);
                move |_| r.next_u64()
            })
            .collect();
        let mut b = Rng::new(7);
        assert_eq!(a, (0..4).map(|_| b.next_u64()).collect::<Vec<_>>());
        let mut r = Rng::derived(0, "t1");
        assert!((0..100).map(|_| r.unit()).all(|u| (0.0..1.0).contains(&u)));
    }
}
