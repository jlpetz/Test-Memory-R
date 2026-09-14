pub mod allocator;
pub mod buffer;
pub mod backend;
pub mod privileges;
pub mod allocation_strategy;

// New unified system exports
pub use allocator::{MemoryAllocator, AllocationConfig, PageSizePreference};
pub use buffer::{MemoryBuffer, BufferInfo, PageType, MemoryType, get_total_system_memory, SegmentInfo};
pub use backend::{Backend, BackendType, WindowsBackend};
pub use privileges::{setup_large_pages_automatically, check_large_page_privilege, diagnose_and_setup_large_pages, auto_grant_large_page_privilege, check_restart_needed};