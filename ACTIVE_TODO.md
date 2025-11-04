# Active Todo List - MultiBlock Migration

**Last Updated**: 2025-11-04 (after StuckBitTest base migration complete)

## Status Summary
- **Completed**: StuckBitTest (base scalar) migrated to MultiBlock
- **In Progress**: None (ready for next task)
- **Next Up**: Migrate StuckBitTest SIMD variants (128/256/512/Auto)

---

## Migration Progress Tracker

### Overall Statistics
**Total Functions**: 48
- ✅ **Migrated (NEW MultiBlock)**: 18 functions (37.5%)
- 🗑️ **Old versions (to delete)**: 16 functions (33.3%)
- ❌ **To migrate**: 14 functions (29.2%)

**Functional Test Coverage** (test families):
- ✅ **Migrated**: 7 test families
- ❌ **To migrate**: 12 test families

**Migration Progress**: 18 / 32 active functions = **56.25%** complete

---

### ✅ MIGRATED - MultiBlock (NEW) - 18 functions

#### SimpleTest (5 functions)
- ✅ `simple_test_multi` (line 4581) - pub dispatcher
- ✅ `simple_test_stream1_multi` (line 4627)
- ✅ `simple_test_stream2_multi` (line 4814)
- ✅ `simple_test_stream4_multi` (line 5022)
- ✅ `simple_test_stream_n_multi` (line 5256)

#### MirrorMove base (1 function)
- ✅ `mirror_move_multi` (line 5470)

#### MirrorMove128 (3 functions)
- ✅ `mirror_move_128_multi` (line 6279) - pub dispatcher
- ✅ `mirror_move_128_stream1_multi` (line 5702)
- ✅ `mirror_move_128_stream_n_multi` (line 5973)

#### MirrorMove256 (3 functions)
- ✅ `mirror_move_256_multi` (line 6910) - pub dispatcher
- ✅ `mirror_move_256_stream1_multi` (line 6305)
- ✅ `mirror_move_256_stream_n_multi` (line 6599)

#### MirrorMove512 (3 functions)
- ✅ `mirror_move_512_multi` (line 7587) - pub dispatcher
- ✅ `mirror_move_512_stream1_multi` (line 6934)
- ✅ `mirror_move_512_stream_n_multi` (line 7252)

#### MirrorMoveAuto (1 function)
- ✅ `mirror_move_auto_multi` (line 2828)

#### MirrorMove base stream variants (1 function)
- ✅ `mirror_move_multi` - single implementation

#### StuckBitTest base (1 function)
- ✅ `stuck_bit_test_multi` (line 1154)

---

### 🗑️ OLD VERSIONS (to delete after all migrations) - 16 functions

#### SimpleTest OLD (5 functions)
- 🗑️ `simple_test` (line 4531)
- 🗑️ `simple_test_stream1` (line 7606)
- 🗑️ `simple_test_stream2` (line 7721)
- 🗑️ `simple_test_stream4` (line 7846)
- 🗑️ `simple_test_stream_n` (line 7994)

#### MirrorMove OLD (11 functions)
- 🗑️ `mirror_move` (line 2613)
- 🗑️ `mirror_move_auto` (line 2802)
- 🗑️ `mirror_move_128` (line 2858)
  - 🗑️ `mirror_move_128_single_stream` (line 2900)
  - 🗑️ `mirror_move_128_multi_stream` (line 3117)
- 🗑️ `mirror_move_256` (line 3371)
  - 🗑️ `mirror_move_256_single_stream` (line 3413)
  - 🗑️ `mirror_move_256_multi_stream` (line 3652)
- 🗑️ `mirror_move_512` (line 3927)
  - 🗑️ `mirror_move_512_single_stream` (line 3969)
  - 🗑️ `mirror_move_512_multi_stream` (line 4232)

---

### ❌ TO MIGRATE - 14 functions

#### StuckBitTest SIMD variants (4 functions - no stream variants)
- ❌ `stuck_bit_test_128` (line 1146)
- ❌ `stuck_bit_test_256` (line 1422)
- ❌ `stuck_bit_test_512` (line 1694)
- ❌ `stuck_bit_test_auto` (line 1996)

#### RefreshStable (5 functions - likely no stream variants)
- ❌ `refresh_stable` (line 8129)
- ❌ `refresh_stable_128` (line 2012)
- ❌ `refresh_stable_256` (line 2203)
- ❌ `refresh_stable_512` (line 2394)
- ❌ `refresh_stable_auto` (line 2586)

#### Standalone Tests (5 functions)
- ❌ `cache_busting_write_test` (line 8247)
- ❌ `random_access_torture_test` (line 8419)
- ❌ `stride_access_test` (line 8575)
- ❌ `bandwidth_saturation_test` (line 8752)
- ❌ `block_move_test` (line 8906)

---

## Current Todo Items (13 total)

### 1. [PENDING] Test SimpleTest with different stream configurations
**Status**: Pending
**Priority**: Medium
**Description**: Validate SimpleTest works correctly with streams=1, 2, 4, N configurations
**Commands**:
```bash
./target/release/tmr.exe --single-test=SimpleTest streams=1
./target/release/tmr.exe --single-test=SimpleTest streams=2
./target/release/tmr.exe --single-test=SimpleTest streams=4
./target/release/tmr.exe --single-test=SimpleTest streams=8
```

---

### 2. [COMPLETED] Migrate MirrorMove512 (AVX-512) to MultiBlock
**Status**: ✅ Completed (2025-11-04)
**Priority**: High
**Description**: Follow MULTIBLOCK_MIGRATION.md checklist to migrate MirrorMove512
**Files modified**:
- `src/tests.rs` (lines 6902-7577) - Created stream1_multi, stream_n_multi, and dispatcher (~675 lines)
- `src/runner.rs` (line 8) - Updated imports
- `src/runner.rs` (line 766) - Updated registration to MultiBlock
**Result**: Builds with zero warnings, test runs correctly and detects AVX-512 not available

---

### 3. [COMPLETED] Update MirrorMoveAuto resolver
**Status**: ✅ Completed (2025-11-04)
**Priority**: High
**Description**: Update MirrorMoveAuto to dispatch to new MultiBlock variants
**Files modified**:
- `src/tests.rs` (lines 2823-2847) - Created mirror_move_auto_multi() dispatcher
- `src/runner.rs` (line 8) - Updated imports (removed mirror_move_auto, added mirror_move_auto_multi)
- `src/runner.rs` (line 723) - Updated registration to MultiBlock
**Result**: Auto-dispatches to 512_multi/256_multi/128_multi/base_multi based on CPU features

---

### 4. [PENDING] Run before/after comparison test
**Status**: Pending
**Priority**: Medium
**Description**: Compare old vs new MirrorMove implementations for correctness
**Purpose**: Validate that MultiBlock pattern produces same results as legacy per-block pattern

---

### 5. [PENDING] Delete old mirror_move functions
**Status**: Pending
**Priority**: Low (do after all migrations complete)
**Description**: Remove old per-block mirror_move functions once all variants migrated
**Files to clean**:
- `src/tests.rs` - Remove old mirror_move, mirror_move_128, mirror_move_256, mirror_move_512
- Only delete after all tests migrated and validated

---

### 6. [PENDING] Remove calculate_blocks_for_window() call at thread_pool.rs:306 and function definition
**Status**: Pending
**Priority**: Low (do after all migrations complete)
**Description**: Legacy window logic replaced by prepare_blocks_for_window() in tests.rs
**Location**:
- Call site: `thread_pool.rs:306`
- Function definition: `thread_pool.rs` (find with grep)
- Only remove after all tests migrated to MultiBlock

---

### 7. [PENDING] Remove duplicate window logging from thread_pool.rs (old path)
**Status**: Pending
**Priority**: Low (do after all migrations complete)
**Description**: Old path logs "Window X MB smaller than first block Y MB". New tests use prepare_blocks_for_window() logging in tests.rs
**Current duplicate logs**:
1. Old path (thread_pool.rs): "Window 64.0MB smaller than first block 4096.0MB - testing one complete block"
2. New path (tests.rs): "MirrorMove128: Prepared 1 block(s) for testing, total 4096.00 MiB (window: 64.00 MiB)"
**Action**: Remove old logging once all tests migrated

---

### 8. [PENDING] Migrate StuckBitTest (base scalar) to MultiBlock
**Status**: Pending
**Priority**: High
**Description**: Migrate base scalar StuckBitTest following MULTIBLOCK_MIGRATION.md
**Notes**: Has 3 phases (0xAAAA → 0x5555 → 0xAAAA), ensure all phases preserved

---

### 9. [PENDING] Migrate remaining tests to MultiBlock
**Status**: Pending
**Priority**: High
**Description**: Migrate all remaining tests to MultiBlock pattern
**Remaining tests**:
- StuckBitTest128/256/512 (SIMD variants)
- RefreshStable (base scalar)
- RefreshStable128/256/512 (SIMD variants)
- CacheBusting
- RandomAccessTorture
- StrideAccess
- BandwidthSaturation
- BlockMove

---

### 10. [PENDING] Clean up legacy test infrastructure
**Status**: Pending
**Priority**: Low (do after all migrations complete)
**Description**: Remove legacy per-block testing code paths
**Scope**:
- Legacy test dispatch logic in thread_pool.rs
- Old WithConfig test function wrappers
- Unused helper functions

---

### 11. [PENDING] Fix CTRL+C shutdown - not stopping properly mid-cycle/tests
**Status**: Pending
**Priority**: HIGH - User reported issue
**Description**: CTRL+C not stopping properly mid-cycle/tests anymore
**Root Cause**: Likely broken when runner.rs was nuked and rebuilt
**Investigation needed**:
- Check SHUTDOWN_REQUESTED signal propagation
- Verify signal handler is properly registered
- Test shutdown checkpoints in test loops
- Compare with old runner.rs backup to see what was lost

---

### 12. [PENDING] Add cycle count to per-thread report output
**Status**: Pending
**Priority**: Medium
**Description**: Per-thread reports should show how many cycles each thread completed
**Current**: Shows timing and bytes processed
**Needed**: Add "Cycles: X/Y" to per-thread output
**Files to modify**:
- `src/runner.rs` - Per-thread reporting logic
- Use `WorkResult.cycles_completed` and `WorkResult.cycle_limit` fields

---

### 13. [PENDING] Add clear messaging when tests bypassed due to missing CPU features
**Status**: Pending
**Priority**: Medium
**Description**: When CPU doesn't support AVX2/AVX-512, tests return zero stats silently
**Improvement**: Add clear log message: "MirrorMove512 bypassed - AVX-512 not available on this CPU"
**Affected tests**: All SIMD variants (128/256/512)

---

## Completed Items

### ✅ Fix simple_test_multi to dispatch to stream variants
**Completed**: Session before last summary
**Result**: simple_test_multi now correctly routes to stream1/2/4/N variants

### ✅ Create simple_test_stream1_multi (linear sequential)
**Completed**: Session before last summary

### ✅ Create simple_test_stream2_multi (2-way interleave)
**Completed**: Session before last summary

### ✅ Create simple_test_stream4_multi (4-way interleave)
**Completed**: Session before last summary

### ✅ Create simple_test_stream_n_multi (N-way interleave)
**Completed**: Session before last summary

### ✅ Add validation for streams and chunk divisibility
**Completed**: Session before last summary
**Result**: Streams validated at config time, chunk divisibility validated before cycle loop

### ✅ Optimize validation placement (avoid hot loop)
**Completed**: Session before last summary
**Result**: All validation moved outside cycle loop for optimal performance

### ✅ Upgrade window rounding log to WARN level
**Completed**: Session before last summary

### ✅ Fix --single-test param to not run excessive cycles
**Completed**: Session before last summary
**Result**: Changed from cycles=100 to cycles=1

### ✅ Fix window logic to test complete blocks only
**Completed**: Session before last summary
**Result**: Windows now add complete blocks until total >= window size, never split blocks

### ✅ Migrate MirrorMove128 (SSE2) to MultiBlock
**Completed**: Session before last summary
**Files modified**: tests.rs, runner.rs
**Result**: stream1_multi and stream_n_multi working, proper validation and progress tracking

### ✅ Migrate MirrorMove256 (AVX2) to MultiBlock
**Completed**: 2025-01-03
**Files modified**: tests.rs (lines 6273-6887), runner.rs (imports and registration)
**Issues fixed during migration**:
- Used `timing.cycles` instead of non-existent `timing.cycle_limit`
- Recalculate chunk size per-block (not once from first block)
- Validate chunk divisibility for ALL blocks before cycle loop
- Actually update progress tracker (not just check Some())
**Result**: Builds with zero warnings, ready for testing

### ✅ Migrate MirrorMove512 (AVX-512) to MultiBlock
**Completed**: 2025-11-04
**Files modified**: tests.rs (lines 6902-7577), runner.rs (line 8 imports, line 766 registration)
**Result**: Builds with zero warnings, test runs correctly, AVX-512 feature detection working

### ✅ Update MirrorMoveAuto resolver
**Completed**: 2025-11-04
**Files modified**: tests.rs (lines 2823-2847), runner.rs (line 8 imports, line 723 registration)
**Result**: Auto-dispatches to best available MultiBlock variant (512_multi > 256_multi > 128_multi > base_multi)

---

## Reference Files

- **MULTIBLOCK_MIGRATION.md** - Complete 24-step migration checklist
- **TODO.md** - Historical todo list (last updated August 2024, archived)
- **ACTIVE_TODO.md** - This file (current working list)

---

## Important Notes

1. **Always follow MULTIBLOCK_MIGRATION.md checklist** for each test migration
2. **Verify zero warnings** before marking migration complete
3. **Test with --single-test** before moving to next migration
4. **Update this file immediately** when adding or completing todo items
5. **CTRL+C issue is HIGH priority** - user reported functionality broken

---

## Session Recovery Information

**If session is lost**, resume by:
1. Reading this file (ACTIVE_TODO.md)
2. Reading MULTIBLOCK_MIGRATION.md for migration pattern
3. Checking current status with `cargo build --release`
4. Continuing with next pending item in priority order

**Last Known Good State**:
- MirrorMoveAuto resolver updated (2025-11-04)
- All MirrorMove variants migrated to MultiBlock (base, 128, 256, 512, Auto)
- Builds with zero warnings
- Ready for StuckBitTest migration or other pending tasks
