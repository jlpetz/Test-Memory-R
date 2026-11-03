//! The latency tests' percentiles from a histogram, not a sort (TODO 105 (1), 2026-11-04).
//!
//! A sample is a latency in ns (one timed block of chained loads, averaged). Each thread keeps a count per bucket, never the samples: a
//! bucket is a positive f64's exponent and the top `BITS` bits of its mantissa, so each power of two is split into 2^`BITS` equal buckets,
//! each 2^-`BITS` of its octave wide (0.098% at 10 bits), whatever the latency. A percentile is the middle of the bucket that holds the
//! sample a sort would pick, so it is off by at most half a bucket: 2^-(`BITS`+1) of the value, 0.049% at 10 bits (0.0007 ns at L1's
//! 1.4 ns, 0.07 ns at DRAM's 140 ns). The exact minimum and maximum are kept beside the counts, and are P0 and P100. The coordinator adds
//! the threads' counts up (`merge`) for the step's percentiles, the same figure a sort of every sample gives, where the sort took every
//! sample (about 57 GB at 192 threads) and minutes on one thread.
//!
//! Checked against the sort on the same samples (the 8500G, 2026-11-04, 18 steps from L1 to DRAMFull): every percentile within
//! 0.020-0.043%, the same at the console's 0.1 ns; at L1 (55 M samples) the fill took 0.046 s against the sort's 0.65-0.70 s.

/// Mantissa bits kept: 2^BITS buckets an octave
pub const BITS: u32 = 10;
const SHIFT: u32 = 52 - BITS;
/// The range kept apart, 2^-4 ns (0.06 ns) to 2^24 ns (17 ms): below and above it the edge buckets take the samples (the exact minimum
/// and maximum still hold them)
const LOW_EXPONENT: i32 = -4;
const HIGH_EXPONENT: i32 = 24;
// 2^e is the f64 with exponent field 1023 + e and a zero mantissa: its key is that field shifted past the kept bits
const LOW_KEY: u64 = ((1023 + LOW_EXPONENT) as u64) << BITS;
const HIGH_KEY: u64 = ((1023 + HIGH_EXPONENT) as u64) << BITS;
/// Buckets: 28 octaves of 1024
pub const BUCKETS: usize = (HIGH_KEY - LOW_KEY) as usize;

#[derive(Clone)]
pub struct Histogram {
    counts: Vec<u64>,
    samples: u64,
    min: f64,
    max: f64,
}

/// Its counts would fill a screen: what it holds instead
impl std::fmt::Debug for Histogram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Histogram").field("samples", &self.samples).field("min", &self.min).field("max", &self.max).finish()
    }
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    pub fn new() -> Self {
        Self { counts: vec![0; BUCKETS], samples: 0, min: f64::INFINITY, max: f64::NEG_INFINITY }
    }

    /// The bucket of `ns`: a positive f64's bits order as its value does, so its top bits are its bucket. Zero, negative and NaN go to the
    /// first, past the range to the last.
    #[inline]
    fn bucket(ns: f64) -> usize {
        if ns.is_nan() || ns <= 0.0 {
            return 0;
        }
        ((ns.to_bits() >> SHIFT).clamp(LOW_KEY, HIGH_KEY - 1) - LOW_KEY) as usize
    }

    /// The middle of bucket `i`: its key with the next mantissa bit set (an octave's mantissa is linear in the value)
    fn middle(i: usize) -> f64 {
        f64::from_bits(((i as u64 + LOW_KEY) << SHIFT) | (1u64 << (SHIFT - 1)))
    }

    #[inline]
    pub fn add(&mut self, ns: f64) {
        self.counts[Self::bucket(ns)] += 1;
        self.samples += 1;
        self.min = self.min.min(ns);
        self.max = self.max.max(ns);
    }

    pub fn merge(&mut self, other: &Histogram) {
        for (mine, theirs) in self.counts.iter_mut().zip(&other.counts) {
            *mine += theirs;
        }
        self.samples += other.samples;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
    }

    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// The smallest sample, exact: 0 with none
    pub fn min(&self) -> f64 {
        if self.samples == 0 { 0.0 } else { self.min }
    }

    /// The `p`th percentile, of the sample the sort picks (`sorted[floor((n - 1) * p / 100)]`): the middle of its bucket, within the exact
    /// minimum and maximum, which P0 and P100 are
    pub fn percentile(&self, p: f64) -> f64 {
        if self.samples == 0 {
            return 0.0;
        }
        let k = ((self.samples as f64 - 1.0) * p / 100.0) as u64;
        // The first and the last sample are the exact extremes
        if k == 0 {
            return self.min;
        }
        if k + 1 >= self.samples {
            return self.max;
        }
        let mut seen = 0u64;
        for (i, &count) in self.counts.iter().enumerate() {
            seen += count;
            if seen > k {
                return Self::middle(i).clamp(self.min, self.max);
            }
        }
        self.max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A percentile is within half a bucket of the sort's, 2^-(BITS+1) of the value, across the latencies TMR sees (0.5 ns to 2 µs), and
    /// histograms merge to what one histogram of every sample gives
    #[test]
    fn its_percentiles_are_within_half_a_bucket_of_the_sorts() {
        // A spread of latencies, as a step's threads give them: deterministic, uneven, over 12 octaves
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut samples: Vec<f64> = (0..200_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                0.5 * 2f64.powf((state % 12_000) as f64 / 1000.0)
            })
            .collect();
        let (mut whole, mut a, mut b) = (Histogram::new(), Histogram::new(), Histogram::new());
        for (i, &ns) in samples.iter().enumerate() {
            whole.add(ns);
            if i % 3 == 0 { a.add(ns) } else { b.add(ns) }
        }
        a.merge(&b);
        samples.sort_by(f64::total_cmp);
        let bound = 1.0 / f64::from(1u32 << (BITS + 1));
        for p in [0.0, 1.0, 5.0, 10.0, 25.0, 50.0, 75.0, 90.0, 95.0, 99.0, 99.9, 100.0] {
            let exact = samples[((samples.len() as f64 - 1.0) * p / 100.0) as usize];
            let got = whole.percentile(p);
            assert!(((got - exact) / exact).abs() <= bound, "P{p}: {got} against {exact}");
            assert_eq!(a.percentile(p), got, "P{p}: merged");
        }
        assert_eq!((whole.samples(), whole.percentile(0.0), whole.percentile(100.0)), (200_000, samples[0], samples[199_999]));
    }

    /// The edges: nothing is 0; one sample is itself; past the range, the exact extremes
    #[test]
    fn its_edges() {
        assert_eq!(Histogram::new().percentile(50.0), 0.0);
        let mut one = Histogram::new();
        one.add(1.37);
        assert_eq!(one.percentile(50.0), 1.37, "clamped to the minimum and maximum, which are the sample");
        let mut far = Histogram::new();
        far.add(1e-9);
        far.add(1e12);
        assert_eq!((far.percentile(0.0), far.percentile(100.0)), (1e-9, 1e12));
        assert_eq!(Histogram::bucket(-1.0), 0);
        assert_eq!(Histogram::bucket(f64::NAN), 0);
        assert_eq!(BUCKETS, 28 * 1024);
    }
}
