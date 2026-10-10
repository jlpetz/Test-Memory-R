//! Compile-only demonstration for TODO 27's proposed repeated cold-path reads.
//! This does not execute TMR or read potentially faulty memory.
#![no_std]

/// Four ordinary observations; the optimizer is allowed to merge the loads.
///
/// # Safety
/// `ptr` must point to a valid, aligned, initialized u64 without concurrent writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plain_four_reads(ptr: *const u64, expected: u64) -> u64 {
    let mut differences = 0;
    for _ in 0..4 {
        // SAFETY: required by this function's contract.
        differences |= unsafe { ptr.read() } ^ expected;
    }
    differences
}

/// Four volatile observations. This enforces loads, not cache eviction or delay.
///
/// # Safety
/// `ptr` must point to a valid, aligned, initialized u64 without concurrent writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn volatile_four_reads(ptr: *const u64, expected: u64) -> u64 {
    let mut differences = 0;
    for _ in 0..4 {
        // SAFETY: required by this function's contract.
        differences |= unsafe { ptr.read_volatile() } ^ expected;
    }
    differences
}
