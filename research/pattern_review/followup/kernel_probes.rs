//! Small heap/stack probes appended only to the copied tests.rs.
use super::*;
use crate::pattern::{Family, Phase, Program, StepEpoch, Schedule};
use crate::pattern_gen::BusBeat;
use crate::memory::allocator::AllocationConfig;
use crate::memory::backend::{AllocError, Backend, BackendAllocation};
use crate::memory::buffer::{BufferInfo, MemoryBuffer, PageType};
use crate::runner::AllocationBlock;
use crate::layout::BlockInfo;

const CANARY: u64 = 0x1357_2468_9abc_def0;

#[repr(align(4096))]
struct Buffer([u64; 1024]);

#[derive(Debug)]
struct HeapView;
impl Backend for HeapView {
    fn allocate(&self, _: &AllocationConfig) -> Result<BackendAllocation, AllocError> {
        unreachable!("ordinary memory owned by the test")
    }
    fn free(&self, _: BackendAllocation) -> Result<(), String> { Ok(()) }
    fn name(&self) -> &'static str { "review-heap" }
}

fn blocks(data: &mut Buffer) -> Vec<AllocationBlock> {
    vec![AllocationBlock {
        buffer: MemoryBuffer::new(
            BackendAllocation {
                ptr: data.0.as_mut_ptr() as *mut u8,
                size: std::mem::size_of_val(&data.0),
                info: BufferInfo { numa_node: 0, page_type: PageType::Regular(4096) },
            },
            std::sync::Arc::new(HeapView),
        ),
        block_info: BlockInfo { size_bytes: std::mem::size_of_val(&data.0), thread_id: 0 },
    }]
}

type Kernel = unsafe fn(*mut u64, usize, usize, u64, Option<usize>, Phase, bool) -> u64;

macro_rules! probe_kernel {
    ($name:ident, $tf:literal, [$($kind:tt)*]) => {
        #[target_feature(enable = $tf)]
        unsafe fn $name(ptr: *mut u64, start: usize, end: usize, lid: u64,
                       stride: Option<usize>, phase: Phase, write: bool) -> u64 {
            if write {
                native_write!([$($kind)*], ptr, start, end, lid, stride, phase);
                0
            } else {
                native_differs!([$($kind)*], ptr, start, end, lid, stride, phase)
            }
        }
    };
}

probe_kernel!(scalar, "avx2", [scalar]);
probe_kernel!(w128, "sse2", [u64x2, 2]);
probe_kernel!(w256, "avx2", [u64x4, 4]);
probe_kernel!(w512, "avx512f,avx512bw,avx512cd,avx512dq,avx512vl", [u64x8, 8]);

fn kernels() -> Vec<(&'static str, Kernel)> {
    let mut out: Vec<(&str, Kernel)> = vec![("scalar", scalar), ("128", w128), ("256", w256)];
    if is_x86_feature_detected!("avx512f") {
        out.push(("512", w512));
    }
    out
}

#[test]
fn review_every_native_word_detects_injected_bits_including_tails() {
    let mut data = Box::new(Buffer([CANARY; 1024]));
    let (start, end) = (8usize, 8 + 37 * 8); // Nonzero start, 37-line tail.
    let lid = crate::pattern::line_base(3) + (start / 8) as u64;
    let ptr = data.0.as_mut_ptr();
    let mut comparisons = 0usize;
    for family in Family::ALL {
        for beat in [BusBeat::Bits12, BusBeat::Bits16, BusBeat::Bits24, BusBeat::Bits32, BusBeat::Bits64] {
            if family != Family::Switch && beat != BusBeat::Bits32 { continue; }
            let phase = Phase { family, key: 0x0123_4567_89ab_cdef, mask: !0, beat };
            let reference: Vec<u64> = (0..37).flat_map(|n| phase.line(lid + n)).collect();
            for period in [None, Some(24), Some(72)] {
                for (name, kernel) in kernels() {
                    data.0.fill(CANARY);
                    unsafe { kernel(ptr, start, end, lid, period, phase, true); }
                    assert_eq!(&data.0[start..end], reference, "{name} {phase:?} {period:?}");
                    assert!(data.0[..start].iter().chain(&data.0[end..]).all(|&x| x == CANARY));
                    assert_eq!(unsafe { kernel(ptr, start, end, lid, period, phase, false) }, 0);
                    let mut index = start;
                    while index < end {
                        let bit = 1u64 << ((index * 17) % 64);
                        unsafe { *ptr.add(index) ^= bit; }
                        assert_eq!(
                            unsafe { kernel(ptr, start, end, lid, period, phase, false) },
                            bit, "{name} {phase:?} {period:?} index {index}"
                        );
                        unsafe { *ptr.add(index) ^= bit; }
                        comparisons += 1;
                        index += 1;
                    }
                    // Two equal corruptions must not cancel as an XOR checksum would.
                    data.0[start] ^= 1;
                    data.0[end - 1] ^= 1;
                    assert_eq!(unsafe { kernel(ptr, start, end, lid, period, phase, false) }, 1);
                    comparisons += 1;
                }
            }
        }
    }
    println!("PASS: {comparisons} corruption checks over all available widths, five families, three walks and tail lines.");
}

#[test]
fn review_clean_rescan_still_fails_and_rescan_off_does_not_claim_reproduction() {
    let mut data = Box::new(Buffer([0; 1024]));
    let phase = Phase { family: Family::Random, key: 7, mask: 0, beat: BusBeat::Bits32 };
    for (n, line) in data.0.as_chunks_mut::<8>().0.iter_mut().enumerate() {
        line.copy_from_slice(&phase.line(n as u64));
    }
    let before = data.0;
    reset_rescan_counts();
    let n = unsafe { native_rescan(&chunk(data.0.as_mut_ptr()), 0, phase, "review", 0x8000) };
    assert_eq!(n, 1);
    assert_eq!((rescan_counts().alarms, rescan_counts().not_reproduced), (1, 1));
    assert_eq!(data.0, before);
    crate::pattern::set_rescan(false);
    reset_rescan_counts();
    let n = unsafe { native_rescan(&chunk(data.0.as_mut_ptr()), 0, phase, "review", 0x8000) };
    crate::pattern::set_rescan(true);
    assert_eq!(n, 1);
    assert_eq!((rescan_counts().alarms, rescan_counts().not_reproduced), (1, 0));
    println!("PASS: simulated vanished failure still fails; rescan=off records an unlocalised alarm, not a rescan result.");
}

#[test]
fn review_repro_native_interval_is_ignored() {
    let mut data = Box::new(Buffer([0; 1024]));
    let blocks = blocks(&mut data);
    let mut config = TestMemoryConfig::new(ExtentMode::FullAllocation, ChunkMode::Whole);
    config.epoch = StepEpoch { seed: 7, suite_cycle: 1, step: 0 };
    config.program = Some(Program::single(Family::Ident));
    config.test_reps = 0;
    config.skip_init = true;
    config.error_check_interval = ErrorCheckInterval { power_of_two_shift: 3 };
    let phase = config.program.unwrap().phase(&config.epoch, 1, 0, Schedule::OncePerRun, Some(BusBeat::Bits32));
    for (n, line) in data.0.as_chunks_mut::<8>().0.iter_mut().enumerate() {
        line.copy_from_slice(&phase.line(n as u64));
    }
    data.0[8] ^= 1;
    data.0[40] ^= 1;
    let stats = unsafe {
        simple_native_scalar(&blocks, 0, ErrorMode::Log, &config.timing, &config, None,
                             NativeRun::simple(config.program.unwrap(), &config))
    };
    assert_eq!((stats.error_count, stats.alarms), (2, 1));
    println!("REPRO: requested interval 8, two separated bad words -> native scans whole chunk and records one alarm.");
}

#[test]
fn review_native_halt_retains_the_corrupt_image_after_cold_rescan() {
    let mut data = Box::new(Buffer([0; 1024]));
    let blocks = blocks(&mut data);
    let mut config = TestMemoryConfig::new(ExtentMode::FullAllocation, ChunkMode::Whole);
    config.epoch = StepEpoch { seed: 9, suite_cycle: 1, step: 0 };
    config.program = Some(Program::single(Family::Random));
    config.test_reps = 0;
    config.skip_init = true;
    let phase = config.program.unwrap().phase(&config.epoch, 1, 0, Schedule::OncePerRun, Some(BusBeat::Bits32));
    for (n, line) in data.0.as_chunks_mut::<8>().0.iter_mut().enumerate() {
        line.copy_from_slice(&phase.line(n as u64));
    }
    for i in [0, 31, 511, 1023] { data.0[i] ^= 0x8000_0000; }
    let before = data.0;
    let stats = unsafe {
        simple_native_scalar(&blocks, 0, ErrorMode::Halt, &config.timing, &config, None,
                             NativeRun::simple(config.program.unwrap(), &config))
    };
    assert_eq!(stats.error_count, 4);
    assert_eq!(data.0, before);
    println!("PASS: four persistent corruptions counted; local halt does not rewrite the test buffer.");
}

struct ReviewLogger(std::sync::Mutex<Vec<String>>);
static REVIEW_LOGS: ReviewLogger = ReviewLogger(std::sync::Mutex::new(Vec::new()));
impl log::Log for ReviewLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool { true }
    fn log(&self, record: &log::Record<'_>) {
        self.0.lock().unwrap().push(record.args().to_string());
    }
    fn flush(&self) {}
}

#[test]
fn review_persistent_hit_retains_phase_key_round_and_vanished_bits() {
    log::set_logger(&REVIEW_LOGS).expect("isolated review process has no other logger");
    log::set_max_level(log::LevelFilter::Error);
    let mut data = Box::new(Buffer([0; 1024]));
    let phase = Phase { family: Family::Random, key: 7, mask: !0, beat: BusBeat::Bits32 };
    for (n, line) in data.0.as_chunks_mut::<8>().0.iter_mut().enumerate() {
        line.copy_from_slice(&phase.line(n as u64));
    }
    data.0[0] ^= 1;
    reset_rescan_counts();
    REVIEW_LOGS.0.lock().unwrap().clear();
    // Simulate a first pass that also saw a difference in bit 47, now vanished.
    let first_mask = 0x0000_8000_0000_0001;
    let count = unsafe {
        native_rescan(&chunk(data.0.as_mut_ptr()), 0, phase, "Mem-SimpleV2", first_mask)
    };
    assert_eq!(count, 1);
    assert_eq!((rescan_counts().alarms, rescan_counts().not_reproduced), (1, 0));
    let messages = REVIEW_LOGS.0.lock().unwrap().join("\n");
    assert!(messages.contains("XOR 0x0000000000000001"));
    assert!(messages.contains("random-~P"));
    assert!(messages.contains("key 0x0000000000000007, round 9"));
    assert!(messages.contains("0x0000800000000001"));
    assert!(messages.contains("didn't see bits 0x0000800000000000 again"));
    println!("PASS: persistent rescan retains phase, key, round, original mask and vanished bit-47:\n{messages}");
}

fn chunk(ptr: *mut u64) -> ChunkCtx {
    ChunkCtx { ptr, chunk_start: 0, chunk_end: 1024, cycle: 1, thread_id: 0, check_mask: None, round: 9 }
}
