# TODO 13. [TMR-APP] Runtime Pattern State Tracking (Dependent Test Support)

> Full record, moved verbatim out of `TODO.md` on 2026-09-28 (open item).
> The short entry in `TODO.md` holds the current status; this file holds the reasoning.

**Priority**: Medium
**Status**: In Progress (design finalized 2026-03-30)

#### Problem

Tests can run in **dependent mode** (skip_init=true), relying on a prior test's pattern.
This requires the verify test to reconstruct the exact same pattern — every input to the
pattern function must match. A `ctx.cycle` mismatch in `block_seed()` caused billions of
false errors in Bench-Verify (fixed with cycle=0 stable seeds, 2026-03-30).

Two kinds of dependency exist:
- **Pattern-agnostic**: MirrorMove — doesn't care what's in memory, just checks round-trip
- **Pattern-aware**: Bench-Verify — must reconstruct exact expected values to compare

#### TM5 Reference

TM5 seeds patterns from address only: `(addr >> 12) + (addr >> 28)`. No thread ID, no cycle.
Write-read cycle counter adds a tiny offset to address (`addr + dWriteReadCycleCounter`)
before hashing, producing slight seed variation per inner cycle. Write and verify within the
same inner cycle always use the identical seed (co-located in same function).

TM5 MirrorMove has NO init phase — operates on whatever data is in memory from the prior
test. Pattern-agnostic dependency is implicit in TM5's test ordering.

#### Design: Two-Layer Validation

**Layer 1 — Plan-time (existing)**: `validate_test_plan()` walks test definitions at startup,
tracks `current_pattern: Option<PatternId>` using `MemoryEffect`. Catches ordering errors
before any test runs (e.g., "Bench-Verify at position 3 has no matching Bench-Init").

**Layer 2 — Runtime (new)**: Coordinator in `execute_test_cycle()` tracks per-thread pattern
state. After each test, updates state based on `MemoryEffect`. Before dependent tests, passes
the stored state to the test via config. Authoritative check — has the actual resolved seeds.

#### Data Structures

```rust
/// What was actually written to a thread's memory. Stored by coordinator.
#[derive(Clone, Debug)]
pub struct ActivePattern {
    pub id: PatternId,              // mode + params (links to plan-time PatternId)
    pub seed: u64,                  // resolved block_seed(ptr, thread_id, 0)
    pub cache_line_bytes: usize,    // for mode 1/2 reconstruction
    pub initialized_bytes: usize,   // window size — how much was actually written
}

/// Per-thread runtime state tracked by the coordinator.
pub struct ThreadPatternState {
    pub active: Option<ActivePattern>,  // None = unknown/destroyed
}
```

Per-thread tracking is needed because:
- Each thread has a different `ptr` and `thread_id` → different resolved seed
- Allocation sizes can be uneven across threads (edge case but possible)
- Future: inter-thread data sharing would need per-thread awareness

#### Flow in execute_test_cycle()

```
for each test_def in test_definitions:
    if test_def.config.skip_init:
        for each thread:
            check thread_states[tid].active exists
            check PatternId matches
            check initialized_bytes >= this test's window
            if valid: set config.pattern_override = Some(active.seed)
            if invalid: warn, fall back to independent mode (skip_init=false)

    dispatch test to all threads

    after results collected:
        match test_def.memory_effect:
            Writes(id) → store ActivePattern per thread
            Preserves  → no change
            Destroys   → clear to None
```

#### Passing State to Tests (Option 1: Copy via Config)

Add `pattern_override: Option<ActivePattern>` to `TestMemoryConfig`. Coordinator sets this
before dispatching dependent tests. Test closures check `config.pattern_override` first;
if present, use the stored seed instead of recomputing from `block_seed()`.

This follows the existing pattern — all test params flow through config, low copy overhead
(~40 bytes), no shared references or synchronization needed.

#### Error Repair

When a dependent test detects errors and wants to re-init a corrupted chunk, the init_fn
closure (always provided, even when skip_init=true) uses `config.pattern_override.seed`
to write the correct pattern. Without this, init_fn would recompute from its own config
which might not match what was actually in memory.

Note: error repair is not yet implemented in the harness — errors currently accumulate.
This design ensures repair will work correctly when added.

#### Current State (Interim Fix)

Bench-Init/Bench-Verify use `cycle=0` in all `block_seed()` calls, making seeds stable
across test boundaries. This is correct for the current codebase. The full runtime tracking
described above is the robust long-term solution that handles edge cases (uneven allocations,
window size mismatches, future inter-thread sharing).
