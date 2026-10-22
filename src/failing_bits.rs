//! Which bit positions failed (TODO 97). Counted on the verifies' cold path only, after a check
//! has failed, and reported by step and over the run: one bit failing everywhere points at one
//! data line, many bits at the timings.

use serde::{Deserialize, Serialize};

/// One bit position's failures: the failed checks whose ORed differences had it, and the bad words
/// rescans found with it wrong, by the way it went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BitFailures {
    pub bit: u8,
    pub checks: u64,
    pub words_0to1: u64,
    pub words_1to0: u64,
}

impl BitFailures {
    fn words(&self) -> u64 {
        self.words_0to1 + self.words_1to0
    }
}

/// Bits a summary row lists; the rest it counts.
const LISTED_BITS: usize = 8;
/// Bits a step line names; more and it gives their number.
const NAMED_BITS: usize = 4;

/// The bits that failed in some checks, in bit order: none for a clean run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FailingBits(Vec<BitFailures>);

impl FailingBits {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn at(&mut self, bit: u32) -> &mut BitFailures {
        let at = match self.0.binary_search_by_key(&bit, |b| u32::from(b.bit)) {
            Ok(at) => at,
            Err(at) => {
                self.0.insert(at, BitFailures { bit: bit as u8, checks: 0, words_0to1: 0, words_1to0: 0 });
                at
            }
        };
        &mut self.0[at]
    }

    /// A failed check, whose differences ORed to `mask`.
    pub(crate) fn add_check(&mut self, mask: u64) {
        for bit in set_bits(mask) {
            self.at(bit).checks += 1;
        }
    }

    /// A bad word: `actual` where `want` was expected. A rescan counts its words in a `WordTally`
    /// instead, which a chunk of bad words doesn't slow. A test's, and the final summary's fixture
    /// (`test_support`, TODO 29).
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn add_word(&mut self, want: u64, actual: u64) {
        let mut tally = WordTally::default();
        tally.add(want, actual);
        self.add_words(&tally);
    }

    /// A rescan's bad words.
    pub(crate) fn add_words(&mut self, tally: &WordTally) {
        for bit in 0..64 {
            let (up, down) = (tally.up[bit], tally.down[bit]);
            if up + down > 0 {
                let b = self.at(bit as u32);
                (b.words_0to1, b.words_1to0) = (b.words_0to1 + up, b.words_1to0 + down);
            }
        }
    }

    pub(crate) fn add(&mut self, other: &FailingBits) {
        for b in &other.0 {
            let to = self.at(u32::from(b.bit));
            to.checks += b.checks;
            to.words_0to1 += b.words_0to1;
            to.words_1to0 += b.words_1to0;
        }
    }

    /// For a step line, after its error count: ` (bit 31)`, ` (bits 7, 31)`, ` (17 bits)`; empty
    /// when none failed.
    pub fn brief(&self) -> String {
        match self.0.len() {
            0 => String::new(),
            1 => format!(" (bit {})", self.0[0].bit),
            n if n > NAMED_BITS => format!(" ({n} bits)"),
            _ => format!(" (bits {})", self.0.iter().map(|b| b.bit.to_string()).collect::<Vec<_>>().join(", ")),
        }
    }

    /// For a summary row: `31: 99 / 1 (1 + 0), 39: 3 / 0`, each bit's failed checks and bad words
    /// (0→1 + 1→0). More than `LISTED_BITS` bits: the most failed, in bit order, and how many more.
    pub fn detail(&self) -> String {
        let mut listed: Vec<&BitFailures> = self.0.iter().collect();
        if listed.len() > LISTED_BITS {
            listed.sort_by_key(|b| (std::cmp::Reverse((b.checks, b.words())), b.bit));
            listed.truncate(LISTED_BITS);
            listed.sort_by_key(|b| b.bit);
        }
        let mut cells: Vec<String> = listed.iter().map(|b| match b.words() {
            0 => format!("{}: {} / 0", b.bit, b.checks),
            n => format!("{}: {} / {n} ({} + {})", b.bit, b.checks, b.words_0to1, b.words_1to0),
        }).collect();
        match self.0.len().saturating_sub(LISTED_BITS) {
            0 => {}
            1 => cells.push("1 more bit".to_string()),
            more => cells.push(format!("{more} more bits")),
        }
        cells.join(", ")
    }
}

fn set_bits(mut mask: u64) -> impl Iterator<Item = u32> {
    std::iter::from_fn(move || {
        (mask != 0).then(|| {
            let bit = mask.trailing_zeros();
            mask &= mask - 1;
            bit
        })
    })
}

/// A rescan's bad words by bit and way: a fixed table, so a chunk of bad words costs a count per
/// wrong bit, not a search (the second review: a chunk of 4 MiB of bad words took 248 ms with the
/// search, 14 ms with this).
#[derive(Debug, Clone)]
pub struct WordTally {
    up: [u64; 64],
    down: [u64; 64],
}

impl Default for WordTally {
    fn default() -> Self {
        WordTally { up: [0; 64], down: [0; 64] }
    }
}

impl WordTally {
    /// A bad word: `actual` where `want` was expected.
    #[inline]
    pub fn add(&mut self, want: u64, actual: u64) {
        for bit in set_bits(actual & !want) {
            self.up[bit as usize] += 1;
        }
        for bit in set_bits(want & !actual) {
            self.down[bit as usize] += 1;
        }
    }
}

/// One work item's failing bits on one worker: its test's own checks, and the seal's.
#[derive(Debug, Clone, Default)]
pub struct CheckBits {
    pub test: FailingBits,
    pub seal: FailingBits,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_count_each_bit_and_words_each_way() {
        let mut bits = FailingBits::default();
        bits.add_check(1 << 31);
        bits.add_check(1 << 31 | 1 << 7);
        bits.add_word(0, 1 << 31);
        bits.add_word(u64::MAX, !(1 << 7));
        assert_eq!(bits.0, [
            BitFailures { bit: 7, checks: 1, words_0to1: 0, words_1to0: 1 },
            BitFailures { bit: 31, checks: 2, words_0to1: 1, words_1to0: 0 },
        ]);
        assert_eq!(bits.brief(), " (bits 7, 31)");
        assert_eq!(bits.detail(), "7: 1 / 1 (0 + 1), 31: 2 / 1 (1 + 0)");
        let mut sum = bits.clone();
        sum.add(&bits);
        assert_eq!(sum.0[1], BitFailures { bit: 31, checks: 4, words_0to1: 2, words_1to0: 0 });
    }

    #[test]
    fn many_bits_are_summarised() {
        let mut bits = FailingBits::default();
        assert_eq!((bits.brief(), bits.detail()), (String::new(), String::new()));
        bits.add_check(1 << 31);
        assert_eq!((bits.brief(), bits.detail()), (" (bit 31)".to_string(), "31: 1 / 0".to_string()));
        // Bits 0 to 9 and 31, bit 31 failing most: the step line counts them, the row lists the
        // most failed
        bits.add_check(0x3ff);
        bits.add_check(1 << 31);
        bits.add_check(1 << 31);
        assert_eq!(bits.brief(), " (11 bits)");
        assert_eq!(bits.detail(), "0: 1 / 0, 1: 1 / 0, 2: 1 / 0, 3: 1 / 0, 4: 1 / 0, 5: 1 / 0, 6: 1 / 0, 31: 3 / 0, 3 more bits");
    }

    #[test]
    fn it_reads_back_as_written() {
        let mut bits = FailingBits::default();
        bits.add_check(1 << 31);
        bits.add_word(0, 1 << 31);
        let json = serde_json::to_string(&bits).unwrap();
        assert_eq!(json, r#"[{"bit":31,"checks":1,"words_0to1":1,"words_1to0":0}]"#);
        assert_eq!(serde_json::from_str::<FailingBits>(&json).unwrap(), bits);
    }
}
