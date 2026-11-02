pub mod allocation_strategy;
pub mod allocator;
pub mod backend;
pub mod buffer;
pub mod fill;
pub mod privileges;
pub mod stitched;

pub use allocator::MemoryAllocator;
pub use buffer::MemoryBuffer;
pub use privileges::{check_large_page_privilege, diagnose_large_pages, grant_large_pages, large_page_question, setup_large_pages};
