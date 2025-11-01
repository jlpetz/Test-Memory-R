/// Pure data structures for reporting - no formatting or presentation logic
/// These structures represent the "what" of reporting, not the "how"
use std::time::Duration;

/// Memory allocation report data
#[derive(Debug, Clone)]
pub struct MemoryAllocationReport {
    // Raw values
    pub total_installed_bytes: u64,
    pub total_physical_bytes: u64,
    pub available_physical_bytes: u64,
    pub used_physical_bytes: u64,
    pub allocation_bytes: u64,
    pub reserve_bytes: u64,
    pub reference_bytes: u64,
    
    // Calculated values
    pub allocation_percent: f64,
    pub reserve_percent: f64,
    pub memory_load_percent: u32,
    
    // Metadata
    pub allocation_type: String,
    pub min_start_address: u64,
    pub actual_start_address: u64,
    
    // Split reserve details (optional)
    pub split_reserve: Option<SplitReserveDetails>,
    
    // Status
    pub warnings: Vec<String>,
}

/// Split reserve breakdown details
#[derive(Debug, Clone)]
pub struct SplitReserveDetails {
    pub pre_percent: f64,
    pub post_percent: f64,
    pub pre_buffer_bytes: u64,
    pub post_reserve_bytes: u64,
}

/// System information report data
#[derive(Debug, Clone)]
pub struct SystemInfoReport {
    pub cpu_info: CpuInfo,
    pub cache_info: CacheInfo,
    pub memory_info: MemoryInfo,
    pub topology_info: TopologyInfo,
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

/// Memory system information
#[derive(Debug, Clone)]
pub struct MemoryInfo {
    pub total_installed: u64,
    pub total_physical: u64,
    pub available_physical: u64,
    pub large_pages_available: bool,
    pub huge_pages_available: bool,
    pub numa_nodes: usize,
}

/// CPU topology information
#[derive(Debug, Clone)]
pub struct TopologyInfo {
    pub is_hybrid: bool,
    pub p_cores: usize,
    pub e_cores: usize,
    pub threads_per_core: usize,
    pub numa_nodes: Vec<NumaNodeInfo>,
    pub assigned_cpus: Vec<usize>,
    pub detection_method: String,
}

/// NUMA node information
#[derive(Debug, Clone)]
pub struct NumaNodeInfo {
    pub node_id: usize,
    pub cpu_count: usize,
    pub memory_bytes: u64,
}

/// Test progress report
#[derive(Debug, Clone)]
pub struct TestProgressReport {
    pub current_cycle: u32,
    pub total_cycles: Option<u32>,
    pub elapsed_time: Duration,
    pub estimated_time_remaining: Option<Duration>,
    pub current_test: String,
    pub current_phase: String,
    pub progress_percent: f64,
    pub errors_found: u32,
    pub threads_active: usize,
    pub memory_tested_bytes: u64,
    pub operations_completed: u64,
    pub operations_per_second: f64,
}

/// Per-thread status for progress reporting
#[derive(Debug, Clone)]
pub struct ThreadStatus {
    pub thread_id: usize,
    pub cpu_id: usize,
    pub current_test: String,
    pub progress_percent: f64,
    pub errors: u32,
}

/// Final test results report
#[derive(Debug, Clone)]
pub struct FinalResultsReport {
    pub success: bool,
    pub total_duration: Duration,
    pub cycles_completed: u32,
    pub total_errors: u32,
    pub error_details: Vec<ErrorDetail>,
    pub statistics: TestStatistics,
    pub coverage_percent: f64,
}

/// Detailed error information
#[derive(Debug, Clone)]
pub struct ErrorDetail {
    pub test_name: String,
    pub thread_id: usize,
    pub address: u64,
    pub expected: u64,
    pub actual: u64,
    pub timestamp: Duration,
    pub cycle: u32,
}

/// Test statistics
#[derive(Debug, Clone)]
pub struct TestStatistics {
    pub total_operations: u64,
    pub bytes_tested: u64,
    pub average_bandwidth_gb_s: f64,
    pub peak_bandwidth_gb_s: f64,
    pub operation_breakdown: Option<OperationBreakdown>,
    pub test_coverage: Vec<TestCoverage>,
}

/// Detailed operation breakdown
#[derive(Debug, Clone)]
pub struct OperationBreakdown {
    pub total_reads: u64,
    pub total_writes: u64,
    pub total_verifies: u64,
    pub total_simd_ops: u64,
    pub total_fence_ops: u64,
    pub total_cache_ops: u64,
    pub simd_type: String,
    pub access_pattern: String,
}

/// Coverage information per test
#[derive(Debug, Clone)]
pub struct TestCoverage {
    pub test_name: String,
    pub iterations: u32,
    pub bytes_tested: u64,
    pub errors_found: u32,
    pub average_time_ms: f64,
}

/// Driver status report
#[derive(Debug, Clone)]
pub struct DriverStatusReport {
    pub available: bool,
    pub version: Option<DriverVersion>,
    pub error_message: Option<String>,
    pub statistics: Option<DriverStatistics>,
}

/// Driver version information
#[derive(Debug, Clone)]
pub struct DriverVersion {
    pub major: u16,
    pub minor: u16,
    pub build: u16,
    pub revision: u16,
}

/// Driver statistics
#[derive(Debug, Clone)]
pub struct DriverStatistics {
    pub allocations_active: u64,
    pub bytes_allocated: u64,
    pub huge_pages_used: u64,
    pub large_pages_used: u64,
    pub standard_pages_used: u64,
    pub numa_nodes_used: Vec<usize>,
}

/// Thread timing deviation report
#[derive(Debug, Clone)]
pub struct ThreadTimingReport {
    pub test_name: String,
    pub average_elapsed_ms: u128,
    pub thread_timings: Vec<ThreadTiming>,
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
    pub throughput_mib_s: f64,
    pub errors: u64,
}

/// Per-thread allocation breakdown
#[derive(Debug, Clone)]
pub struct ThreadAllocationReport {
    pub total_threads: usize,
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

/// Page type allocation report
#[derive(Debug, Clone)]
pub struct PageAllocationReport {
    pub page_types: Vec<PageTypeAllocation>,
    pub user_constraints: PageConstraints,
}

/// Page type allocation details
#[derive(Debug, Clone)]
pub struct PageTypeAllocation {
    pub page_type: String,
    pub page_size: u64,
    pub system_available: u64,
    pub requested: u64,
    pub allocated: u64,
    pub success: bool,
}

/// User page size constraints
#[derive(Debug, Clone)]
pub struct PageConstraints {
    pub min_page_size: Option<String>,
    pub max_page_size: Option<String>,
}

/// Performance statistics by thread
#[derive(Debug, Clone)]
pub struct PerformanceByThreadReport {
    pub threads: Vec<ThreadPerformance>,
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
}

/// System memory analysis report
#[derive(Debug, Clone)]
pub struct SystemMemoryAnalysisReport {
    pub memory_types: Vec<MemoryTypeAnalysis>,
    pub min_start_address: u64,
}

/// Individual memory type analysis
#[derive(Debug, Clone)]
pub struct MemoryTypeAnalysis {
    pub memory_type: String,
    pub total_gib: Option<f64>,
    pub available_gib: Option<f64>,
    pub used_gib: Option<f64>,
    pub usage_percent: Option<f64>,
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
    pub streams: usize,
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
    pub throughput_gib_s: f64,
    pub errors: u64,
}

/// CPU performance variance report
#[derive(Debug, Clone)]
pub struct CpuVarianceReport {
    pub average_throughput_mib_s: f64,
    pub cpu_performances: Vec<CpuPerformanceEntry>,
}

#[derive(Debug, Clone)]
pub struct CpuPerformanceEntry {
    pub cpu_id: usize,
    pub throughput_mib_s: f64,
    pub variance_percent: f64,
    pub test_count: u32,
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
    pub per_test_summaries: Vec<TestSummaryEntry>,
}

#[derive(Debug, Clone)]
pub struct TestSummaryEntry {
    pub name: String,
    pub average_duration_secs: f64,
    pub total_data_gib: f64,
    pub average_throughput_mib_s: f64,
    pub average_throughput_gib_s: f64,
    pub total_errors: u64,
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
    pub available_virtual_bytes: u64,
    pub memory_load_percent: u32,
    
    // Allocation planning information
    pub allocation_bytes: u64,
    pub reserve_bytes: u64,
    pub reference_bytes: u64,
    pub allocation_type: String,
    pub min_start_address: u64,
    
    // Split reserve details (if applicable)
    pub split_reserve: Option<SplitReserveDetails>,
    
    // Warnings
    pub warnings: Vec<String>,
}

/// Current memory status report (kept for backward compatibility)
#[derive(Debug, Clone)]
pub struct CurrentMemoryStatus {
    pub physical_total_gib: f64,
    pub physical_available_gib: f64,
    pub physical_used_gib: f64,
    pub physical_free_percent: f64,
    pub page_file_total_gib: f64,
    pub page_file_available_gib: f64,
    pub page_file_used_gib: f64,
    pub memory_load_percent: f64,
}

/// Block allocation distribution report - compatible with both Windows and driver APIs
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
    pub median_allocation_bytes: u64,
    pub unfair_threads: Vec<UnfairThreadAllocation>,
}

/// Threads with unfair allocation (too much or too little)
#[derive(Debug, Clone)]
pub struct UnfairThreadAllocation {
    pub thread_id: usize,
    pub allocated_bytes: u64,
    pub deviation_from_mean_percent: f64,
    pub reason: String, // e.g., "got all huge pages", "only regular pages"
}

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