//! Mem-Sequence's sequence (TODO 97): the passes a chunk gets, in order, each a write, a read, or
//! a read-then-write, each in an access order. Parsed once when the config loads; the test runs
//! each pass as one loop, matched once outside it.

/// What a pass does to each line it visits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Writes the next phase of the program
    Write,
    /// Checks the phase last written
    Read,
    /// Checks the phase last written, then writes its inverse in the same visit: the reads are
    /// checked in a stream with writes in it
    ReadWrite,
}

/// The order a pass visits a chunk's lines in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Forward,
    Reverse,
    /// Every line once, in an order scrambled anew each pass (`random_line`)
    Random,
    /// TM5's strided walk, the test's `parameter` giving the stride, as SimpleTest's
    Stride,
}

impl Order {
    pub fn parse(s: &str) -> Option<Order> {
        match s.to_ascii_lowercase().as_str() {
            "forward" => Some(Order::Forward),
            "reverse" => Some(Order::Reverse),
            "random" => Some(Order::Random),
            "stride" => Some(Order::Stride),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Order::Forward => "forward",
            Order::Reverse => "reverse",
            Order::Random => "random",
            Order::Stride => "stride",
        }
    }
}

/// One step: `count` passes of `op`, in `order` or the sequence's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub op: Op,
    pub count: u32,
    pub order: Option<Order>,
}

/// The most steps a sequence lists.
pub const MAX_STEPS: usize = 16;
/// The most passes one step makes.
pub const MAX_COUNT: u32 = 64;

/// A parsed sequence, e.g. `W 4R RW 2R@random`, and the order its steps take when they name none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sequence {
    steps: [Step; MAX_STEPS],
    len: u8,
    pub order: Order,
}

impl Default for Sequence {
    /// SimpleTest's shape: a write, then five reads
    fn default() -> Self {
        Sequence::parse("W 5R", Order::Forward).expect("a valid sequence")
    }
}

impl Sequence {
    /// From steps separated by spaces or commas: an optional count, `W`, `R` or `RW`, and an
    /// optional `@forward`, `@reverse`, `@random` or `@stride`.
    pub fn parse(text: &str, order: Order) -> Result<Self, String> {
        let mut steps = [Step { op: Op::Read, count: 1, order: None }; MAX_STEPS];
        let mut len = 0;
        for token in text.split(|c: char| c == ',' || c.is_whitespace()).filter(|t| !t.is_empty()) {
            if len == MAX_STEPS {
                return Err(format!("a sequence lists at most {MAX_STEPS} steps"));
            }
            let (body, step_order) = match token.split_once('@') {
                Some((body, o)) => (
                    body,
                    Some(Order::parse(o).ok_or_else(|| format!("'{token}': the order is forward, reverse, random or stride, not '{o}'"))?),
                ),
                None => (token, None),
            };
            let digits = body.find(|c: char| !c.is_ascii_digit()).unwrap_or(body.len());
            let count = match &body[..digits] {
                "" => 1,
                n => n.parse::<u32>().map_err(|_| format!("'{token}': a count of {n}"))?,
            };
            let op = match body[digits..].to_ascii_uppercase().as_str() {
                "W" => Op::Write,
                "R" => Op::Read,
                "RW" => Op::ReadWrite,
                "WR" => {
                    return Err(format!(
                        "'{token}': WR isn't a step: a read straight after a write of the same line \
                    is answered by the cache; W then R reads it from memory"
                    ));
                }
                other => return Err(format!("'{token}': a step is W, R or RW, with a count and an @order, not '{other}'")),
            };
            if !(1..=MAX_COUNT).contains(&count) {
                return Err(format!("'{token}': a step runs 1 to {MAX_COUNT} passes"));
            }
            steps[len] = Step { op, count, order: step_order };
            len += 1;
        }
        let seq = Sequence { steps, len: len as u8, order };
        if len == 0 {
            return Err("the sequence is empty: list W, R or RW steps".to_string());
        }
        if !seq.steps().iter().any(|s| s.op != Op::Write) {
            return Err(format!("'{text}' never reads: a correctness test must check what it writes"));
        }
        Ok(seq)
    }

    /// The same steps, the ones that name no order taking `order`.
    pub fn with_order(self, order: Order) -> Self {
        Sequence { order, ..self }
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps[..self.len as usize]
    }

    /// The order a step's passes take.
    pub fn order_of(&self, step: &Step) -> Order {
        step.order.unwrap_or(self.order)
    }

    /// Whether the first pass reads, so each chunk is written once before it.
    pub fn reads_first(&self) -> bool {
        self.steps[0].op != Op::Write
    }

    /// Whether any pass walks TM5's stride.
    pub fn strides(&self) -> bool {
        self.steps().iter().any(|s| self.order_of(s) == Order::Stride)
    }

    /// The program rounds a chunk's passes write over `reps` runs, the fill included (rounds `2n`
    /// and `2n + 1` are pair `n`'s P and ~P, `Program::phase`). A `W` writes the next round. An
    /// `RW` writes the inverse of the round it read, `c ^ 1`: the next round when it read a pair's
    /// P, else that pair's P again. So no `W` writes what an `RW` already left (the review: the
    /// second `W` of `W 2R RW 2R` rewrote ~P over ~P). `round_after` steps the same rule.
    pub fn rounds(&self, reps: u32) -> u32 {
        let (mut current, mut next) = (0u64, 0u64);
        if self.reads_first() {
            (current, next) = round_after(Op::Write, current, next);
        }
        for _ in 0..reps {
            for s in self.steps() {
                for _ in 0..s.count {
                    (current, next) = round_after(s.op, current, next);
                }
            }
        }
        next as u32
    }

    /// Passes a chunk gets over `reps` runs, the fill included, and the line visits that read and
    /// write in them: for bytes counted.
    pub fn visits(&self, reps: u32) -> (u64, u64) {
        let (mut reads, mut writes) = (0u64, u64::from(self.reads_first()));
        for s in self.steps() {
            let n = u64::from(s.count) * u64::from(reps);
            match s.op {
                Op::Write => writes += n,
                Op::Read => reads += n,
                Op::ReadWrite => (reads, writes) = (reads + n, writes + n),
            }
        }
        (reads, writes)
    }

    /// Whether some pass writes what no read checks: a write straight before a `W` (`2W R`,
    /// `RW W R`), or the last step's (`W R RW`). Allowed, as write stress; the run warns.
    pub fn unread_writes(&self) -> bool {
        let steps = self.steps();
        let writes = |s: &Step| s.op != Op::Read;
        steps.iter().any(|s| s.op == Op::Write && s.count > 1)
            || steps.windows(2).any(|w| writes(&w[0]) && w[1].op == Op::Write)
            || writes(&steps[steps.len() - 1])
    }

    /// `W 4R RW 2R@random`, the steps as given, with `@order` where a step names one.
    pub fn name(&self) -> String {
        self.steps()
            .iter()
            .map(|s| {
                let op = match s.op {
                    Op::Write => "W",
                    Op::Read => "R",
                    Op::ReadWrite => "RW",
                };
                let count = if s.count == 1 { String::new() } else { s.count.to_string() };
                let order = s.order.map_or(String::new(), |o| format!("@{}", o.name()));
                format!("{count}{op}{order}")
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The round a chunk holds after a pass of `op`, and the next round a `W` writes, from the round it
/// held and the next (`Sequence::rounds`). A read changes neither.
#[inline]
pub fn round_after(op: Op, current: u64, next: u64) -> (u64, u64) {
    match op {
        Op::Write => (next, next + 1),
        Op::Read => (current, next),
        Op::ReadWrite => {
            let wrote = current ^ 1;
            (wrote, if wrote == next { next + 1 } else { next })
        }
    }
}

/// Line `j`'s place in a random pass over a block of `2^bits` lines: a bijection of `j` on
/// `0..2^bits` (odd multiplies and right xorshifts, each invertible there) keyed by `key`, so every
/// line of the block comes up once and consecutive visits land far apart.
#[inline(always)]
pub fn random_line(j: u64, bits: u32, key: u64) -> u64 {
    let mask = (1u64 << bits).wrapping_sub(1) | u64::from(bits == 64).wrapping_neg();
    let s = bits.div_ceil(2).max(1);
    let mut x = (j ^ key) & mask;
    x = x.wrapping_mul(0x9E37_79B9_7F4A_7C15) & mask;
    x ^= x >> s;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9) & mask;
    x ^= x >> s;
    x = x.wrapping_add(key >> 32) & mask;
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing() {
        let seq = Sequence::parse("W 2R RW 2R RW@random", Order::Forward).unwrap();
        assert_eq!(seq.name(), "W 2R RW 2R RW@random");
        assert_eq!(seq.steps()[4], Step { op: Op::ReadWrite, count: 1, order: Some(Order::Random) });
        assert_eq!(seq.order_of(&seq.steps()[0]), Order::Forward);
        assert_eq!(Sequence::parse("w,4r,rw@Reverse", Order::Random).unwrap().name(), "W 4R RW@reverse");
        assert!(!seq.reads_first() && !seq.strides() && seq.unread_writes(), "the last RW's write isn't read");
        assert!(!Sequence::parse("W 2R RW 2R", Order::Forward).unwrap().unread_writes());
        // A run: W round 0 (P), RW round 1 (~P, a round), RW back to round 0's P (not one)
        assert_eq!((seq.rounds(4), seq.visits(4)), (8, (6 * 4, 3 * 4)));
        let leading = Sequence::parse("5R W", Order::Forward).unwrap();
        assert_eq!((leading.reads_first(), leading.rounds(2)), (true, 3));
        let rounds = |text: &str, reps| Sequence::parse(text, Order::Forward).unwrap().rounds(reps);
        assert_eq!([rounds("W 5R", 4), rounds("W 2R RW 2R", 2), rounds("W RW RW R", 1), rounds("R RW R", 1)], [4, 4, 2, 2]);
        assert!(Sequence::parse("2W R", Order::Forward).unwrap().unread_writes());
        assert!(Sequence::parse("W R W", Order::Forward).unwrap().unread_writes(), "the last write");
        assert!(Sequence::parse("W RW W R", Order::Forward).unwrap().unread_writes(), "RW's write, overwritten");
        assert!(!Sequence::parse("3X", Order::Forward).unwrap_err().contains("cache"), "the WR note is for WR");
        for (bad, says) in [
            ("", "empty"),
            ("W 2W", "never reads"),
            ("WR", "cache"),
            ("3X", "W, R or RW"),
            ("R@sideways", "forward, reverse"),
            ("0R", "1 to 64"),
            ("65R", "1 to 64"),
        ] {
            let err = Sequence::parse(bad, Order::Forward).unwrap_err();
            assert!(err.contains(says), "'{bad}': {err}");
        }
        assert_eq!(Sequence::default().name(), "W 5R");
    }

    /// A random pass visits every line of its block once, for any size and key.
    #[test]
    fn random_lines_are_a_permutation() {
        for bits in [0, 1, 6, 10, 15] {
            for key in [0, 1, 0xDEAD_BEEF_1234_5678, u64::MAX] {
                let n = 1u64 << bits;
                let mut seen = vec![false; n as usize];
                for j in 0..n {
                    let l = random_line(j, bits, key);
                    assert!(l < n && !seen[l as usize], "{bits} bits, key {key:#x}: line {l} twice or out of the block");
                    seen[l as usize] = true;
                }
            }
        }
        // Consecutive visits mostly land in other 8 KiB rows' worth of lines (128 lines)
        let far = (0..4095u64).filter(|&j| random_line(j, 12, 7).abs_diff(random_line(j + 1, 12, 7)) >= 128).count();
        assert!(far > 3500, "{far} of 4095 steps went far");
    }
}
