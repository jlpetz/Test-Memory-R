//! Audit probes appended to runner.rs in an isolated copy by run_repros.py.
//! Assertions document the observed current behaviour, including defects.

use super::*;
use crate::memory::allocator::AllocationConfig;
use crate::memory::backend::{AllocError, Backend, BackendAllocation};
use crate::memory::buffer::{BufferInfo, PageType};
use crate::memory::MemoryBuffer;
use crate::seal::{SealKernel, SealPattern, SealWidth};
use crate::tests::{ChunkMode, ExtentMode, TestMemoryConfig, TestTiming};
use std::sync::Arc;

const SIZE: usize = 32 * 1024;
const GUARD: usize = 4096;
const SENTINEL: u64 = 0x1234_5678_ABCD_EF01;

#[derive(Debug)]
struct HeapView;

impl Backend for HeapView {
    fn allocate(&self, _: &AllocationConfig) -> Result<BackendAllocation, AllocError> {
        unreachable!("audit uses an ordinary heap allocation")
    }
    fn free(&self, _: BackendAllocation) -> Result<(), String> { Ok(()) }
    fn name(&self) -> &'static str { "audit-heap" }
}

struct Region {
    mem: *mut u8,
    layout: std::alloc::Layout,
    blocks: Vec<AllocationBlock>,
}

impl Region {
    fn new() -> Self {
        let layout = std::alloc::Layout::from_size_align(SIZE + 2 * GUARD, 4096).unwrap();
        let mem = unsafe { std::alloc::alloc(layout) };
        assert!(!mem.is_null());
        unsafe { std::ptr::write_bytes(mem, 0xA7, layout.size()) };
        let blocks = vec![AllocationBlock {
            buffer: MemoryBuffer::new(
                BackendAllocation {
                    ptr: unsafe { mem.add(GUARD) },
                    size: SIZE,
                    info: BufferInfo { numa_node: 0, page_type: PageType::Regular(SIZE) },
                },
                Arc::new(HeapView),
            ),
            block_info: crate::BlockInfo { size_bytes: SIZE, thread_id: 0 },
        }];
        Self { mem, layout, blocks }
    }

    fn words(&mut self) -> &mut [u64] {
        unsafe { std::slice::from_raw_parts_mut(self.mem.add(GUARD).cast(), SIZE / 8) }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        let bytes = unsafe { std::slice::from_raw_parts(self.mem, self.layout.size()) };
        assert!(bytes[..GUARD].iter().chain(&bytes[GUARD + SIZE..]).all(|b| *b == 0xA7));
        self.blocks.clear();
        unsafe { std::alloc::dealloc(self.mem, self.layout) };
    }
}

fn config(mode: u32) -> TestMemoryConfig {
    TestMemoryConfig::new(
        ExtentMode::Absolute { size_bytes: SIZE },
        ChunkMode::Absolute { size_bytes: 12 * 1024 },
    )
    .with_timing(TestTiming::cycles_only(1))
    .with_pattern_config(Some(mode), Some(0x5DEECE66D), Some(0xB))
}

fn run(region: &mut Region, name: &str, config: &TestMemoryConfig, thread: usize) -> crate::tests::TestStats {
    let TestFunction::MultiBlock(function) = crate::tests::get_test_function_by_name(name).unwrap() else {
        panic!("expected a correctness kernel")
    };
    unsafe { function(&region.blocks, thread, ErrorMode::Log, &config.timing, config, None) }
}

#[test]
fn audit_invocation_epoch_is_reused() {
    let mut region = Region::new();
    for mode in [0, 1, 2, 12, 14] {
        let cfg = config(mode);
        assert_eq!(run(&mut region, "Mem-SimpleV2", &cfg, 0).error_count, 0);
        let first = region.words().to_vec();
        assert_eq!(run(&mut region, "Mem-SimpleV2", &cfg, 0).error_count, 0);
        assert_eq!(first, region.words());
        let mut two = cfg;
        two.timing = TestTiming::cycles_only(2);
        assert_eq!(run(&mut region, "Mem-SimpleV2", &two, 0).error_count, 0);
        assert_ne!(first, region.words());
        println!("EPOCH mode {mode}: repeated invocation IDENTICAL; local cycle 2 DIFFERENT");
    }
}

#[test]
fn audit_simd_mode_substitutions() {
    let mut region = Region::new();
    for mode in [0, 1, 2, 10, 11, 12, 13, 14] {
        let cfg = config(mode);
        assert_eq!(run(&mut region, "Mem-SimpleV2", &cfg, 0).error_count, 0);
        let scalar = region.words().to_vec();
        for (name, available) in [
            ("Mem-SimpleV2-128", is_x86_feature_detected!("sse2")),
            ("Mem-SimpleV2-256", is_x86_feature_detected!("avx2")),
            ("Mem-SimpleV2-512", is_x86_feature_detected!("avx512f")),
        ] {
            if !available { continue; }
            assert_eq!(run(&mut region, name, &cfg, 0).error_count, 0);
            let equal = scalar == region.words();
            assert_eq!(equal, matches!(mode, 10 | 11 | 12 | 14));
            let differences = scalar.iter().zip(region.words().iter()).filter(|(a, b)| a != b).count();
            println!("WIDTH mode {mode}, {name}: scalar_equal={equal}, different_words={differences}");
        }
    }
}

#[test]
fn audit_relative_patterns_alias_different_workers() {
    let (mut a, mut b) = (Region::new(), Region::new());
    assert_ne!(a.words().as_ptr(), b.words().as_ptr());
    for mode in [10, 11, 13] {
        let cfg = config(mode);
        assert_eq!(run(&mut a, "Mem-SimpleV2", &cfg, 0).error_count, 0);
        assert_eq!(run(&mut b, "Mem-SimpleV2", &cfg, 1).error_count, 0);
        assert_eq!(a.words(), b.words());
        println!("WORKER_ALIAS mode {mode}: all {} words identical at different addresses and thread IDs", SIZE / 8);
    }
}

#[test]
fn audit_bench_mode10_ignores_custom_parameter() {
    let mut region = Region::new();
    let cfg = config(10);
    assert_eq!(run(&mut region, "Mem-SimpleV2", &cfg, 0).error_count, 0);
    let simple = region.words().to_vec();
    assert_eq!(run(&mut region, "Bench-Init-TMR-0", &cfg, 0).error_count, 0);
    let differences = simple.iter().zip(region.words().iter()).filter(|(a, b)| a != b).count();
    assert_eq!(differences, SIZE / 8);
    assert!(region.words().iter().enumerate().all(|(i, value)| *value == i as u64 ^ 0xDEADBEEFDEADBEEF));
    println!("BENCH_PARAM mode 10: custom param0 ignored by Bench; {differences} words differ from Simple");
}

#[test]
fn audit_seal_has_no_temporal_epoch() {
    let mut region = Region::new();
    let kernel = SealKernel { pattern: SealPattern::Tmr, width: SealWidth::W256 };
    let ptr = region.words().as_mut_ptr();
    unsafe { kernel.fill(ptr, 0, SIZE / 8) };
    let old = region.words().to_vec();
    for _ in 0..4 {
        unsafe { kernel.fill(ptr, 0, SIZE / 8) };
        assert_eq!(old, region.words());
    }
    // Replaying a previous image at the same addresses is undetectable.
    region.words().copy_from_slice(&old);
    assert_eq!(unsafe { kernel.check(ptr, 0, SIZE / 8, "audit stale image") }, 0);
    println!("SEAL_EPOCH: four reseals identical; old same-address image accepted");
}

#[test]
fn audit_seal_u64_uniqueness_has_a_counterexample() {
    // Pure numeric addresses: no allocation or pointer dereference at either address.
    let (a, b) = (0x6B39_4CE3_7B10, 0x6B49_4CE3_7B18);
    let first = crate::seal::tmr_word(a);
    let second = crate::seal::tmr_word(b);
    assert_eq!(first, second);
    assert_eq!(first, 0x405A_C1F5_4054_8C70);
    println!("SEAL_WORD_ALIAS: {a:#x} and {b:#x} both contain {first:#x}");
}

#[test]
fn audit_zero_test_reps_initializes_the_wrong_epoch() {
    let mut region = Region::new();
    for mode in [0, 1, 2, 12, 14] {
        let mut cfg = config(mode);
        cfg.test_reps = 0;
        cfg.verify_reps = 1;
        let stats = run(&mut region, "Mem-SimpleV2", &cfg, 0);
        assert!(stats.error_count > 0);
        println!("ZERO_WRITE_REPS mode {mode}: {} false errors in fault-free memory", stats.error_count);
    }
}

#[test]
fn audit_json_plan_accepts_zero_correctness_repetitions() {
    for field in ["test_reps", "verify_reps", "write_read_cycles"] {
        let demo = crate::config::ModernConfig::create_demo_config();
        let mut json = serde_json::to_value(demo).unwrap();
        json["system"]["seal"] = serde_json::json!("off");
        json["test_sequence"] = serde_json::json!([{
            "enabled": true,
            "function": "Mem-SimpleV2",
            "cycles": 1,
            "extent": {"mode": "absolute", "size": "32KiB"},
            "chunk": {"mode": "absolute", "size": "32KiB"},
            "pattern_mode": 0
        }]);
        json["test_sequence"][0][field] = serde_json::json!(0);
        let parsed: crate::config::ModernConfig = serde_json::from_value(json).unwrap();
        let plan = parsed.plan().unwrap();
        assert_eq!(plan.tests.len(), 1);
        let cfg = &plan.tests[0].config;
        let mut region = Region::new();
        let stats = run(&mut region, "Mem-SimpleV2", cfg, 0);
        if field == "test_reps" {
            assert!(stats.error_count > 0);
        } else {
            assert_eq!(stats.error_count, 0);
        }
        println!("JSON_ZERO {field}: accepted, {} errors", stats.error_count);
    }
}

fn definition(name: &'static str, cfg: TestMemoryConfig) -> TestDefinition {
    TestDefinition {
        actual_name: name,
        display_name: name.to_string(),
        function: crate::tests::get_test_function_by_name(name).unwrap(),
        memory_effect: derive_memory_effect(name, &cfg),
        config: cfg,
        original_name: None,
        test_index: 0,
        id: None,
        seal_use: seal_use(name),
        seal_opt_out: false,
    }
}

#[test]
fn audit_dependency_validation_misses_epoch_and_extent() {
    let mut region = Region::new();
    for mode in [0, 1, 2, 12, 14] {
        let init = definition("Mem-SimpleV2", config(mode));
        let mut verify_cfg = config(mode);
        verify_cfg.skip_init = true;
        let verify = definition("Bench-Verify-TM5-0", verify_cfg);
        assert!(validate_test_plan(&[init.clone(), verify.clone()]).is_empty());
        assert_eq!(run(&mut region, init.actual_name, &init.config, 0).error_count, 0);
        let result = run(&mut region, verify.actual_name, &verify.config, 0);
        assert!(result.error_count > 0);
        println!("DEPENDENCY_EPOCH mode {mode}: validator accepts; {} false errors", result.error_count);
    }
    region.words().fill(SENTINEL);
    let mut init_cfg = config(10);
    init_cfg.extent_mode = ExtentMode::Absolute { size_bytes: SIZE / 2 };
    let init = definition("Bench-Init-TMR-0", init_cfg);
    let mut verify_cfg = config(10);
    verify_cfg.skip_init = true;
    let verify = definition("Bench-Verify-TMR-0", verify_cfg);
    assert!(validate_test_plan(&[init.clone(), verify.clone()]).is_empty());
    assert_eq!(run(&mut region, init.actual_name, &init.config, 0).error_count, 0);
    let result = run(&mut region, verify.actual_name, &verify.config, 0);
    assert!(result.error_count > 0);
    println!("DEPENDENCY_EXTENT: 16 KiB writer -> 32 KiB reader accepted; {} false errors", result.error_count);
}

#[test]
fn audit_seal_halt_still_runs_the_chunk_body() {
    use std::cell::Cell;
    let mut region = Region::new();
    let mut cfg = config(10);
    cfg.chunk_mode = ChunkMode::Absolute { size_bytes: SIZE };
    let kernel = SealKernel::detect();
    cfg.seal = crate::seal::SealStep { kernel, on: true, expects: true, wrap: true };
    let ptr = region.words().as_mut_ptr();
    unsafe { kernel.fill(ptr, 0, SIZE / 8) };
    region.words()[0] ^= 1;
    let writes = Cell::new(0);
    let stats = unsafe {
        crate::test_harness::run_phased_test(
            &region.blocks, 0, ErrorMode::Halt, &cfg.timing, &cfg, None,
            "Mem-SimpleV2", crate::tests::TestAction::WriteVerify, 1, false, 1, 1,
            |_| {},
            |ctx| {
                writes.set(writes.get() + 1);
                for i in ctx.chunk_start..ctx.chunk_end { *ctx.ptr.add(i) = i as u64; }
            },
            |_| 0,
        )
    };
    assert_eq!(stats.seal_errors, 1);
    assert_eq!(writes.get(), 1);
    println!("HALT_ORDER: bad seal counted, but write body still executes and chunk is resealed before halt");
}
