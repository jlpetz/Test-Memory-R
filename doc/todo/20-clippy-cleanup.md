# TODO 20. [TMR-APP] Address Clippy Warnings

> Full record, moved verbatim out of `TODO_ARCHIVE.md` on 2026-09-28 (closed item).
> The short entry in `TODO_ARCHIVE.md` holds the current status; this file holds the reasoning.

**Status**: DONE (2026-06-01)

**Resolution**: `cargo clippy --all-targets` is now **zero warnings** (was 225 on lib,
plus several more in the bin/test targets the old count missed). Approach:

1. **Auto-fix pass** (`cargo clippy --fix`): collapsible_if, assign_op_pattern,
   unnecessary_map_or, unnecessary_sort_by, etc. → 225→136.
2. **`missing_safety_doc` (77 occurrences)**: created `src/test_fn_safety.md` and applied
   `#[doc = include_str!("test_fn_safety.md")]` to the test-fn macro templates
   (`auto_dispatch`, `simple_test_nt_impl`, the lat_v2/bandwidth macros) + standalone test
   fns. One shared safety contract, single line per site — no duplicated prose.
3. **`missing_transmute_annotations` (32)**: annotated the NT-store SIMD macro with
   `transmute::<$simd_type, $arch_type>`.
4. **`type_complexity` (5)**: added type aliases — `CpuStatRow`/`TestCpuStats` and
   `CpuAssignment` (runner/thread_pool), `DriverInitResult` (driver/interface),
   `ParsedRunConfig` (main).
5. **`too_many_arguments` (5)**: addressed individually, NOT blanket-allowed —
   - `allocate_with_page_type`: `allocator: &mut Self` → real `&mut self`, plus a
     `PageTypeAllocParams` struct for the invariant phase inputs.
   - `simple_test_v2_sequential`/`_strided`: bundled pattern params into
     `SimplePatternConfig` (zero perf cost — consumed in cold setup, not the hot loop;
     `Copy` struct is SROA'd back into registers).
   - `run_tests_with_layout_and_timing_filtered`: 13→7 args via `TestRunOverrides`.
   - `execute_test_cycle`: 10→2 args via `CycleContext` (built once, reused per cycle).
6. **Tail**: orphaned doc comments removed, `sort_by`→`sort_by_key`, `from_str`→
   `from_config_str` (avoids `FromStr` confusion), `ChunkCtx::is_empty`, `Default` impls,
   `..Default::default()` struct-init in tests, `manual_checked_ops`.

All 52 lib tests pass; release build clean. No hot-path code touched.
