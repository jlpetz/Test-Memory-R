pub mod allocation_strategy;
pub mod allocator;
pub mod backend;
pub mod buffer;
pub mod fill;
pub mod privileges;
pub mod stitched;

pub use allocator::MemoryAllocator;
pub use buffer::MemoryBuffer;
pub use privileges::{check_large_page_privilege, check_restart_needed, diagnose_and_setup_large_pages};
