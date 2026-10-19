//! The work a step does around its test sequence, by operation (TODO 90): the seal's fills and
//! checks, a test's own pattern fill, a latency test's chain build. Every record has the same
//! shape, a time and the bytes it covered, and is named by its `Op`. The step report, the results
//! file and the final summary's table list them from `Op::ALL`, so a new kind of work is a new
//! variant here and nothing else.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Op {
    /// The seal written over memory: the whole span at a cycle's start, each chunk after its test,
    /// a step's extent at its end, what a step without the seal left unsealed
    SealFill,
    /// The seal checked: before each chunk, before a step that overwrites it, at a cycle's end
    SealCheck,
    /// A test's own fill before its sequence: the harness's, Mem-Random's, BlockMove's source,
    /// a bandwidth test's first touch
    PatternFill,
    /// A latency test's pointer chain or store table: time only
    ChainBuild,
}

impl Op {
    pub const ALL: [Op; 4] = [Op::SealFill, Op::SealCheck, Op::PatternFill, Op::ChainBuild];

    /// Its name in every report
    pub fn name(self) -> &'static str {
        match self {
            Op::SealFill => "Seal fill",
            Op::SealCheck => "Seal check",
            Op::PatternFill => "Pattern fill",
            Op::ChainBuild => "Chain build",
        }
    }

    /// Its key in the results file
    pub fn key(self) -> &'static str {
        match self {
            Op::SealFill => "seal_fill",
            Op::SealCheck => "seal_check",
            Op::PatternFill => "pattern_fill",
            Op::ChainBuild => "chain_build",
        }
    }

    /// Whether its bytes mean anything: a chain build's speed isn't a bandwidth
    pub fn has_bytes(self) -> bool {
        self != Op::ChainBuild
    }
}

/// One operation's work: how long it took and the bytes it covered.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct OpWork {
    pub time: Duration,
    pub bytes: u64,
}

impl OpWork {
    pub fn is_empty(&self) -> bool {
        self.time.is_zero() && self.bytes == 0
    }

    /// MiB/s, when it covered bytes in measurable time
    pub fn mib_s(&self) -> Option<f64> {
        let secs = self.time.as_secs_f64();
        (self.bytes > 0 && secs > 0.0).then(|| self.bytes as f64 / (1024.0 * 1024.0) / secs)
    }
}

/// A step's (or a run's) work around its test sequence: one record an `Op`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct OpTally([OpWork; Op::ALL.len()]);

impl OpTally {
    pub fn get(&self, op: Op) -> OpWork {
        self.0[op as usize]
    }

    pub fn add(&mut self, op: Op, time: Duration, bytes: u64) {
        let work = &mut self.0[op as usize];
        work.time += time;
        work.bytes += bytes;
    }

    /// Another thread's, in the same step: the threads run at once, so the bytes add and the time
    /// is the slowest thread's, as the test's own figures are.
    pub fn merge_thread(&mut self, other: &OpTally) {
        for (mine, theirs) in self.0.iter_mut().zip(&other.0) {
            mine.time = mine.time.max(theirs.time);
            mine.bytes += theirs.bytes;
        }
    }

    /// Another step's, or a cycle's: one after the other, so both add.
    pub fn add_all(&mut self, other: &OpTally) {
        for op in Op::ALL {
            let theirs = other.get(op);
            self.add(op, theirs.time, theirs.bytes);
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (Op, OpWork)> + '_ {
        Op::ALL.into_iter().map(|op| (op, self.get(op)))
    }

    /// The step report's form: every operation, `n/a` where the step did none.
    /// `seal fill 0.4s, 28.00 GiB @ 68266 MiB/s; seal check 0.5s, …; pattern fill n/a; chain build n/a`
    pub fn line(&self) -> String {
        self.iter()
            .map(|(op, work)| {
                let name = op.name().to_lowercase();
                match work.mib_s() {
                    _ if work.is_empty() => format!("{name} n/a"),
                    Some(speed) if op.has_bytes() => format!(
                        "{name} {:.1}s, {:.2} GiB @ {speed:.0} MiB/s",
                        work.time.as_secs_f64(),
                        work.bytes as f64 / (1u64 << 30) as f64
                    ),
                    _ => format!("{name} {:.1}s", work.time.as_secs_f64()),
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threads_merge_by_the_slowest_and_steps_add() {
        let ms = Duration::from_millis;
        let (mut step, mut other) = (OpTally::default(), OpTally::default());
        step.add(Op::SealCheck, ms(400), 1 << 30);
        other.add(Op::SealCheck, ms(450), 1 << 30);
        step.merge_thread(&other);
        assert_eq!(step.get(Op::SealCheck), OpWork { time: ms(450), bytes: 2 << 30 });
        let mut run = OpTally::default();
        run.add_all(&step);
        run.add_all(&step);
        assert_eq!(run.get(Op::SealCheck), OpWork { time: ms(900), bytes: 4 << 30 });
        assert!(run.get(Op::PatternFill).is_empty() && !run.get(Op::SealCheck).is_empty());
        let mib_s = run.get(Op::SealCheck).mib_s().unwrap();
        assert!((mib_s - 4096.0 / 0.9).abs() < 1e-6, "{mib_s}");
        run.add(Op::ChainBuild, ms(120), 0);
        assert_eq!(run.line(), "seal fill n/a; seal check 0.9s, 4.00 GiB @ 4551 MiB/s; pattern fill n/a; chain build 0.1s");
    }
}
