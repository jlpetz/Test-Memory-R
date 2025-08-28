pub mod interface;
pub mod statistics;
pub mod types;
pub mod utils;

// Re-export main driver functionality
pub use interface::{DriverHandle, get_global_driver_handle, is_driver_connected, reset_driver_state};
pub use statistics::{AppDriverStats, DriverStatType, track_call};
pub use types::{DriverStatus, DriverVersionInfo, CompatibilityFlags, PageSize, MemoryType, DriverVersionError, RemapAllInput, RemapAllOutput, BatchRemapInput, BatchRemapOutput, RemapRequest, ThreadAllocationRequest, AllocationResult, EnhancedBatchAllocateInput, EnhancedBatchAllocateOutput};
pub use utils::{reset_driver, display_driver_info, check_and_display_driver_status, refresh_driver_status, display_driver_stats, set_use_remap_all, get_use_remap_all, print_driver_info, compare_app_vs_driver_stats, reset_app_driver_stats, display_app_driver_stats_table};