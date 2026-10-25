#![allow(unsafe_op_in_unsafe_fn)]
#![feature(portable_simd)]
#![feature(clflushopt_target_feature)]
#![feature(simd_x86_clflushopt)]
#![feature(x86_amx_intrinsics)]
// AMX: the amx-tile and amx-int8 target features, `_tile_*`, their detection (TODO 102)
// The engine prints nothing and exits nowhere: it hands its frontend events (`frontend.rs`, TODO 29), and the one-shot commands write to
// their caller's writer (`commands.rs`). The CLI, which prints, is its own crate (`crates/tmr-cli`). Test code prints freely.
#![cfg_attr(not(test), deny(clippy::print_stdout, clippy::print_stderr, clippy::exit))]
// Every `unsafe` block says why it is sound (`// SAFETY:`), and shipped code doesn't `unwrap()`: `expect` says why it can't fail
// (2026-10-23). Tests may do either.
#![cfg_attr(not(test), warn(clippy::undocumented_unsafe_blocks, clippy::unwrap_used))]

// The engine. Its public items are what tmr.exe (`crates/tmr-app`), the CLI (`crates/tmr-cli`) and the window (`crates/tmr-gui`) call:
// `session` (a command line's setup and run), `frontend` (the interface a session is shown through, with the types its events carry),
// `commands` (the one-shot commands), `logfile`, `BUILD` and `request_stop`. Everything else is private, so `dead_code` checks it. While
// `main.rs` held the CLI and every module here was `pub`, all of them counted as public API, and an uncalled `pub fn` was never reported
// (TODO 69 E).
pub mod commands; // the one-shot commands, writing to their caller's writer (TODO 29)
pub mod frontend; // the engine's interface to whoever shows a session: its events (TODO 29)
pub mod logfile;
pub mod session; // a command line's setup, then its run, through a frontend (TODO 29) // the log file, which every frontend's logger writes (TODO 29)
pub use build_info::BUILD;
pub use runner::request_stop;
// What the CLI's tests drive the engine with, under the `test-support` feature (TODO 29)
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

mod amx; // the AMX tile layer: detection, the tile configuration (TODO 102)
mod app_config; // Persistent application config (tmr-cfg.json)
mod bandwidth_tests; // Bandwidth measurement tests
mod build_info;
mod cache;
mod calibration; // Adaptive cache calibration
mod config;
mod constants;
mod cpu_selection; // two-stage CPU selection: skip-cores filter + cpu-stride spacing
mod cpu_topology;
mod error_context; // where a worker is in the run, for its error lines (TODO 74)
mod exchange; // the page exchange: tiles that cross threads, CCDs and NUMA nodes, copied, inverted or rewritten (TODO 28)
mod failing_bits; // which bit positions failed checks, from the verifies' cold path (TODO 97)
mod formatting;
mod latency_tests; // Latency measurement tests
mod latency_tests_v2; // v2 latency tests: Layout A/B reads + NT write saturation PoC
mod layout;
mod memory;
mod ops; // the work around a test, by operation: seal fills and checks, fills (TODO 90)
mod params; // Centralized parameter registry
mod pattern; // native patterns (spec v2): families, programs, keys
mod pattern_gen; // v2 pattern generation (LCG, Mode 0/1/2)
mod pattern_switch24; // the native switch's 24-bit lines as SIMD byte shuffles
mod plans; // built-in test plans: which of the suite's tests a run takes (TODO 25)
mod progress;
mod reporting;
mod results; // New results module
mod run_context; // Run identity + resolved config recorded into result files (TODO #67)
mod runner;
mod seal; // the seal: TM5's test 0, checked and resealed around every chunk (TODO 74)
mod sequence; // Mem-Sequence's passes: writes, reads and read-then-writes in an order (TODO 97)
mod simd;
mod smbios; // SMBIOS table parser (system identity, memory modules)
mod storm; // Mem-CoherenceStorm: cache lines every thread writes at once, GSAT's coherency test (TODO 28)
mod test_harness; // v2 zero-cost test orchestration harness
mod test_memory; // where a test's extent lies in a thread's blocks (TODO 76)
mod test_scaffolding; // shared zero-cost bookkeeping for loop-owning v1 tests (TODO #19 Part A)
mod tests;
mod thread_pool;
mod tsc; // TSC frequency detection (x86_64)
mod whea; // WHEA hardware-error monitoring via the System event log (TODO #63) // which build this binary is: version, commit, compiler (TODO 72)
mod xform; // Mem-TileTransform: a memory test under the matrix unit's load (TODO 102)

// Common result type for the crate
pub(crate) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// Crate-root shortcuts, so other modules can write `crate::X`
use cache::SystemInfo;
use config::*;
use layout::{BlockInfo, EnhancedMemoryLayout};
use memory::{check_large_page_privilege, check_restart_needed};
use progress::ProgressTracker;
use runner::AllocationBlock;
use simd::detect_simd_capabilities;
use tests::{set_active_threads_per_core, set_calibration_data};

/// What a run does on an error. Public as a setup event's payload (`frontend::Selection`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ErrorMode {
    Log,   // Log errors and continue (default)
    Halt,  // Stop current test on first error
    Panic, // Panic on first error (for debugging)
}

/// One variant since the driver-client purge (TODO #4/5). A ring-0 backend would come back as a second one.
///
/// Names the API only. Whether a block got large pages is decided per allocation, and the results file records the outcome as `page_mix`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum MemoryBackend {
    VirtualAlloc2,
}

pub(crate) struct RuntimeConfig {
    pub memory_backend: MemoryBackend,
    pub large_pages_available: bool,
    pub cpu_list: Option<Vec<usize>>,
    /// Pin each worker to its `cpu_list` entry. When false the OS schedules the workers, and `cpu_list` is only the ids they are numbered
    /// by.
    pub pin_threads: bool,
    /// The workers' scheduling priority (`priority=`, `system.thread_priority`; TODO 77).
    pub thread_priority: crate::runner::ThreadPriority,
    pub enhanced_memory_strategy: crate::memory::allocation_strategy::EnhancedMemoryStrategy,
    pub memory_allocation: MemoryAllocationConfig,
}
