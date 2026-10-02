//! Pure data structures for reporting - no formatting or presentation logic
//! These structures represent the "what" of reporting, not the "how"

/// System information report data
#[derive(Debug, Clone)]
pub struct SystemInfoReport {
    pub cpu_info: CpuInfo,
    pub cache_info: CacheInfo,
    pub tsc_info: TscCalibrationInfo,
}

/// TSC (Time Stamp Counter) calibration information
#[derive(Debug, Clone)]
pub struct TscCalibrationInfo {
    pub frequency_ghz: f64,
    pub detection_method: String,
    pub is_invariant: bool,
    pub confidence_percent: f64,
    pub samples: u32,
    pub calibration_time_ms: u64,
    pub std_dev_ghz: f64,
    pub converged: bool,
}

/// CPU information
#[derive(Debug, Clone)]
pub struct CpuInfo {
    pub brand: String,
    pub vendor: String,
    pub family: u32,
    pub model: u32,
    pub stepping: u32,
    pub physical_cores: usize,
    pub logical_cores: usize,
    pub has_hyperthreading: bool,
    pub simd_capabilities: Vec<String>,
}

/// Cache information
#[derive(Debug, Clone)]
pub struct CacheInfo {
    pub l1_data_total: u64,
    pub l1_instruction_total: u64,
    pub l2_total: u64,
    pub l3_total: u64,
    pub per_core_l1d: u64,
    pub per_core_l1i: u64,
    pub per_core_l2: u64,
    pub cache_line_size: u64,
    pub detection_method: String,
}

/// Thread timing deviation report
///
/// The WHEA counts sit at report level, not on [`ThreadTiming`], because WHEA events are
/// system-wide: nothing in the event identifies the core that faulted (the XML's
/// `Execution ProcessID` is the logging service). They are carried here so the table's aggregate
/// row can show them in the `WHEA` column and fail even when every thread row is clean — otherwise
/// a WHEA-only failure shows a table of zeros under a failing topline, and the table is not
/// self-contained once it is rendered anywhere other than directly under that topline. Same
/// reasoning applies to the three `PerformanceBy*Report`s below.
#[derive(Debug, Clone)]
pub struct ThreadTimingReport {
    pub average_elapsed_ms: u128,
    pub thread_timings: Vec<ThreadTiming>,
    /// WHEA events during this test (the delta across it, not the run total).
    pub whea: crate::whea::WheaCounts,
}

/// Individual thread timing data
#[derive(Debug, Clone)]
pub struct ThreadTiming {
    pub thread_id: usize,
    pub cpu_id: usize,
    pub physical_core_id: usize,
    pub numa_node: u32,
    pub runtime_ms: u128,
    pub deviation_ms: i128,
    pub data_bytes: u64,
    pub deviation_data_bytes: i64,
    pub throughput_mib_s: f64,
    pub deviation_speed_mib_s: f64,
    pub errors: u64,
    pub cycles_completed: u32,
}

/// Per-thread allocation breakdown
#[derive(Debug, Clone)]
pub struct ThreadAllocationReport {
    pub allocations: Vec<ThreadAllocation>,
}

/// Individual thread allocation info
#[derive(Debug, Clone)]
pub struct ThreadAllocation {
    pub thread_id: usize,
    pub total_size_bytes: u64,
    pub huge_pages_count: u64,
    pub large_pages_count: u64,
    pub regular_pages_count: u64,
}

/// Performance statistics by thread
#[derive(Debug, Clone)]
pub struct PerformanceByThreadReport {
    pub threads: Vec<ThreadPerformance>,
    /// False under `--disable-pinning`: the CPU columns then only say which NUMA node a thread's
    /// memory came from, not where it ran.
    pub pinned: bool,
    /// WHEA events for the whole run — see [`ThreadTimingReport`] for why this is not per-thread.
    pub whea: crate::whea::WheaCounts,
}

/// Individual thread performance
#[derive(Debug, Clone)]
pub struct ThreadPerformance {
    pub thread_id: usize,
    pub cpu_id: usize,
    pub physical_core_id: usize,
    pub numa_node: u32,
    pub total_time_ms: u128,
    pub total_bytes: u64,
    pub throughput_mib_s: f64,
    pub total_errors: u64,
}

/// Performance statistics by CPU
#[derive(Debug, Clone)]
pub struct PerformanceByCpuReport {
    pub cpus: Vec<CpuPerformance>,
    /// WHEA events for the whole run — see [`ThreadTimingReport`] for why this is not per-CPU.
    pub whea: crate::whea::WheaCounts,
}

/// Individual CPU performance
#[derive(Debug, Clone)]
pub struct CpuPerformance {
    pub cpu_id: usize,
    pub physical_core_id: usize,
    pub numa_node: u32,
    pub total_time_ms: u128,
    pub total_bytes: u64,
    pub throughput_mib_s: f64,
    pub total_errors: u64,
}

/// Performance statistics by physical core
#[derive(Debug, Clone)]
pub struct PerformanceByPhysicalCoreReport {
    pub cores: Vec<PhysicalCorePerformance>,
    /// WHEA events for the whole run — see [`ThreadTimingReport`] for why this is not per-core.
    pub whea: crate::whea::WheaCounts,
}

/// Individual physical core performance
#[derive(Debug, Clone)]
pub struct PhysicalCorePerformance {
    pub core_id: usize,
    pub logical_cpus: Vec<usize>,
    pub total_time_ms: u128,
    pub total_bytes: u64,
    pub throughput_mib_s: f64,
    pub total_errors: u64,
}

/// CPU topology display report
#[derive(Debug, Clone)]
pub struct CpuTopologyReport {
    pub cpus: Vec<CpuTopologyEntry>,
    pub summary: TopologySummary,
    /// See [`PerformanceByThreadReport::pinned`].
    pub pinned: bool,
}

/// Individual CPU topology entry
#[derive(Debug, Clone)]
pub struct CpuTopologyEntry {
    pub logical_cpu: usize,
    pub physical_core: usize,
    pub core_type: String,
    pub threads_on_core: usize,
    pub numa_node: u32,
    pub is_hyperthreaded: bool,
    pub status: String,
    pub thread_id: Option<usize>,
}

/// CPU topology summary information
#[derive(Debug, Clone)]
pub struct TopologySummary {
    pub is_hybrid: bool,
    pub total_logical: usize,
    pub total_physical: usize,
    pub performance_cores: usize,
    pub efficiency_cores: usize,
    pub performance_logical: usize,
    pub efficiency_logical: usize,
    pub assigned_count: usize,
    pub available_count: usize,
    pub skipped_count: usize,
    pub smt_excluded_count: usize,
}

/// Test configuration report showing all tests and their settings
#[derive(Debug, Clone)]
pub struct TestConfigurationReport {
    pub suite_timing: String,
    pub test_count: usize,
    pub tests: Vec<TestConfigurationEntry>,
}

#[derive(Debug, Clone)]
pub struct TestConfigurationEntry {
    pub number: usize,
    pub name: String,
    pub timing: String,
    pub parameter: String,
    pub window_mode: String,
    pub chunk_mode: String,
    pub flags: Vec<String>,
}

/// Cycle completion report
#[derive(Debug, Clone)]
pub struct CycleReport {
    pub cycle_number: u32,
    pub duration_secs: u32,
    pub test_performances: Vec<TestPerformanceEntry>,
}

#[derive(Debug, Clone)]
pub struct TestPerformanceEntry {
    pub number: usize,
    pub name: String,
    pub duration_secs: f64,
    pub data_processed_gib: f64,
    pub throughput_mib_s: f64,
    #[expect(dead_code, reason = "TODO #29: the per-cycle report is to be revived, not deleted (from TODO #69 F)")]
    pub throughput_gib_s: f64,
    pub errors: u64,
    /// OS-reported hardware errors during this test, and the corrected subset (see whea.rs).
    pub whea_total: u64,
    pub whea_corrected: u64,
}

/// Final test summary report
#[derive(Debug, Clone)]
pub struct FinalTestSummaryReport {
    pub total_runtime: String,              // Fixed format: "HH:MM:SS"
    pub cycles_completed: usize,
    pub total_data_processed_gib: f64,
    pub overall_throughput_mib_s: f64,
    pub overall_throughput_gib_s: f64,
    pub total_errors: u64,
    /// OS-reported hardware errors for the whole run, and the corrected subset (see whea.rs).
    pub whea_total: u64,
    pub whea_corrected: u64,
    /// False when WHEA monitoring never started, so the zero counts are not mistaken for a pass.
    pub whea_monitored: bool,
    pub per_test_summaries: Vec<TestSummaryEntry>,
}

#[derive(Debug, Clone)]
pub struct TestSummaryEntry {
    pub name: String,
    pub average_duration_secs: f64,
    /// Per-cycle *average*, like duration and throughput. Only errors and WHEA are summed.
    pub average_data_gib: f64,
    pub average_throughput_mib_s: f64,
    pub total_errors: u64,
    // WHEA counts summed over every cycle this test ran in (see whea.rs).
    pub whea_total: u64,
    pub whea_corrected: u64,
    // Latency metrics (Some for latency tests, None for other tests)
    pub latency_samples: Option<u64>,
    pub latency_p5_ns: Option<f64>,
    pub latency_p10_ns: Option<f64>,
    pub latency_p25_ns: Option<f64>,
    pub latency_p50_ns: Option<f64>,
    pub latency_p75_ns: Option<f64>,
    pub latency_p90_ns: Option<f64>,
    pub latency_p95_ns: Option<f64>,
    pub latency_p99_ns: Option<f64>,
    pub latency_p99_9_ns: Option<f64>,
    pub latency_spread: Option<f64>,  // P95/P5 ratio
}

/// Consolidated memory report combining current status and allocation planning
#[derive(Debug, Clone)]
pub struct ConsolidatedMemoryReport {
    // System memory information
    pub total_installed_bytes: u64,
    pub total_physical_bytes: u64,
    pub available_physical_bytes: u64,
    pub used_physical_bytes: u64,
    pub total_virtual_bytes: u64,
    pub memory_load_percent: u32,
    
    // Allocation planning information, as in `AllocationResult`
    pub reference_bytes: u64,
    pub reference_name: &'static str,
    pub requested_reserve_bytes: u64,
    pub target_bytes: Option<u64>,
    pub raw_allocation_bytes: u64,
    pub thread_count: usize,
    pub per_thread_raw_bytes: u64,
    pub rounding_step_bytes: u64,
    pub rounding_direction: String,
    /// The step and direction asked for, when they would have passed the reference figure
    pub rounding_asked: Option<(u64, String)>,
    pub per_thread_bytes: u64,
    pub allocation_bytes: u64,
    pub reserve_bytes: u64,

    // Warnings
    pub warnings: Vec<String>,
}

/// Block allocation distribution report - backend-agnostic; `allocator_backend` names the source
#[derive(Debug, Clone)]
pub struct BlockAllocationReport {
    pub allocator_backend: String,
    pub total_threads: usize,
    pub total_allocated_bytes: u64,
    pub block_size_distribution: Vec<BlockSizeDistribution>,
    pub page_type_summary: PageTypeSummary,
    pub per_thread_allocation: Vec<ThreadBlockAllocation>,
    pub numa_distribution: Vec<NumaNodeDistribution>,
    pub allocation_fairness: AllocationFairness,
}

/// Block size distribution statistics
#[derive(Debug, Clone)]
pub struct BlockSizeDistribution {
    pub block_size_mb: u32,
    pub block_count: u32,
    pub total_bytes: u64,
    pub page_type: String,
    pub threads_with_this_size: u32,
    pub average_per_thread: f64,
}

/// Page type allocation summary
#[derive(Debug, Clone)]
pub struct PageTypeSummary {
    pub huge_pages: PageTypeStats,
    pub large_pages: PageTypeStats,
    pub regular_pages: PageTypeStats,
}

/// Statistics for a specific page type
#[derive(Debug, Clone)]
pub struct PageTypeStats {
    pub page_count: u64,
    pub total_bytes: u64,
    pub block_count: u32,
    pub threads_using: u32,
    pub percentage_of_total: f64,
}

/// Per-thread block allocation details
#[derive(Debug, Clone)]
pub struct ThreadBlockAllocation {
    pub thread_id: usize,
    pub cpu_id: usize,
    pub numa_node: u32,
    /// The thread's share from the layout: what the allocator was asked for
    pub target_bytes: u64,
    pub total_bytes: u64,
    pub block_sizes: Vec<ThreadBlockSize>,
    pub page_type_breakdown: ThreadPageTypeBreakdown,
}

/// Block size allocation for a specific thread
#[derive(Debug, Clone)]
pub struct ThreadBlockSize {
    pub size_mb: u32,
    pub count: u32,
    pub total_bytes: u64,
    pub page_type: String,
}

/// Page type breakdown for a specific thread
#[derive(Debug, Clone)]
pub struct ThreadPageTypeBreakdown {
    pub huge_pages_count: u64,
    pub large_pages_count: u64,
    pub regular_pages_count: u64,
    pub huge_pages_bytes: u64,
    pub large_pages_bytes: u64,
    pub regular_pages_bytes: u64,
}

/// NUMA node distribution statistics
#[derive(Debug, Clone)]
pub struct NumaNodeDistribution {
    pub node_id: u32,
    pub thread_count: u32,
    pub total_bytes: u64,
    pub block_sizes: Vec<u32>, // Block sizes in MB allocated on this node
    pub page_types: NumaPageTypeStats,
}

/// Page type statistics per NUMA node
#[derive(Debug, Clone)]
pub struct NumaPageTypeStats {
    pub huge_pages_count: u32,
    pub large_pages_count: u32,
    pub regular_pages_count: u32,
    pub huge_pages_bytes: u64,
    pub large_pages_bytes: u64,
    pub regular_pages_bytes: u64,
}

/// Allocation fairness analysis
#[derive(Debug, Clone)]
pub struct AllocationFairness {
    pub coefficient_of_variation: f64, // Standard deviation / mean for allocation sizes
    pub min_allocation_bytes: u64,
    pub max_allocation_bytes: u64,
    pub mean_allocation_bytes: f64,
    /// Fewest and most 1 GiB pages any thread got
    pub min_huge_pages: u64,
    pub max_huge_pages: u64,
}

// Note: CacheLatencyReport was removed - --cache-latency now uses LatencyTestSummaryReport

/// Multi-threaded latency test summary report
/// Aggregates results from all threads for each cache level tested
#[derive(Debug, Clone)]
pub struct LatencyTestSummaryReport {
    pub levels_tested: Vec<LatencyLevelSummary>,
}

/// Summary of latency measurements for a single cache level across all threads
#[derive(Debug, Clone)]
pub struct LatencyLevelSummary {
    pub total_samples: usize,          // Total samples across all threads
    pub per_thread_results: Vec<LatencyThreadResult>,
    pub consolidated: LatencyPercentiles,  // Consolidated across all threads
}

/// Per-thread latency results
#[derive(Debug, Clone)]
pub struct LatencyThreadResult {
    pub thread_id: usize,
    pub cpu_id: usize,
    pub sample_count: usize,
    pub percentiles: LatencyPercentiles,
}

/// Full percentile breakdown for latency measurements
#[derive(Debug, Clone)]
pub struct LatencyPercentiles {
    pub min_ns: f64,
    pub p5_ns: f64,
    pub p10_ns: f64,
    pub p25_ns: f64,
    pub p50_ns: f64,
    pub p75_ns: f64,
    pub p90_ns: f64,
    pub p95_ns: f64,
    pub p99_ns: f64,
    pub p99_9_ns: f64,
    pub spread_ratio: f64,  // P95/P5 - indicates variability
}

// Note: CacheLatencyLevel was removed - --cache-latency now uses LatencyLevelSummary

/// Generic table data for rendering
#[derive(Debug, Clone)]
pub struct TableData {
    pub title: Option<String>,
    pub headers: Vec<TableHeader>,
    pub rows: Vec<Vec<String>>,
    pub footer: Option<String>,
}

/// Table header with alignment
#[derive(Debug, Clone)]
pub struct TableHeader {
    pub text: String,
    pub alignment: ColumnAlignment,
}

/// Column alignment options
#[derive(Debug, Clone, Copy)]
pub enum ColumnAlignment {
    Left,
    Center,
    Right,
}

impl TableHeader {
    pub fn new(text: impl Into<String>, alignment: ColumnAlignment) -> Self {
        Self {
            text: text.into(),
            alignment,
        }
    }
}

impl Default for TableData {
    fn default() -> Self {
        Self::new()
    }
}

impl TableData {
    pub fn new() -> Self {
        Self {
            title: None,
            headers: Vec::new(),
            rows: Vec::new(),
            footer: None,
        }
    }
    
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }
    
    pub fn add_header(mut self, text: impl Into<String>, alignment: ColumnAlignment) -> Self {
        self.headers.push(TableHeader::new(text, alignment));
        self
    }
    
    pub fn add_row(mut self, row: Vec<String>) -> Self {
        self.rows.push(row);
        self
    }
    
    pub fn with_footer(mut self, footer: impl Into<String>) -> Self {
        self.footer = Some(footer.into());
        self
    }
}