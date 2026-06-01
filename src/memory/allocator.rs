use crate::memory::backend::{Backend, BackendType, WindowsBackend, DriverBackend};
use crate::memory::buffer::{MemoryBuffer, PageType};
use crate::memory::buffer::MemoryType as BufferMemoryType;
use crate::{BlockInfo, AllocationBlock};
use crate::cpu_topology::get_numa_node_for_cpu;
use crate::constants::{HUGE_PAGE_SIZE_USIZE, BYTES_PER_MIB_USIZE, bytes_to_gib_f64};
use std::sync::Arc;

pub struct MemoryAllocator {
    backend: Arc<dyn Backend>,
    stats: AllocationStats,
}

#[derive(Debug, Clone)]
pub struct AllocationConfig {
    pub size: usize,
    pub numa_node: Option<u32>,
    pub page_size: PageSizePreference,
    pub memory_type: BufferMemoryType,
    pub zero_memory: bool,
    pub timeout_ms: u32,
    pub base_address: Option<*mut u8>,  // Minimum start address for allocation (fragmentation prevention)
    pub alignment: Option<usize>,       // Custom alignment requirement (must be power of 2)
}

#[derive(Debug, Clone)]
pub enum PageSizePreference {
    Any,                                    // Let backend decide
    Prefer(PageType),                       // Prefer but fall back
    Require(PageType),                      // Must have or fail
    Range { min: PageType, max: PageType }, // Range of acceptable sizes
}

#[derive(Debug, Default)]
pub struct AllocationStats {
    pub total_allocations: usize,
    pub total_bytes_allocated: usize,
    pub failed_allocations: usize,
    pub large_page_allocations: usize,
    pub huge_page_allocations: usize,
}

/// Allocation strategy for plan-based allocator
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AllocationStrategy {
    /// Legacy greedy allocation - largest chunks first without fairness planning
    Greedy,
    /// Plan-based: Exhaust all huge pages first, then large pages, then regular pages
    PlanPageSizePref,
    /// Plan-based: For each block size, try huge then large, regular pages as last resort
    PlanBlockSizePref,
}

impl std::str::FromStr for AllocationStrategy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "greedy" => Ok(AllocationStrategy::Greedy),
            "plan-pagesize-pref" | "planpagesizepref" => Ok(AllocationStrategy::PlanPageSizePref),
            "plan-blocksize-pref" | "planblocksizepref" => Ok(AllocationStrategy::PlanBlockSizePref),
            _ => Err(format!("Invalid allocation strategy: '{}'. Valid options: greedy, plan-pagesize-pref, plan-blocksize-pref", s)),
        }
    }
}

impl std::fmt::Display for AllocationStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AllocationStrategy::Greedy => write!(f, "greedy"),
            AllocationStrategy::PlanPageSizePref => write!(f, "plan-pagesize-pref"),
            AllocationStrategy::PlanBlockSizePref => write!(f, "plan-blocksize-pref"),
        }
    }
}

/// Page size level for constraint checking (ordered: Regular < Large < Huge)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PageSizeLevel {
    Regular = 0,
    Large = 1,
    Huge = 2,
}

impl PageSizeLevel {
    /// Parse page size level from string (matches config format)
    pub fn from_config_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "huge" | "1gb" => PageSizeLevel::Huge,
            "large" | "2mb" => PageSizeLevel::Large,
            _ => PageSizeLevel::Regular, // "regular", "4kb", or default
        }
    }
}

/// Check if a page size level is allowed given min/max constraints
pub fn is_page_size_allowed(
    level: PageSizeLevel,
    min_page_size: &str,
    max_page_size: &str,
) -> bool {
    let min = PageSizeLevel::from_config_str(min_page_size);
    let max = PageSizeLevel::from_config_str(max_page_size);
    level >= min && level <= max
}

impl MemoryAllocator {
    pub fn new(backend_type: BackendType) -> Result<Self, String> {
        let backend: Arc<dyn Backend> = match backend_type {
            BackendType::Auto => Self::auto_detect_backend()?,
            BackendType::Windows { large_pages } => {
                Arc::new(WindowsBackend::new(large_pages))
            }
            BackendType::Driver => {
                Arc::new(DriverBackend::new()?)
            }
        };
        
        Ok(Self {
            backend,
            stats: AllocationStats::default(),
        })
    }
    
    fn auto_detect_backend() -> Result<Arc<dyn Backend>, String> {
        // Try driver first, fall back to Windows large pages, then regular
        if let Ok(driver_backend) = DriverBackend::new() {
            log::info!("Auto-detected TMR kernel driver backend");
            Ok(Arc::new(driver_backend))
        } else if crate::memory::check_large_page_privilege().is_ok() {
            log::info!("Auto-detected Windows large pages backend");
            Ok(Arc::new(WindowsBackend::new(true)))
        } else {
            log::info!("Auto-detected Windows regular backend");
            Ok(Arc::new(WindowsBackend::new(false)))
        }
    }
    
    pub fn allocate(&mut self, config: &AllocationConfig) -> Result<MemoryBuffer, String> {
        let allocation = self.backend.allocate(config)?;
        
        // Update statistics
        self.stats.total_allocations += 1;
        self.stats.total_bytes_allocated += allocation.size;
        
        if allocation.info.uses_large_pages() {
            self.stats.large_page_allocations += 1;
        }
        if allocation.info.uses_huge_pages() {
            self.stats.huge_page_allocations += 1;
        }
        
        Ok(MemoryBuffer::new(allocation, self.backend.clone()))
    }
    
    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }
    
    pub fn stats(&self) -> &AllocationStats {
        &self.stats
    }
    
    pub fn reset_stats(&mut self) {
        self.stats = AllocationStats::default();
    }
    
    /// Batch allocation - optimized for driver backend, individual calls for Windows backend
    pub fn batch_allocate(&mut self, configs: Vec<(usize, AllocationConfig)>) -> Result<Vec<(usize, MemoryBuffer)>, String> {
        if configs.is_empty() {
            return Ok(Vec::new());
        }
        
        // For driver backend, use optimized batch API
        if matches!(self.backend_name(), "TMR Kernel Driver") {
            self.batch_allocate_driver(configs)
        } else {
            // For Windows backend, allocate individually
            self.batch_allocate_individual(configs)
        }
    }
    
    /// Individual allocation fallback for Windows backend
    fn batch_allocate_individual(&mut self, configs: Vec<(usize, AllocationConfig)>) -> Result<Vec<(usize, MemoryBuffer)>, String> {
        let mut results = Vec::new();
        
        for (thread_id, config) in configs {
            match self.allocate(&config) {
                Ok(buffer) => results.push((thread_id, buffer)),
                Err(e) => return Err(format!("Failed to allocate for thread {}: {}", thread_id, e)),
            }
        }
        
        Ok(results)
    }
    
    /// Optimized batch allocation for driver backend
    fn batch_allocate_driver(&mut self, configs: Vec<(usize, AllocationConfig)>) -> Result<Vec<(usize, MemoryBuffer)>, String> {
        // Convert AllocationConfig to ThreadAllocationRequest
        let mut thread_requests = Vec::new();
        let mut total_memory_target = 0;
        
        for (thread_id, config) in &configs {
            let request = crate::driver::types::ThreadAllocationRequest {
                thread_id: *thread_id as u32,
                cpu_id: 0, // Will be set by caller if needed
                size_bytes: config.size,
                block_count: 1,
                minimum_page_size: match config.page_size {
                    PageSizePreference::Require(PageType::Huge(_)) => crate::driver::PageSize::Huge,
                    PageSizePreference::Require(PageType::Large(_)) => crate::driver::PageSize::Large,
                    _ => crate::driver::PageSize::Regular,
                },
                maximum_page_size: crate::driver::PageSize::Huge,
                memory_type: match config.memory_type {
                    BufferMemoryType::WriteBack => crate::driver::MemoryType::WriteBack,
                    BufferMemoryType::WriteCombining => crate::driver::MemoryType::WriteCombining,
                    BufferMemoryType::Uncached => crate::driver::MemoryType::Uncached,
                    BufferMemoryType::WriteProtected => crate::driver::MemoryType::WriteProtected,
                    BufferMemoryType::WriteCombined => crate::driver::MemoryType::WriteCombining,
                },
                numa_node: config.numa_node.unwrap_or(0),
                strict_numa: matches!(config.page_size, PageSizePreference::Require(_)),
                zero_memory: config.zero_memory,
                contiguous: false,
                timeout_ms: config.timeout_ms,
                retry_interval_ms: 100,
                max_retries: 5,
            };
            
            thread_requests.push(request);
            total_memory_target += config.size;
        }
        
        // Call driver batch allocation
        let interface = crate::driver::get_global_driver_handle()?;
        let batch_result = interface.enhanced_batch_allocate(total_memory_target, &thread_requests)?;
        
        log::info!("Batch allocation completed: {}/{} successful", 
                  batch_result.successful_allocations, batch_result.total_allocations);
        
        if batch_result.successful_allocations == 0 {
            return Err("Batch allocation failed - no successful allocations".to_string());
        }
        
        // Convert allocation results to MemoryBuffer
        let mut results = Vec::new();
        
        for i in 0..batch_result.total_allocations as usize {
            let result = &batch_result.results[i];
            if !result.success {
                continue;
            }
            
            // Create segments from allocation result
            let segments = vec![crate::memory::buffer::SegmentInfo {
                virtual_address: result.virtual_address,
                physical_address: result.physical_address,
                size: result.size,
                page_size_kb: match result.guaranteed_page_type {
                    2 => 1048576, // Huge (1GB)
                    1 => 2048,    // Large (2MB) 
                    _ => 4,       // Regular (4KB)
                },
                numa_node: result.numa_node,
            }];
            
            let page_type = if result.guaranteed_page_type == 2 {
                PageType::Huge(result.size)
            } else if result.guaranteed_page_type == 1 {
                PageType::Large(result.size)
            } else {
                PageType::Regular(result.size)
            };
            
            let buffer_info = crate::memory::buffer::BufferInfo {
                physical_address: Some(result.physical_address),
                numa_node: result.numa_node,
                page_type,
                segments,
            };
            
            let backend_allocation = crate::memory::backend::BackendAllocation {
                ptr: result.virtual_address as *mut u8,
                size: result.size,
                info: buffer_info,
            };
            
            let memory_buffer = MemoryBuffer::new(backend_allocation, self.backend.clone());
            results.push((result.thread_id as usize, memory_buffer));
            
            // Update stats
            self.stats.total_allocations += 1;
            self.stats.total_bytes_allocated += result.size;
            
            if result.guaranteed_page_type >= 1 {
                self.stats.large_page_allocations += 1;
            }
            if result.guaranteed_page_type >= 2 {
                self.stats.huge_page_allocations += 1;
            }
        }
        
        Ok(results)
    }
    
    /// Two-stage chunk-based allocation for Windows VirtualAlloc2
    /// Stage 1: Discover largest available chunks per NUMA node using power-of-2 sizes
    /// Stage 2: Distribute chunks fairly to threads prioritizing contiguous allocation
    pub fn chunk_allocate(
        &mut self,
        thread_blocks: &std::collections::HashMap<usize, Vec<BlockInfo>>,
        runtime_config: &crate::RuntimeConfig,
    ) -> Result<std::collections::HashMap<usize, Vec<AllocationBlock>>, String> {
        use std::collections::HashMap;
        
        // Calculate total memory needed per NUMA node
        let mut total_size_per_numa: HashMap<u32, usize> = HashMap::new();
        let mut thread_numa_assignments: HashMap<usize, (usize, u32)> = HashMap::new();
        
        for (thread_id, blocks) in thread_blocks {
            // Get the correct CPU ID from the CPU list, fallback to thread_id if not available  
            let cpu_id = runtime_config.cpu_list.as_ref()
                .and_then(|list| list.get(*thread_id))
                .copied()
                .unwrap_or(*thread_id);
            let numa_node = get_numa_node_for_cpu(cpu_id);
            let thread_total: usize = blocks.iter().map(|b| b.size_bytes).sum();
            
            // Debug: Log memory allocation mapping
            log::debug!("Memory: Thread {} → CPU {} → NUMA {} ({}MB)", 
                       thread_id, cpu_id, numa_node, thread_total / BYTES_PER_MIB_USIZE);
            
            *total_size_per_numa.entry(numa_node).or_insert(0) += thread_total;
            thread_numa_assignments.insert(*thread_id, (thread_total, numa_node));
        }
        
        log::info!("Chunk allocation: {} threads across {} NUMA nodes", 
                  thread_blocks.len(), total_size_per_numa.len());
        
        // Stage 1: Discover chunks per NUMA node (with smart greedy fairness)
        let thread_count = thread_blocks.len();
        let allocated_chunks = self.discover_chunks_per_numa(&total_size_per_numa, runtime_config, thread_count)?;
        
        log::info!("Stage 1 complete: {} chunks allocated across {} NUMA nodes", 
                  allocated_chunks.len(), total_size_per_numa.len());
        
        // Stage 2: Distribute chunks to threads
        let distributed_blocks = self.distribute_chunks_to_threads(allocated_chunks, &thread_numa_assignments, thread_blocks)?;
        
        log::info!("Stage 2 complete: Memory distributed to {} threads", distributed_blocks.len());
        
        Ok(distributed_blocks)
    }
    
    /// NEW: Plan-based chunk allocation with fairness guarantee
    /// Creates a pre-allocation plan that ensures fair distribution among threads
    /// while maximizing large block sizes
    pub fn chunk_allocate_planned(
        &mut self,
        thread_blocks: &std::collections::HashMap<usize, Vec<BlockInfo>>,
        runtime_config: &crate::RuntimeConfig,
        strategy: AllocationStrategy,
    ) -> Result<std::collections::HashMap<usize, Vec<AllocationBlock>>, String> {
        use std::collections::HashMap;
        
        // Calculate total memory needed per NUMA node and thread assignments
        let mut total_size_per_numa: HashMap<u32, usize> = HashMap::new();
        let mut thread_numa_assignments: HashMap<usize, (usize, u32)> = HashMap::new();
        let mut threads_per_numa: HashMap<u32, Vec<usize>> = HashMap::new();
        
        for (thread_id, blocks) in thread_blocks {
            let cpu_id = runtime_config.cpu_list.as_ref()
                .and_then(|list| list.get(*thread_id))
                .copied()
                .unwrap_or(*thread_id);
            let numa_node = get_numa_node_for_cpu(cpu_id);
            let thread_total: usize = blocks.iter().map(|b| b.size_bytes).sum();
            
            log::debug!("Plan-based allocation: Thread {} → CPU {} → NUMA {} ({}MB)", 
                       thread_id, cpu_id, numa_node, thread_total / BYTES_PER_MIB_USIZE);
            
            *total_size_per_numa.entry(numa_node).or_insert(0) += thread_total;
            thread_numa_assignments.insert(*thread_id, (thread_total, numa_node));
            threads_per_numa.entry(numa_node).or_default().push(*thread_id);
        }
        
        log::info!("Plan-based chunk allocation: {} threads across {} NUMA nodes (strategy: {:?})", 
                  thread_blocks.len(), total_size_per_numa.len(), strategy);
        
        let mut all_allocated_blocks = HashMap::new();
        
        // Process each NUMA node separately
        for (&numa_node, &total_needed) in &total_size_per_numa {
            let numa_threads = &threads_per_numa[&numa_node];
            let thread_count = numa_threads.len();
            let per_thread_target = total_needed / thread_count;
            
            log::info!("NUMA {}: Creating allocation plan for {} threads × {:.2}GB = {:.2}GB total",
                      numa_node, thread_count,
                      bytes_to_gib_f64(per_thread_target as u64),
                      bytes_to_gib_f64(total_needed as u64));
            
            // Create allocation plan
            let plan = Self::create_allocation_plan(per_thread_target, thread_count);
            
            // Log the plan
            log::info!("NUMA {}: Allocation plan:", numa_node);
            for (block_size, blocks_per_thread) in &plan {
                log::info!("  - {} × {}MB blocks per thread ({}MB total per thread)",
                         blocks_per_thread, block_size / (1024 * 1024),
                         (block_size * blocks_per_thread) / (1024 * 1024));
            }
            
            // Execute the plan based on strategy
            let allocated_chunks = match strategy {
                AllocationStrategy::Greedy => {
                    // Use the legacy greedy allocator - bypass planning
                    log::info!("NUMA {}: Using legacy greedy allocation (no fairness planning)", numa_node);
                    return self.chunk_allocate(thread_blocks, runtime_config);
                }
                AllocationStrategy::PlanPageSizePref => {
                    self.execute_plan_page_type_first(&plan, numa_node, runtime_config)?
                }
                AllocationStrategy::PlanBlockSizePref => {
                    self.execute_plan_block_size_first(&plan, numa_node, runtime_config)?
                }
            };
            
            let total_chunk_bytes: usize = allocated_chunks.iter().map(|c| c.chunk_size).sum();
            log::info!("NUMA {}: Plan execution complete - {} chunks allocated ({:.2}GB of {:.2}GB target)",
                      numa_node, allocated_chunks.len(),
                      bytes_to_gib_f64(total_chunk_bytes as u64),
                      bytes_to_gib_f64(total_needed as u64));
            if total_chunk_bytes > total_needed {
                log::warn!("NUMA {}: ⚠️ Over-allocation before distribution: {:.2}GB allocated for {:.2}GB target ({:.2}GB excess will be orphaned)",
                         numa_node,
                         bytes_to_gib_f64(total_chunk_bytes as u64),
                         bytes_to_gib_f64(total_needed as u64),
                         bytes_to_gib_f64((total_chunk_bytes - total_needed) as u64));
            }

            // Distribute chunks to threads (ensuring fairness based on plan)
            let thread_allocations = self.distribute_planned_chunks(
                allocated_chunks, &plan, numa_threads, &thread_numa_assignments, thread_blocks
            )?;
            
            // Merge into final results
            for (thread_id, blocks) in thread_allocations {
                all_allocated_blocks.entry(thread_id).or_insert_with(Vec::new).extend(blocks);
            }
        }
        
        log::info!("Plan-based allocation complete: Memory distributed to {} threads", 
                  all_allocated_blocks.len());
        
        // Validate that we actually allocated memory
        if all_allocated_blocks.is_empty() {
            return Err("Failed to allocate any memory blocks. This may be due to:\n\
                       1. Insufficient available memory\n\
                       2. Large page privilege not granted (restart required after granting privilege)\n\
                       3. System memory fragmentation preventing large allocations\n\
                       Try: Restart your session/system after granting large page privilege, or use allocator=plan-blocksize-pref".to_string());
        }
        
        // Validate that all threads got memory
        let threads_without_memory: Vec<_> = thread_blocks.keys()
            .filter(|tid| !all_allocated_blocks.contains_key(tid))
            .collect();
        
        if !threads_without_memory.is_empty() {
            return Err(format!("Failed to allocate memory for {} thread(s): {:?}\n\
                               Some threads received memory but others did not. This indicates partial allocation failure.",
                               threads_without_memory.len(), threads_without_memory));
        }
        
        Ok(all_allocated_blocks)
    }
    
    /// Create a fair allocation plan that maximizes block sizes
    fn create_allocation_plan(per_thread_target: usize, thread_count: usize) -> Vec<(usize, usize)> {
        // Plan format: [(block_size_bytes, total_blocks_needed), ...]
        let mut plan = Vec::new();
        let mut remaining_per_thread = per_thread_target;
        
        log::info!("Creating allocation plan: {} bytes per thread × {} threads = {} total",
                 per_thread_target, thread_count, per_thread_target * thread_count);
        
        // Block sizes including new 4GB size: 4GB, 2GB, 1GB, 512MB, 256MB, 128MB, 64MB, 32MB, 16MB
        let block_sizes_mb = [4096, 2048, 1024, 512, 256, 128, 64, 32, 16];
        
        for &block_size_mb in &block_sizes_mb {
            let block_size = (block_size_mb as usize) * 1024 * 1024;
            
            if remaining_per_thread >= block_size {
                let blocks_per_thread = remaining_per_thread / block_size;
                let total_blocks = blocks_per_thread * thread_count;
                
                plan.push((block_size, total_blocks));
                remaining_per_thread %= block_size;
                
                log::info!("Plan: {} × {}MB blocks ({} per thread)",
                         total_blocks, block_size_mb, blocks_per_thread);
            }
        }
        
        if remaining_per_thread > 0 {
            log::warn!("Allocation plan has {} bytes remainder per thread (will attempt to allocate)",
                     remaining_per_thread);
        }
        
        plan
    }
    
    /// Execute plan with PageTypeFirst strategy
    fn execute_plan_page_type_first(
        &mut self,
        plan: &[(usize, usize)],
        numa_node: u32,
        runtime_config: &crate::RuntimeConfig,
    ) -> Result<Vec<AllocatedChunk>, String> {
        let mut allocated_chunks = Vec::new();

        // Get page size constraints from config
        let min_page = &runtime_config.memory_allocation.min_page_size;
        let max_page = &runtime_config.memory_allocation.max_page_size;
        let huge_allowed = is_page_size_allowed(PageSizeLevel::Huge, min_page, max_page);
        let large_allowed = is_page_size_allowed(PageSizeLevel::Large, min_page, max_page);

        log::info!("NUMA {}: Page size constraints: min={}, max={} (huge={}, large={})",
                 numa_node, min_page, max_page, huge_allowed, large_allowed);

        // Phase 1: PageTypeFirst - Try planned chunks + additional sizes with huge pages
        if runtime_config.large_pages_available && huge_allowed {
            log::info!("NUMA {}: Phase 1 - PageTypeFirst: Try planned chunks + extra sizes with huge pages", numa_node);
            
            // First, try all planned chunks with huge pages
            for &(block_size, total_blocks_planned) in plan {
                if block_size >= HUGE_PAGE_SIZE_USIZE {
                    log::info!("NUMA {}: Trying {} × {}MB huge page blocks (planned)",
                             numa_node, total_blocks_planned, block_size / (1024 * 1024));
                    
                    let mut allocated_this_size = 0;
                    for _ in 0..total_blocks_planned {
                        let config = AllocationConfig {
                            size: block_size,
                            numa_node: Some(numa_node),
                            page_size: PageSizePreference::Require(PageType::Huge(block_size)),
                            memory_type: BufferMemoryType::WriteBack,
                            zero_memory: true,
                            timeout_ms: 5000,
                            base_address: None,
                            alignment: Some(HUGE_PAGE_SIZE_USIZE),
                        };
                        
                        match self.allocate(&config) {
                            Ok(buffer) => {
                                log::info!("✅ NUMA {}: {}MB chunk allocated (1GB huge)",
                                         numa_node, block_size / (1024 * 1024));
                                allocated_chunks.push(AllocatedChunk {
                                    buffer,
                                    chunk_size: block_size,
                                    numa_node,
                                });
                                allocated_this_size += 1;
                            }
                            Err(e) => {
                                log::info!("❌ NUMA {}: {}MB huge page allocation failed: {}",
                                         numa_node, block_size / (1024 * 1024), e);
                                break; // Stop trying this planned size with huge pages
                            }
                        }
                    }
                    
                    if allocated_this_size > 0 {
                        log::info!("NUMA {}: Successfully allocated {} of {} planned {}MB huge page blocks",
                                 numa_node, allocated_this_size, total_blocks_planned, block_size / (1024 * 1024));
                    }
                }
            }
            
            // Then, try additional huge-page sizes not in plan (PageTypeFirst benefit)
            let allocated_so_far: usize = allocated_chunks.iter().map(|c| c.chunk_size).sum();
            let total_needed = plan.iter().map(|(size, count)| size * count).sum::<usize>();
            
            if allocated_so_far < total_needed {
                let remaining_needed = total_needed - allocated_so_far;
                log::info!("NUMA {}: Trying additional huge page sizes for remaining {} bytes",
                         numa_node, remaining_needed);
                
                // Try other huge-page-compatible sizes not already attempted in the plan
                let all_additional_sizes_mb = [1024]; // 1GB chunks (good for huge pages)
                let plan_sizes_mb: std::collections::HashSet<usize> = plan.iter()
                    .map(|(size, _)| size / (1024 * 1024))
                    .collect();
                
                let additional_huge_sizes_mb: Vec<usize> = all_additional_sizes_mb
                    .into_iter()
                    .filter(|&size_mb| !plan_sizes_mb.contains(&size_mb))
                    .collect();
                let mut remaining = remaining_needed;
                
                if additional_huge_sizes_mb.is_empty() {
                    log::info!("NUMA {}: No additional huge page sizes needed (all sizes already in plan)", numa_node);
                } else {
                    for &size_mb in &additional_huge_sizes_mb {
                    let chunk_size = size_mb * 1024 * 1024;
                    
                    if remaining >= chunk_size {
                        let max_chunks = remaining / chunk_size;
                        log::info!("NUMA {}: Trying up to {} × {}MB with huge pages (additional)",
                                 numa_node, max_chunks, size_mb);
                        
                        let mut allocated_this_size = 0;
                        for _ in 0..max_chunks {
                            let config = AllocationConfig {
                                size: chunk_size,
                                numa_node: Some(numa_node),
                                page_size: PageSizePreference::Require(PageType::Huge(chunk_size)),
                                memory_type: BufferMemoryType::WriteBack,
                                zero_memory: true,
                                timeout_ms: 5000,
                                base_address: None,
                                alignment: Some(HUGE_PAGE_SIZE_USIZE),
                            };
                            
                            match self.allocate(&config) {
                                Ok(buffer) => {
                                    log::info!("✅ NUMA {}: {}MB chunk allocated (1GB huge)",
                                             numa_node, size_mb);
                                    allocated_chunks.push(AllocatedChunk {
                                        buffer,
                                        chunk_size,
                                        numa_node,
                                    });
                                    remaining -= chunk_size;
                                    allocated_this_size += 1;
                                }
                                Err(e) => {
                                    log::info!("❌ NUMA {}: {}MB huge page allocation failed: {}",
                                             numa_node, size_mb, e);
                                    break; // Stop trying this size, try next size
                                }
                            }
                        }
                        
                        if allocated_this_size > 0 {
                            log::info!("NUMA {}: Successfully allocated {} × {}MB additional huge pages",
                                     numa_node, allocated_this_size, size_mb);
                        }
                    }
                }
                }
            }
        }
        
        // Phase 2: PageTypeFirst - Complete planned chunks first, then try additional sizes
        // Use byte deficit (not per-size slot counting) to avoid over-allocation when
        // Phase 1b grabbed chunks of sizes not in the plan.
        let allocated_so_far: usize = allocated_chunks.iter().map(|c| c.chunk_size).sum();
        let total_needed = plan.iter().map(|(size, count)| size * count).sum::<usize>();
        let mut byte_deficit = total_needed.saturating_sub(allocated_so_far);

        if byte_deficit > 0 && runtime_config.large_pages_available && large_allowed {
            log::info!("NUMA {}: Phase 2 - Complete planned chunks first, then fill remaining (deficit: {} bytes)",
                     numa_node, byte_deficit);

            // Phase 2a: Complete any planned chunks that weren't fully allocated in Phase 1
            log::info!("NUMA {}: Phase 2a - Completing planned chunks with large pages", numa_node);

            for &(block_size, total_blocks_planned) in plan {
                if byte_deficit == 0 { break; }
                if block_size >= 16 * 1024 * 1024 { // Only sizes suitable for large pages
                    // Count how many of this planned size we already have
                    let already_allocated = allocated_chunks.iter()
                        .filter(|c| c.chunk_size == block_size)
                        .count();

                    let plan_still_needed = total_blocks_planned.saturating_sub(already_allocated);
                    // Cap by byte deficit to prevent over-allocation
                    let max_by_deficit = byte_deficit / block_size;
                    let still_needed = plan_still_needed.min(max_by_deficit);

                    if still_needed > 0 {
                        log::info!("NUMA {}: Completing planned {} × {}MB blocks (have {}, need {}, capped to {} by deficit)",
                                 numa_node, total_blocks_planned, block_size / (1024 * 1024),
                                 already_allocated, plan_still_needed, still_needed);

                        let mut allocated_this_size = 0;
                        for _ in 0..still_needed {
                            let config = AllocationConfig {
                                size: block_size,
                                numa_node: Some(numa_node),
                                page_size: PageSizePreference::Require(PageType::Large(block_size)),
                                memory_type: BufferMemoryType::WriteBack,
                                zero_memory: true,
                                timeout_ms: 5000,
                                base_address: None,
                                alignment: Some(2 * 1024 * 1024), // 2MB alignment for large pages
                            };

                            match self.allocate(&config) {
                                Ok(buffer) => {
                                    log::info!("✅ NUMA {}: {}MB chunk allocated (2MB large)",
                                             numa_node, block_size / (1024 * 1024));
                                    allocated_chunks.push(AllocatedChunk {
                                        buffer,
                                        chunk_size: block_size,
                                        numa_node,
                                    });
                                    byte_deficit = byte_deficit.saturating_sub(block_size);
                                    allocated_this_size += 1;
                                }
                                Err(e) => {
                                    log::info!("❌ NUMA {}: {}MB large page allocation failed: {}",
                                             numa_node, block_size / (1024 * 1024), e);
                                    break; // Stop trying this planned size with large pages
                                }
                            }
                        }

                        if allocated_this_size > 0 {
                            log::info!("NUMA {}: Successfully completed {} of {} remaining {}MB blocks",
                                     numa_node, allocated_this_size, still_needed, block_size / (1024 * 1024));
                        }
                    }
                }
            }

            // Phase 2b: Fill any remaining space with other chunk sizes (PageTypeFirst benefit)
            if byte_deficit > 0 {
                log::info!("NUMA {}: Phase 2b - Fill remaining {} bytes with any large page chunks",
                         numa_node, byte_deficit);
                
                let all_chunk_sizes_mb = [4096, 2048, 1024, 512, 256, 128, 64, 32, 16];

                for &chunk_mb in &all_chunk_sizes_mb {
                    if byte_deficit == 0 { break; }
                    let chunk_size = (chunk_mb as usize) * 1024 * 1024;

                    // Only try sizes that are 16MB+ (suitable for large pages) and fit in deficit
                    if chunk_size >= 16 * 1024 * 1024 && byte_deficit >= chunk_size {
                        let max_chunks = byte_deficit / chunk_size;
                        if max_chunks > 0 {
                            log::info!("NUMA {}: Filling remaining with up to {} × {}MB large pages",
                                     numa_node, max_chunks, chunk_mb);

                            let mut allocated_this_size = 0;
                            for _ in 0..max_chunks {
                                let config = AllocationConfig {
                                    size: chunk_size,
                                    numa_node: Some(numa_node),
                                    page_size: PageSizePreference::Require(PageType::Large(chunk_size)),
                                    memory_type: BufferMemoryType::WriteBack,
                                    zero_memory: true,
                                    timeout_ms: 5000,
                                    base_address: None,
                                    alignment: Some(2 * 1024 * 1024), // 2MB alignment for large pages
                                };

                                match self.allocate(&config) {
                                    Ok(buffer) => {
                                        log::info!("✅ NUMA {}: {}MB chunk allocated (2MB large)",
                                                 numa_node, chunk_mb);
                                        allocated_chunks.push(AllocatedChunk {
                                            buffer,
                                            chunk_size,
                                            numa_node,
                                        });
                                        byte_deficit = byte_deficit.saturating_sub(chunk_size);
                                        allocated_this_size += 1;
                                    }
                                    Err(e) => {
                                        log::info!("❌ NUMA {}: {}MB large page allocation failed: {}",
                                                 numa_node, chunk_mb, e);
                                        break; // Stop trying this size with large pages
                                    }
                                }
                            }

                            if allocated_this_size > 0 {
                                log::info!("NUMA {}: Successfully filled {} × {}MB with large pages",
                                         numa_node, allocated_this_size, chunk_mb);
                            }
                        }
                    }
                }

                log::info!("NUMA {}: Phase 2b complete, {} bytes still needed",
                         numa_node, byte_deficit);
            }
        }
        
        // Phase 3: Regular pages as last resort - try ALL chunk sizes
        let regular_allowed = is_page_size_allowed(PageSizeLevel::Regular, min_page, max_page);
        // Recompute deficit from actual allocations (byte_deficit may not be in scope if Phase 2 was skipped)
        let allocated_final: usize = allocated_chunks.iter().map(|c| c.chunk_size).sum();
        let mut remaining = total_needed.saturating_sub(allocated_final);
        if remaining > 0 && regular_allowed {
            log::info!("NUMA {}: Phase 3 - Using regular pages for remaining {} bytes", numa_node, remaining);

            let all_chunk_sizes_mb = [4096, 2048, 1024, 512, 256, 128, 64, 32, 16];

            for &chunk_mb in &all_chunk_sizes_mb {
                if remaining == 0 { break; }
                let chunk_size = (chunk_mb as usize) * 1024 * 1024;

                if remaining >= chunk_size {
                    let max_chunks = remaining / chunk_size;
                    if max_chunks > 0 {
                        log::info!("NUMA {}: Trying up to {} × {}MB with regular pages",
                                 numa_node, max_chunks, chunk_mb);

                        let mut allocated_this_size = 0;
                        for _ in 0..max_chunks {
                            let config = AllocationConfig {
                                size: chunk_size,
                                numa_node: Some(numa_node),
                                page_size: PageSizePreference::Prefer(PageType::Regular(chunk_size)),
                                memory_type: BufferMemoryType::WriteBack,
                                zero_memory: true,
                                timeout_ms: 5000,
                                base_address: None,
                                alignment: Some(64 * 1024),
                            };

                            match self.allocate(&config) {
                                Ok(buffer) => {
                                    log::info!("✅ NUMA {}: {}MB chunk allocated (4KB regular)",
                                             numa_node, chunk_mb);
                                    allocated_chunks.push(AllocatedChunk {
                                        buffer,
                                        chunk_size,
                                        numa_node,
                                    });
                                    remaining = remaining.saturating_sub(chunk_size);
                                    allocated_this_size += 1;
                                }
                                Err(e) => {
                                    log::info!("❌ NUMA {}: {}MB regular page allocation failed: {}",
                                             numa_node, chunk_mb, e);
                                    break; // Stop trying this size, try next size
                                }
                            }
                        }

                        if allocated_this_size > 0 {
                            log::info!("NUMA {}: Successfully allocated {} × {}MB with regular pages",
                                     numa_node, allocated_this_size, chunk_mb);
                        }
                    }
                }
            }

            log::info!("NUMA {}: Phase 3 complete, {} bytes still unallocated",
                     numa_node, remaining);
        }

        // Final allocation sanity check
        let final_allocated: usize = allocated_chunks.iter().map(|c| c.chunk_size).sum();
        if final_allocated > total_needed {
            log::warn!("NUMA {}: Over-allocation detected! Allocated {} bytes but only {} needed ({} bytes excess)",
                     numa_node, final_allocated, total_needed, final_allocated - total_needed);
        } else if final_allocated < total_needed {
            log::warn!("NUMA {}: Under-allocation: {} bytes allocated of {} needed ({} bytes short)",
                     numa_node, final_allocated, total_needed, total_needed - final_allocated);
        } else {
            log::info!("NUMA {}: Allocation complete: {} bytes allocated (exact match)", numa_node, final_allocated);
        }

        Ok(allocated_chunks)
    }

    /// Execute plan with BlockSizeFirst strategy
    fn execute_plan_block_size_first(
        &mut self,
        plan: &[(usize, usize)],
        numa_node: u32,
        runtime_config: &crate::RuntimeConfig,
    ) -> Result<Vec<AllocatedChunk>, String> {
        let mut allocated_chunks = Vec::new();

        // Get page size constraints from config
        let min_page = &runtime_config.memory_allocation.min_page_size;
        let max_page = &runtime_config.memory_allocation.max_page_size;
        let huge_allowed = is_page_size_allowed(PageSizeLevel::Huge, min_page, max_page);
        let large_allowed = is_page_size_allowed(PageSizeLevel::Large, min_page, max_page);
        let regular_allowed = is_page_size_allowed(PageSizeLevel::Regular, min_page, max_page);

        log::info!("NUMA {}: Page size constraints: min={}, max={} (huge={}, large={}, regular={})",
                 numa_node, min_page, max_page, huge_allowed, large_allowed, regular_allowed);

        // Phase 1: For each block size, try huge then large (skip regular)
        log::info!("NUMA {}: Phase 1 - Block-size-first with huge/large pages only", numa_node);

        for &(block_size, total_blocks_planned) in plan {
            let mut blocks_allocated = 0;

            // Try huge pages if size is eligible and allowed
            if block_size >= HUGE_PAGE_SIZE_USIZE && runtime_config.large_pages_available && huge_allowed {
                log::info!("NUMA {}: Trying {} × {}MB with huge pages",
                         numa_node, total_blocks_planned, block_size / (1024 * 1024));

                for _ in 0..total_blocks_planned {
                    let config = AllocationConfig {
                        size: block_size,
                        numa_node: Some(numa_node),
                        page_size: PageSizePreference::Require(PageType::Huge(block_size)),
                        memory_type: BufferMemoryType::WriteBack,
                        zero_memory: true,
                        timeout_ms: 5000,
                        base_address: None,
                        alignment: Some(HUGE_PAGE_SIZE_USIZE),
                    };

                    match self.allocate(&config) {
                        Ok(buffer) => {
                            blocks_allocated += 1;
                            log::info!("✅ NUMA {}: {}MB chunk allocated (1GB huge) - planned block {}/{}",
                                     numa_node, block_size / (1024 * 1024), blocks_allocated, total_blocks_planned);
                            allocated_chunks.push(AllocatedChunk {
                                buffer,
                                chunk_size: block_size,
                                numa_node,
                            });
                        }
                        Err(e) => {
                            log::info!("❌ NUMA {}: {}MB huge page allocation failed: {}",
                                     numa_node, block_size / (1024 * 1024), e);
                            break; // Try large pages
                        }
                    }
                }
            }
            
            // Try large pages for remaining blocks (if allowed)
            let remaining_blocks = total_blocks_planned - blocks_allocated;
            if remaining_blocks > 0 && block_size >= 16 * 1024 * 1024 && runtime_config.large_pages_available && large_allowed {
                log::info!("NUMA {}: Trying {} × {}MB with large pages",
                         numa_node, remaining_blocks, block_size / (1024 * 1024));
                
                for _ in 0..remaining_blocks {
                    let config = AllocationConfig {
                        size: block_size,
                        numa_node: Some(numa_node),
                        page_size: PageSizePreference::Require(PageType::Large(block_size)),
                        memory_type: BufferMemoryType::WriteBack,
                        zero_memory: true,
                        timeout_ms: 5000,
                        base_address: None,
                        alignment: Some(2 * 1024 * 1024),
                    };
                    
                    match self.allocate(&config) {
                        Ok(buffer) => {
                            blocks_allocated += 1;
                            log::info!("✅ NUMA {}: {}MB chunk allocated (2MB large) - planned block {}/{}",
                                     numa_node, block_size / (1024 * 1024), blocks_allocated, total_blocks_planned);
                            allocated_chunks.push(AllocatedChunk {
                                buffer,
                                chunk_size: block_size,
                                numa_node,
                            });
                        }
                        Err(e) => {
                            log::info!("❌ NUMA {}: {}MB large page allocation failed: {}",
                                     numa_node, block_size / (1024 * 1024), e);
                            break; // Move to next block size
                        }
                    }
                }
            }
        }
        
        // Phase 2: Regular pages as absolute last resort (if allowed)
        let allocated_so_far: usize = allocated_chunks.iter().map(|c| c.chunk_size).sum();
        let total_needed = plan.iter().map(|(size, count)| size * count).sum::<usize>();

        if allocated_so_far < total_needed && regular_allowed {
            log::info!("NUMA {}: Phase 2 - Regular pages as last resort", numa_node);
            
            for &(block_size, total_blocks_planned) in plan {
                let already_allocated = allocated_chunks.iter()
                    .filter(|c| c.chunk_size == block_size)
                    .count();
                let still_needed = total_blocks_planned.saturating_sub(already_allocated);
                
                if still_needed > 0 {
                    log::info!("NUMA {}: Last resort - {} × {}MB with regular pages",
                             numa_node, still_needed, block_size / (1024 * 1024));

                    let mut phase2_allocated = 0;
                    for _ in 0..still_needed {
                        let config = AllocationConfig {
                            size: block_size,
                            numa_node: Some(numa_node),
                            page_size: PageSizePreference::Prefer(PageType::Regular(block_size)),
                            memory_type: BufferMemoryType::WriteBack,
                            zero_memory: true,
                            timeout_ms: 5000,
                            base_address: None,
                            alignment: Some(64 * 1024),
                        };

                        match self.allocate(&config) {
                            Ok(buffer) => {
                                phase2_allocated += 1;
                                log::info!("✅ NUMA {}: {}MB chunk allocated (4KB regular) - planned block {}/{}",
                                         numa_node, block_size / (1024 * 1024),
                                         already_allocated + phase2_allocated, total_blocks_planned);
                                allocated_chunks.push(AllocatedChunk {
                                    buffer,
                                    chunk_size: block_size,
                                    numa_node,
                                });
                            }
                            Err(e) => {
                                log::info!("❌ NUMA {}: {}MB regular page allocation failed: {}",
                                         numa_node, block_size / (1024 * 1024), e);
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Final allocation sanity check
        let final_allocated: usize = allocated_chunks.iter().map(|c| c.chunk_size).sum();
        let total_needed_bytes = plan.iter().map(|(size, count)| size * count).sum::<usize>();
        if final_allocated > total_needed_bytes {
            log::warn!("NUMA {}: Over-allocation detected! Allocated {} bytes but only {} needed ({} bytes excess)",
                     numa_node, final_allocated, total_needed_bytes, final_allocated - total_needed_bytes);
        } else if final_allocated < total_needed_bytes {
            log::warn!("NUMA {}: Under-allocation: {} bytes allocated of {} needed ({} bytes short)",
                     numa_node, final_allocated, total_needed_bytes, total_needed_bytes - final_allocated);
        } else {
            log::info!("NUMA {}: Allocation complete: {} bytes allocated (exact match)", numa_node, final_allocated);
        }

        Ok(allocated_chunks)
    }

    /// Distribute planned chunks fairly to threads according to the plan
    /// Groups consecutive blocks per thread (e.g., 16×2GB blocks: thread0 gets blocks 0&1, thread1 gets blocks 2&3, etc.)
    fn distribute_planned_chunks(
        &mut self,
        chunks: Vec<AllocatedChunk>,
        plan: &[(usize, usize)],
        numa_threads: &[usize],
        thread_assignments: &std::collections::HashMap<usize, (usize, u32)>,
        thread_blocks: &std::collections::HashMap<usize, Vec<BlockInfo>>,
    ) -> Result<std::collections::HashMap<usize, Vec<AllocationBlock>>, String> {
        use std::collections::HashMap;
        
        let mut thread_allocations = HashMap::new();
        let mut all_chunks = chunks;
        
        log::info!("Distributing {} chunks to {} threads with consecutive block grouping",
                 all_chunks.len(), numa_threads.len());
        
        // Process each block size in the plan
        for &(block_size, total_blocks) in plan {
            let blocks_per_thread = total_blocks / numa_threads.len();
            
            if blocks_per_thread == 0 {
                log::warn!("Not enough {}MB blocks for all threads: {} total ÷ {} threads",
                         block_size / (1024 * 1024), total_blocks, numa_threads.len());
                continue;
            }
            
            log::info!("Distributing {} × {}MB blocks ({} consecutive blocks per thread)",
                     total_blocks, block_size / (1024 * 1024), blocks_per_thread);
            
            // Extract chunks of this size from the main collection
            let (mut size_chunks, remaining): (Vec<_>, Vec<_>) = all_chunks
                .into_iter()
                .partition(|chunk| chunk.chunk_size == block_size);
            
            // Keep only the number we need for this block size
            size_chunks.truncate(total_blocks);
            all_chunks = remaining;
            
            if size_chunks.len() < total_blocks {
                log::warn!("Only allocated {} of {} requested {}MB blocks",
                         size_chunks.len(), total_blocks, block_size / (1024 * 1024));
            }
            
            // Distribute consecutive blocks to each thread
            let mut chunk_iter = size_chunks.into_iter();
            for &thread_id in numa_threads {
                for block_num in 0..blocks_per_thread {
                    if let Some(chunk) = chunk_iter.next() {
                        
                        let block_info = thread_blocks[&thread_id]
                            .get(block_num)
                            .cloned()
                            .unwrap_or(BlockInfo {
                                size_bytes: chunk.chunk_size,
                                thread_id,
                            });
                        
                        let allocated_block = AllocationBlock {
                            buffer: chunk.buffer,
                            block_info,
                        };
                        
                        thread_allocations.entry(thread_id)
                            .or_insert_with(Vec::new)
                            .push(allocated_block);
                    }
                }
            }
        }
        
        // Handle any remaining chunks not in the plan (from Phase 2b/3)
        if !all_chunks.is_empty() {
            log::info!("Distributing {} remaining chunks not in plan (round-robin to fill gaps)",
                     all_chunks.len());
            
            // Calculate how much each thread still needs to reach target
            let mut thread_gaps: Vec<(usize, usize)> = Vec::new();  // (thread_id, bytes_needed)
            for &thread_id in numa_threads {
                let current_total: usize = thread_allocations.get(&thread_id)
                    .map(|blocks| blocks.iter().map(|b| b.buffer.size()).sum())
                    .unwrap_or(0);
                let target = thread_assignments[&thread_id].0;
                
                if current_total < target {
                    let gap = target - current_total;
                    thread_gaps.push((thread_id, gap));
                }
            }
            
            // Sort by gap size (largest gaps first) for better fairness
            thread_gaps.sort_by_key(|b| std::cmp::Reverse(b.1));

            // Sort remaining chunks by size (largest first) for efficient filling
            all_chunks.sort_by_key(|b| std::cmp::Reverse(b.chunk_size));
            
            let mut remaining_chunks = all_chunks;
            for (thread_id, gap) in thread_gaps {
                let mut filled = 0;
                let mut chunks_used = Vec::new();
                
                // Find chunks that fit in this thread's gap
                for (i, chunk) in remaining_chunks.iter().enumerate() {
                    if filled + chunk.chunk_size <= gap {
                        filled += chunk.chunk_size;
                        chunks_used.push(i);
                        
                        log::info!("Thread {}: Adding extra {}MB chunk to fill gap",
                                 thread_id, chunk.chunk_size / (1024 * 1024));
                    }
                }
                
                // Move used chunks to thread (reverse order to maintain indices)
                for &i in chunks_used.iter().rev() {
                    let chunk = remaining_chunks.remove(i);
                    let block_info = BlockInfo {
                        size_bytes: chunk.chunk_size,
                        thread_id,
                    };
                    
                    let allocated_block = AllocationBlock {
                        buffer: chunk.buffer,
                        block_info,
                    };
                    
                    thread_allocations.entry(thread_id)
                        .or_insert_with(Vec::new)
                        .push(allocated_block);
                }
                
                if filled > 0 {
                    log::info!("Thread {}: Filled {} bytes of {} byte gap with extra chunks",
                             thread_id, filled, gap);
                }
            }
            
            if !remaining_chunks.is_empty() {
                log::warn!("Still have {} unallocated chunks after gap filling", remaining_chunks.len());
                for chunk in &remaining_chunks {
                    log::warn!("Unallocated: {}MB chunk", chunk.chunk_size / (1024 * 1024));
                }
            }
        }
        
        // Log final distribution with per-thread deviation warnings
        log::info!("Plan-based distribution complete:");
        let mut total_distributed: usize = 0;
        let mut total_target: usize = 0;
        for &thread_id in numa_threads {
            let thread_total: usize = thread_allocations.get(&thread_id)
                .map(|blocks| blocks.iter().map(|b| b.buffer.size()).sum())
                .unwrap_or(0);
            let target = thread_assignments[&thread_id].0;
            total_distributed += thread_total;
            total_target += target;

            if thread_total > target {
                log::warn!("Thread {}: {:.2}GB allocated (target: {:.2}GB) — ⚠️ over-distributed by {:.2}GB",
                         thread_id,
                         bytes_to_gib_f64(thread_total as u64),
                         bytes_to_gib_f64(target as u64),
                         bytes_to_gib_f64((thread_total - target) as u64));
            } else if thread_total < target {
                log::warn!("Thread {}: {:.2}GB allocated (target: {:.2}GB) — ⚠️ under-distributed by {:.2}GB",
                         thread_id,
                         bytes_to_gib_f64(thread_total as u64),
                         bytes_to_gib_f64(target as u64),
                         bytes_to_gib_f64((target - thread_total) as u64));
            } else {
                log::info!("Thread {}: {:.2}GB allocated (target: {:.2}GB)",
                         thread_id,
                         bytes_to_gib_f64(thread_total as u64),
                         bytes_to_gib_f64(target as u64));
            }
        }
        log::info!("Distribution summary: {:.2}GB distributed to threads of {:.2}GB target",
                 bytes_to_gib_f64(total_distributed as u64),
                 bytes_to_gib_f64(total_target as u64));

        Ok(thread_allocations)
    }
    
    /// Stage 1: Discover largest available chunks per NUMA node with smart greedy fairness
    fn discover_chunks_per_numa(
        &mut self,
        total_size_per_numa: &std::collections::HashMap<u32, usize>,
        runtime_config: &crate::RuntimeConfig,
        thread_count: usize,
    ) -> Result<Vec<AllocatedChunk>, String> {
        
        // Power-of-2 sizes: 4GB, 2GB, 1GB, 512MB, 256MB, 128MB, 64MB, 32MB, 16MB (minimum per requirements)
        let chunk_sizes_mb = [4096, 2048, 1024, 512, 256, 128, 64, 32, 16];
        let mut allocated_chunks = Vec::new();
        
        for (&numa_node, &total_needed) in total_size_per_numa {
            let mut remaining = total_needed;
            let phase_params = PageTypeAllocParams {
                numa_node,
                chunk_sizes_mb: &chunk_sizes_mb,
                runtime_config,
                thread_count,
            };

            log::info!("NUMA node {}: Allocating {:.2} GiB in power-of-2 chunks", 
                      numa_node, bytes_to_gib_f64(total_needed as u64));
            
            // Phase 1: Huge pages (1GB) - try all chunk sizes with huge pages first
            if runtime_config.large_pages_available {
                log::info!("NUMA {}: Phase 1 - Trying huge pages (1GB)", numa_node);
                self.allocate_with_page_type(
                    &mut allocated_chunks, &mut remaining, "huge", &phase_params
                )?;
            }
            
            // Phase 2: Large pages (2MB) - restart chunk sizes for large pages
            if remaining > 0 && runtime_config.large_pages_available {
                log::info!("NUMA {}: Phase 2 - Trying large pages (2MB)", numa_node);
                self.allocate_with_page_type(
                    &mut allocated_chunks, &mut remaining, "large", &phase_params
                )?;
            }
            
            // Phase 3: Regular pages (4KB) - restart chunk sizes for regular pages
            if remaining > 0 {
                log::info!("NUMA {}: Phase 3 - Trying regular pages (4KB)", numa_node);
                self.allocate_with_page_type(
                    &mut allocated_chunks, &mut remaining, "regular", &phase_params
                )?;
            }
            
            if remaining > 16 * 1024 * 1024 {
                log::warn!("NUMA node {}: {:.2} MB unallocated (system memory fragmentation)", 
                          numa_node, remaining as f64 / (1024.0 * 1024.0));
            }
        }
        
        Ok(allocated_chunks)
    }
    
    /// Stage 2: Distribute chunks to threads prioritizing contiguous allocation
    fn distribute_chunks_to_threads(
        &mut self,
        chunks: Vec<AllocatedChunk>,
        thread_assignments: &std::collections::HashMap<usize, (usize, u32)>,
        thread_blocks: &std::collections::HashMap<usize, Vec<BlockInfo>>,
    ) -> Result<std::collections::HashMap<usize, Vec<AllocationBlock>>, String> {
        use std::collections::HashMap;
        
        let mut thread_allocations = HashMap::new();
        
        // Group chunks by NUMA node (already sorted by size from allocation order)
        let mut numa_chunks: HashMap<u32, Vec<AllocatedChunk>> = HashMap::new();
        for chunk in chunks {
            numa_chunks.entry(chunk.numa_node).or_default().push(chunk);
        }
        
        // Count threads per NUMA node for fair distribution
        let mut threads_per_numa: HashMap<u32, Vec<usize>> = HashMap::new();
        for (thread_id, (_, numa_node)) in thread_assignments {
            threads_per_numa.entry(*numa_node).or_default().push(*thread_id);
        }
        
        // Distribute chunks within each NUMA node
        for (numa_node, available_chunks) in numa_chunks {
            let numa_threads = &threads_per_numa[&numa_node];
            
            log::info!("NUMA node {}: Distributing {} chunks to {} threads", 
                      numa_node, available_chunks.len(), numa_threads.len());
            
            // Separate chunks by page type first, then by size
            let mut chunks_by_page_type_and_size: HashMap<(bool, usize), Vec<AllocatedChunk>> = HashMap::new();
            
            for chunk in available_chunks {
                let is_huge_page = chunk.buffer.uses_huge_pages();
                let size = chunk.chunk_size;
                chunks_by_page_type_and_size.entry((is_huge_page, size)).or_default().push(chunk);
            }
            
            // Sort by huge pages first (true sorts before false), then by size descending
            let mut sorted_groups: Vec<_> = chunks_by_page_type_and_size.into_iter().collect();
            sorted_groups.sort_by(|a, b| {
                // Sort by huge page first (huge pages = true come first)
                match b.0.0.cmp(&a.0.0) {
                    std::cmp::Ordering::Equal => {
                        // If same page type, sort by size descending
                        b.0.1.cmp(&a.0.1)
                    }
                    other => other
                }
            });
            
            // Track allocated amounts per thread to handle deficits
            let mut thread_allocated: HashMap<usize, usize> = HashMap::new();
            let mut thread_targets: HashMap<usize, usize> = HashMap::new();
            
            for &thread_id in numa_threads {
                thread_allocated.insert(thread_id, 0);
                thread_targets.insert(thread_id, thread_assignments[&thread_id].0);
            }
            
            // Fair distribution with sequential blocks
            let mut is_first_page_type = true;
            
            for ((is_huge_page, chunk_size), mut same_type_chunks) in sorted_groups {
                let page_type = if is_huge_page { "huge page" } else { "large/regular page" };
                let total_chunks = same_type_chunks.len();
                
                log::info!("NUMA {}: Processing {} × {}MB {} chunks", 
                           numa_node, total_chunks, chunk_size / (1024*1024), page_type);
                
                if same_type_chunks.is_empty() {
                    continue;
                }
                
                // Reverse the chunks so we can pop from the front (maintaining sequential order)
                same_type_chunks.reverse();
                
                if is_first_page_type {
                    // FIRST PAGE TYPE: Fair distribution with sequential blocks
                    log::info!("NUMA {}: First page type - fair sequential distribution", numa_node);
                    
                    let num_threads = numa_threads.len();
                    let blocks_per_thread = total_chunks / num_threads;
                    let remainder = total_chunks % num_threads;
                    
                    log::info!("NUMA {}: {} blocks ÷ {} threads = {} per thread, {} remainder", 
                              numa_node, total_chunks, num_threads, blocks_per_thread, remainder);
                    
                    let mut block_index = 0;
                    for (thread_idx, &thread_id) in numa_threads.iter().enumerate() {
                        let mut blocks_to_give = blocks_per_thread;
                        if thread_idx < remainder {
                            blocks_to_give += 1; // First 'remainder' threads get +1 block
                        }
                        
                        if blocks_to_give > 0 {
                            log::info!("NUMA {}: Thread {} gets {} × {}MB {} blocks [{}..{}]", 
                                      numa_node, thread_id, blocks_to_give, chunk_size / (1024*1024), 
                                      page_type, block_index, block_index + blocks_to_give - 1);
                            
                            // Give sequential blocks to this thread
                            for _ in 0..blocks_to_give {
                                if let Some(chunk) = same_type_chunks.pop() {
                                    let block_info = thread_blocks[&thread_id].first()
                                        .cloned()
                                        .unwrap_or(BlockInfo {
                                            size_bytes: chunk.chunk_size,
                                            thread_id,
                                        });
                                    
                                    let allocated_block = AllocationBlock {
                                        buffer: chunk.buffer,
                                        block_info,
                                    };
                                    
                                    thread_allocations.entry(thread_id).or_insert_with(Vec::new).push(allocated_block);
                                    *thread_allocated.get_mut(&thread_id).unwrap() += chunk_size;
                                    block_index += 1;
                                }
                            }
                        }
                    }
                    
                    is_first_page_type = false;
                } else {
                    // SUBSEQUENT PAGE TYPES: Deficit-aware sequential fill
                    log::info!("NUMA {}: Subsequent page type - deficit fill only", numa_node);
                    
                    // Find threads with deficits
                    let mut deficit_threads: Vec<(usize, usize)> = Vec::new();
                    for &thread_id in numa_threads {
                        let target = thread_targets[&thread_id];
                        let allocated = thread_allocated[&thread_id];
                        if allocated < target {
                            let deficit = target - allocated;
                            deficit_threads.push((thread_id, deficit));
                        }
                    }
                    
                    if deficit_threads.is_empty() {
                        log::info!("NUMA {}: All threads satisfied, no deficits remain", numa_node);
                        break;
                    }
                    
                    // Calculate blocks needed per deficit thread
                    let mut thread_blocks_needed: Vec<(usize, usize)> = Vec::new();
                    let mut total_blocks_requested = 0;
                    
                    for (thread_id, deficit_bytes) in deficit_threads {
                        let blocks_needed = deficit_bytes.div_ceil(chunk_size); // Round up
                        thread_blocks_needed.push((thread_id, blocks_needed));
                        total_blocks_requested += blocks_needed;
                    }
                    
                    log::info!("NUMA {}: {} deficit threads need {} total blocks (have {} available)", 
                              numa_node, thread_blocks_needed.len(), total_blocks_requested, total_chunks);
                    
                    // Give sequential chunks to each deficit thread
                    for (thread_id, blocks_needed) in thread_blocks_needed {
                        let blocks_to_give = std::cmp::min(blocks_needed, same_type_chunks.len());
                        
                        if blocks_to_give > 0 {
                            log::info!("NUMA {}: Thread {} deficit fill → {} × {}MB chunks (sequential)", 
                                      numa_node, thread_id, blocks_to_give, chunk_size / (1024*1024));
                            
                            for _ in 0..blocks_to_give {
                                if let Some(chunk) = same_type_chunks.pop() {
                                    let block_info = thread_blocks[&thread_id].first()
                                        .cloned()
                                        .unwrap_or(BlockInfo {
                                            size_bytes: chunk.chunk_size,
                                            thread_id,
                                        });
                                    
                                    let allocated_block = AllocationBlock {
                                        buffer: chunk.buffer,
                                        block_info,
                                    };
                                    
                                    thread_allocations.entry(thread_id).or_insert_with(Vec::new).push(allocated_block);
                                    *thread_allocated.get_mut(&thread_id).unwrap() += chunk_size;
                                }
                            }
                        }
                        
                        if same_type_chunks.is_empty() {
                            break;
                        }
                    }
                }
            }
            
            // Log final allocation summary
            log::info!("NUMA {}: Final allocation summary:", numa_node);
            for &thread_id in numa_threads {
                let target = thread_targets[&thread_id];
                let allocated = thread_allocated[&thread_id];
                let deficit = target as i64 - allocated as i64;
                log::info!("NUMA {}: Thread {} - Target: {:.1}MB, Allocated: {:.1}MB, Deficit: {:.1}MB", 
                          numa_node, thread_id, 
                          target as f64 / (1024.0*1024.0), 
                          allocated as f64 / (1024.0*1024.0), 
                          deficit as f64 / (1024.0*1024.0));
            }
        }
        
        Ok(thread_allocations)
    }
    
    
    /// Helper function to allocate with a specific page type, trying all chunk sizes
    fn allocate_with_page_type(
        &mut self,
        allocated_chunks: &mut Vec<AllocatedChunk>,
        remaining: &mut usize,
        page_type: &str,
        params: &PageTypeAllocParams,
    ) -> Result<(), String> {
        let PageTypeAllocParams { numa_node, chunk_sizes_mb, runtime_config, thread_count } = *params;
        for &chunk_mb in chunk_sizes_mb {
            let chunk_size = (chunk_mb as usize) * 1024 * 1024;
            
            // Skip chunk sizes larger than remaining memory
            if *remaining < chunk_size {
                continue;
            }
            
            // Smart Greedy: Skip chunk size if it can't be distributed fairly
            // Only allocate chunk_size if remaining memory allows at least 1 chunk per thread
            let chunks_possible = *remaining / chunk_size;
            if chunks_possible < thread_count {
                log::info!("NUMA {}: Smart Greedy: Skipping {}MB chunks (only {} possible for {} threads - ensuring fairness)", 
                          numa_node, chunk_mb, chunks_possible, thread_count);
                continue;
            }
            
            // Determine page size preference based on page type
            let page_size_pref = match page_type {
                "huge" => {
                    // Check if large pages are available before trying huge pages
                    if !runtime_config.large_pages_available {
                        continue; // Skip huge pages if not available
                    }
                    // Only try huge pages for 1GB+ chunks
                    if chunk_size >= HUGE_PAGE_SIZE_USIZE {
                        PageSizePreference::Require(PageType::Huge(chunk_size))
                    } else {
                        continue; // Skip smaller chunks for huge pages
                    }
                }
                "large" => {
                    // Check if large pages are available before trying large pages
                    if !runtime_config.large_pages_available {
                        continue; // Skip large pages if not available
                    }
                    // Try large pages for 16MB+ chunks
                    if chunk_size >= 16 * 1024 * 1024 {
                        PageSizePreference::Require(PageType::Large(chunk_size))
                    } else {
                        continue; // Skip smaller chunks for large pages
                    }
                }
                "regular" => PageSizePreference::Prefer(PageType::Regular(chunk_size)),
                _ => return Err(format!("Unknown page type: {}", page_type)),
            };
            
            // Continue allocating chunks of this size until we can't get more
            while *remaining >= chunk_size {
                let config = AllocationConfig {
                    size: chunk_size,
                    numa_node: Some(numa_node),
                    page_size: page_size_pref.clone(),
                    memory_type: BufferMemoryType::WriteBack,
                    zero_memory: true,
                    timeout_ms: 5000,
                    base_address: None,
                    alignment: Some(Self::get_alignment_for_chunk_size(chunk_size)),
                };
                
                match self.allocate(&config) {
                    Ok(buffer) => {
                        let actual_page_type = if buffer.uses_huge_pages() { "1GB huge" } 
                                               else if buffer.uses_large_pages() { "2MB large" } 
                                               else { "4KB regular" };
                        
                        log::info!("✅ NUMA {}: {}MB chunk allocated ({})", 
                                  numa_node, chunk_mb, actual_page_type);
                        
                        allocated_chunks.push(AllocatedChunk {
                            buffer,
                            chunk_size,
                            numa_node,
                        });
                        *remaining -= chunk_size;
                    }
                    Err(e) => {
                        log::info!("❌ NUMA {}: {}MB {} exhausted - trying next size", 
                                  numa_node, chunk_mb, page_type);
                        log::debug!("NUMA {}: {}MB chunk allocation failed: {}", numa_node, chunk_mb, e);
                        // Can't allocate this size anymore, try next smaller size
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// Get alignment requirements for chunk size following page boundaries per Copilot criteria
    fn get_alignment_for_chunk_size(chunk_size: usize) -> usize {
        if chunk_size >= HUGE_PAGE_SIZE_USIZE {
            // 1GB chunks: align to 1GB boundary for optimal huge page allocation
            HUGE_PAGE_SIZE_USIZE
        } else if chunk_size >= 16 * 1024 * 1024 {
            // Any chunk that might get large pages: align to 2MB boundary (large page size)
            2 * 1024 * 1024
        } else {
            // Smaller chunks that won't get large pages: align to 64KB boundary (Windows allocation granularity)
            64 * 1024
        }
    }
    
    
}

/// Allocated chunk structure for two-stage allocation
#[derive(Debug)]
struct AllocatedChunk {
    buffer: MemoryBuffer,
    chunk_size: usize,
    numa_node: u32,
}

/// Invariant inputs to a single page-type allocation phase (huge/large/regular).
/// These stay constant across the phases for one NUMA node; only the page type
/// and the running accumulators differ per call.
struct PageTypeAllocParams<'a> {
    numa_node: u32,
    chunk_sizes_mb: &'a [u32],
    runtime_config: &'a crate::RuntimeConfig,
    thread_count: usize,
}

impl Default for AllocationConfig {
    fn default() -> Self {
        Self {
            size: 1024 * 1024, // 1MB default
            numa_node: None,
            page_size: PageSizePreference::Any,
            memory_type: BufferMemoryType::WriteBack,
            zero_memory: false,
            timeout_ms: 10000,
            base_address: None,
            alignment: None,
        }
    }
}

impl AllocationConfig {
    pub fn new(size: usize) -> Self {
        Self {
            size,
            ..Default::default()
        }
    }
    
    pub fn with_numa_node(mut self, numa_node: u32) -> Self {
        self.numa_node = Some(numa_node);
        self
    }
    
    pub fn with_page_size(mut self, page_size: PageSizePreference) -> Self {
        self.page_size = page_size;
        self
    }
    
    pub fn with_memory_type(mut self, memory_type: BufferMemoryType) -> Self {
        self.memory_type = memory_type;
        self
    }
    
    pub fn with_zero_memory(mut self, zero_memory: bool) -> Self {
        self.zero_memory = zero_memory;
        self
    }
    
    pub fn with_timeout(mut self, timeout_ms: u32) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }
    
    pub fn with_base_address(mut self, base_address: *mut u8) -> Self {
        self.base_address = Some(base_address);
        self
    }
    
    pub fn with_alignment(mut self, alignment: usize) -> Self {
        self.alignment = Some(alignment);
        self
    }
}