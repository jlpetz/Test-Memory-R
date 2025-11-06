# Option D: Separate Init Phase - Reporting Mock-ups

## Thread Synchronization Requirements

```rust
// Pseudocode for Option D flow
fn run_test_cycle() {
    // === PHASE 1: INIT (all threads synchronized) ===
    barrier.wait();  // Wait for all threads to be ready

    let init_start = Instant::now();
    parallel_init();  // All threads init their windows
    barrier.wait();   // Wait for all threads to finish init

    let init_elapsed = init_start.elapsed();

    // === PHASE 2: TEST (all threads synchronized) ===
    barrier.wait();  // Ensure all ready to start test

    let test_start = Instant::now();
    for cycle in 0..cycles {
        parallel_test();  // All threads run test
    }
    barrier.wait();   // Wait for all to finish

    let test_elapsed = test_start.elapsed();

    // === PHASE 3: VERIFY (all threads synchronized) ===
    barrier.wait();

    let verify_start = Instant::now();
    parallel_verify();
    barrier.wait();

    let verify_elapsed = verify_start.elapsed();
}
```

**Why barriers?**: Ensures accurate timing and prevents fast threads from starting test phase while slow threads still initializing.

---

## Mock-up Option 1: Separate Init Table Before Test Report (RECOMMENDED)

```
🔧 Initialization Phase - MirrorMove512
   Pattern: Address-based (0x0123456789ABCDEF multiplier)
   Duration: 2.34s
--------------------------------------------------------------------------------------------------------------------------------------------------
    Thread       L CPU      P Core     NUMA           Time       Dev T        Data       Dev D         Speed       Dev D
--------------------------------------------------------------------------------------------------------------------------------------------------
         0           2           1      0             2.3s       -0.0s    5.00 GiB   +0.00 GiB  2175.3 MiB/s        +4.3
         1           3           1      0             2.3s       -0.0s    5.00 GiB   +0.00 GiB  2172.8 MiB/s        +1.8
         2           4           2      0             2.3s       +0.0s    5.00 GiB   +0.00 GiB  2168.9 MiB/s        -2.1
         3           5           2      0             2.3s       +0.0s    5.00 GiB   +0.00 GiB  2169.4 MiB/s        -1.6
         4           6           3      0             2.3s       -0.0s    5.00 GiB   +0.00 GiB  2173.5 MiB/s        +2.5
         5           7           3      0             2.3s       -0.0s    5.00 GiB   +0.00 GiB  2171.2 MiB/s        +0.2
       Avg                                            2.3s                5.00 GiB              2171.0 MiB/s
--------------------------------------------------------------------------------------------------------------------------------------------------

📊 Test Phase - Cycle 1 - MirrorMove512: 45.7s, 0 errors, 640.00 GiB @ 14310.5 MiB/s, 10 cycles
   Config: streams=1, window=FullAllocation, chunk=FixedSize { size_mb: 64 }
--------------------------------------------------------------------------------------------------------------------------------------------------
    Thread       L CPU      P Core     NUMA           Time       Dev T        Data       Dev D         Speed       Dev S      Cycles      Errors
--------------------------------------------------------------------------------------------------------------------------------------------------
         0           2           1      0            45.7s       +0.1s  106.67 GiB   +0.00 GiB 2386.7 MiB/s        +4.2          10           ✅
         1           3           1      0            45.6s       -0.0s  106.67 GiB   +0.00 GiB 2389.1 MiB/s        +6.6          10           ✅
         2           4           2      0            45.7s       +0.1s  106.67 GiB   +0.00 GiB 2385.4 MiB/s        +2.9          10           ✅
         3           5           2      0            45.7s       +0.1s  106.67 GiB   +0.00 GiB 2383.8 MiB/s        +1.3          10           ✅
         4           6           3      0            45.6s       -0.0s  106.67 GiB   +0.00 GiB 2390.5 MiB/s        +8.0          10           ✅
         5           7           3      0            45.6s       -0.0s  106.67 GiB   +0.00 GiB 2387.9 MiB/s        +5.4          10           ✅
       Avg                                           45.6s               106.67 GiB             2387.2 MiB/s                    10.0           ✅
--------------------------------------------------------------------------------------------------------------------------------------------------

✅ Verification Phase - MirrorMove512
   Pattern: Original address-based (round-trip validation)
   Duration: 2.15s, 0 errors found
--------------------------------------------------------------------------------------------------------------------------------------------------
    Thread       L CPU      P Core     NUMA           Time       Dev T        Data       Dev D         Speed       Dev D      Errors
--------------------------------------------------------------------------------------------------------------------------------------------------
         0           2           1      0             2.1s       -0.0s    5.00 GiB   +0.00 GiB  2380.1 MiB/s        +5.3           ✅
         1           3           1      0             2.1s       +0.0s    5.00 GiB   +0.00 GiB  2376.2 MiB/s        +1.4           ✅
         2           4           2      0             2.2s       +0.0s    5.00 GiB   +0.00 GiB  2373.9 MiB/s        -0.9           ✅
         3           5           2      0             2.2s       +0.0s    5.00 GiB   +0.00 GiB  2372.5 MiB/s        -2.3           ✅
         4           6           3      0             2.1s       -0.0s    5.00 GiB   +0.00 GiB  2378.8 MiB/s        +4.0           ✅
         5           7           3      0             2.1s       -0.0s    5.00 GiB   +0.00 GiB  2377.1 MiB/s        +2.3           ✅
       Avg                                            2.1s                5.00 GiB              2376.4 MiB/s                       ✅
--------------------------------------------------------------------------------------------------------------------------------------------------

Runtime: 00:00:50 | Total: Init 2.3s + Test 45.6s + Verify 2.1s | Completed | Test Throughput: 14.0 GiB/s (14311.0 MiB/s)
```

**Pros**:
- ✅ Clear separation of phases
- ✅ Each phase has its own timing/throughput
- ✅ Easy to compare init vs verify speeds
- ✅ Consistent with TM5 architecture
- ✅ Test throughput is pure (not skewed by init)

**Cons**:
- ❌ More vertical space
- ❌ Three separate tables per cycle

---

## Mock-up Option 2: Compact Init Summary (Minimal)

```
🔧 Init: 2.34s, 30.00 GiB written @ 2171.0 MiB/s

📊 Test report - Cycle 1 - MirrorMove512: 45.7s, 0 errors, 640.00 GiB @ 14310.5 MiB/s, 10 cycles
   Config: streams=1, window=FullAllocation, chunk=FixedSize { size_mb: 64 }
--------------------------------------------------------------------------------------------------------------------------------------------------
    Thread       L CPU      P Core     NUMA           Time       Dev T        Data       Dev D         Speed       Dev S      Cycles      Errors
--------------------------------------------------------------------------------------------------------------------------------------------------
         0           2           1      0            45.7s       +0.1s  106.67 GiB   +0.00 GiB 2386.7 MiB/s        +4.2          10           ✅
         1           3           1      0            45.6s       -0.0s  106.67 GiB   +0.00 GiB 2389.1 MiB/s        +6.6          10           ✅
         2           4           2      0            45.7s       +0.1s  106.67 GiB   +0.00 GiB 2385.4 MiB/s        +2.9          10           ✅
         3           5           2      0            45.7s       +0.1s  106.67 GiB   +0.00 GiB 2383.8 MiB/s        +1.3          10           ✅
         4           6           3      0            45.6s       -0.0s  106.67 GiB   +0.00 GiB 2390.5 MiB/s        +8.0          10           ✅
         5           7           3      0            45.6s       -0.0s  106.67 GiB   +0.00 GiB 2387.9 MiB/s        +5.4          10           ✅
       Avg                                           45.6s               106.67 GiB             2387.2 MiB/s                    10.0           ✅
--------------------------------------------------------------------------------------------------------------------------------------------------

✅ Verify: 2.15s, 30.00 GiB verified @ 2376.4 MiB/s, 0 errors

Runtime: 00:00:50 | Total: Init 2.3s + Test 45.6s + Verify 2.1s | Test Throughput: 14.0 GiB/s
```

**Pros**:
- ✅ Minimal vertical space
- ✅ Init/verify are single lines
- ✅ Still captures all data
- ✅ Test report unchanged (familiar format)

**Cons**:
- ❌ Can't see per-thread init/verify breakdown
- ❌ Loses thread-level detail for debugging

---

## Mock-up Option 3: Collapsible Detailed View

```
🔧 Init: 2.34s, 30.00 GiB @ 2171.0 MiB/s [+] Show details

📊 Test report - Cycle 1 - MirrorMove512: 45.7s, 0 errors, 640.00 GiB @ 14310.5 MiB/s, 10 cycles
--------------------------------------------------------------------------------------------------------------------------------------------------
    Thread       L CPU      P Core     NUMA           Time       Dev T        Data       Dev D         Speed       Dev S      Cycles      Errors
--------------------------------------------------------------------------------------------------------------------------------------------------
         [... test data ...]
--------------------------------------------------------------------------------------------------------------------------------------------------

✅ Verify: 2.15s, 30.00 GiB @ 2376.4 MiB/s, 0 errors [+] Show details
```

**With `--verbose` or interactive mode**:
```
🔧 Initialization Phase [expanded]
    Thread 0: 2.3s, 5.00 GiB @ 2175.3 MiB/s
    Thread 1: 2.3s, 5.00 GiB @ 2172.8 MiB/s
    ...
```

**Pros**:
- ✅ Compact by default
- ✅ Details available when needed
- ✅ Best of both worlds

**Cons**:
- ❌ Requires UI state management
- ❌ More complex implementation

---

## Mock-up Option 4: Final Summary Enhancement (Shows All Phases)

Keep per-cycle reporting as compact (Option 2), but enhance **Final Test Summary** to show all phases:

```
================================================================================
📊 Final Test Summary - Per-Test Performance (20 cycles)
----------------------------------------------------------------------------------------------------------------------------------
    #       Test Name           Init      Test      Verify     Total      Test Data            Test Throughput    Errors
----------------------------------------------------------------------------------------------------------------------------------
    1       MirrorMove512       2.3s     45.7s       2.1s     50.1s    640.00 GiB    14310.5 MiB/s (13.98 GiB/s)      ✅
                              30.0 GiB  640.0 GiB   30.0 GiB  700.0 GiB
                           2171.0 MiB/s            2376.4 MiB/s

    2       StuckBitTest512     1.8s     28.3s       1.7s     31.8s    480.00 GiB    17322.5 MiB/s (16.92 GiB/s)      ✅
                              30.0 GiB  480.0 GiB   30.0 GiB  540.0 GiB
                           2654.3 MiB/s            2812.1 MiB/s
----------------------------------------------------------------------------------------------------------------------------------
```

**Shows**:
- Init time + data + throughput
- Test time + data + throughput
- Verify time + data + throughput
- Total time + total data

**Pros**:
- ✅ Complete picture of all phases
- ✅ Easy to compare init/test/verify speeds across tests
- ✅ Per-cycle reports stay compact
- ✅ Final summary has all detail

**Cons**:
- ❌ Wide table
- ❌ May be information overload

---

## Mock-up Option 5: Side-by-Side Phases (Alternative Layout)

```
📊 Test Cycle 1 - MirrorMove512 - Runtime: 00:00:50
================================================================================
Phase            Duration    Data Processed         Throughput        Status
================================================================================
🔧 Init             2.34s      30.00 GiB     2171.0 MiB/s (2.12 GiB/s)    ✅
📊 Test            45.67s     640.00 GiB    14310.5 MiB/s (13.98 GiB/s)   ✅
✅ Verify           2.15s      30.00 GiB     2376.4 MiB/s (2.32 GiB/s)    ✅
================================================================================
Total              50.16s     700.00 GiB    14250.0 MiB/s (13.92 GiB/s)
================================================================================

📊 Test Phase - Per-Thread Breakdown
--------------------------------------------------------------------------------------------------------------------------------------------------
    Thread       L CPU      P Core     NUMA           Time       Dev T        Data       Dev D         Speed       Dev S      Cycles      Errors
--------------------------------------------------------------------------------------------------------------------------------------------------
         [... test data ...]
--------------------------------------------------------------------------------------------------------------------------------------------------

[Optional: Init/Verify per-thread breakdowns with --verbose flag]
```

**Pros**:
- ✅ Clean phase overview
- ✅ Test breakdown still detailed
- ✅ Easy to see phase times at a glance
- ✅ Total includes all phases

**Cons**:
- ❌ No per-thread init/verify detail by default

---

## CPU Performance Summary - How to Handle Init/Verify?

### Option A: Add Init/Verify Sections

```
=== CPU Performance Summary ===

🔧 Initialization Phase Performance
---------------------------------------------------------------------------------------------------------------------------------------
    Thread       L CPU      P Core     NUMA           Time       Dev T         Data       Dev D         Speed       Dev S
---------------------------------------------------------------------------------------------------------------------------------------
         0           2           1      0             2.3s       -0.0s      5.00 GiB   +0.00 GiB  2175.3 MiB/s        +4.3
         [...]
---------------------------------------------------------------------------------------------------------------------------------------

📊 Test Phase Performance by Thread
---------------------------------------------------------------------------------------------------------------------------------------
    Thread       L CPU      P Core     NUMA           Time       Dev T         Data       Dev D         Speed       Dev S      Errors
---------------------------------------------------------------------------------------------------------------------------------------
         0           2           1      0        04:32.316       -1.8s  2204.33 GiB  -14.28 GiB  8289.0 MiB/s        +0.3           ✅
         [...]
---------------------------------------------------------------------------------------------------------------------------------------

✅ Verification Phase Performance
---------------------------------------------------------------------------------------------------------------------------------------
    Thread       L CPU      P Core     NUMA           Time       Dev T         Data       Dev D         Speed       Dev D      Errors
---------------------------------------------------------------------------------------------------------------------------------------
         0           2           1      0             2.1s       -0.0s      5.00 GiB   +0.00 GiB  2380.1 MiB/s        +5.3           ✅
         [...]
---------------------------------------------------------------------------------------------------------------------------------------
```

### Option B: Aggregated Summary Only

```
=== CPU Performance Summary ===

📊 Test Phase Performance by Thread (Init: 2.3s, Verify: 2.1s, Test: 04:32.3s)
---------------------------------------------------------------------------------------------------------------------------------------
    Thread       L CPU      P Core     NUMA           Time       Dev T         Data       Dev D         Speed       Dev S      Errors
---------------------------------------------------------------------------------------------------------------------------------------
         0           2           1      0        04:32.316       -1.8s  2204.33 GiB  -14.28 GiB  8289.0 MiB/s        +0.3           ✅
         [...]
---------------------------------------------------------------------------------------------------------------------------------------
```

---

## Recommendation: **Mock-up Option 5 + Enhanced Final Summary**

**For per-cycle reporting**: Use Option 5 (Side-by-Side Phases)
- Compact phase summary at top
- Detailed test breakdown below
- Optional `--verbose` for init/verify per-thread

**For final summary**: Use Option 4 enhancement
- Show all three phases per test
- Easy to compare across tests

**Example flow**:

```
=== Cycle 1 ===

📊 Test Cycle 1 - MirrorMove512 - Runtime: 00:00:50
================================================================================
Phase            Duration    Data Processed         Throughput        Status
================================================================================
🔧 Init             2.34s      30.00 GiB     2171.0 MiB/s (2.12 GiB/s)    ✅
📊 Test            45.67s     640.00 GiB    14310.5 MiB/s (13.98 GiB/s)   ✅
✅ Verify           2.15s      30.00 GiB     2376.4 MiB/s (2.32 GiB/s)    ✅
================================================================================

📊 Test Phase - Per-Thread Breakdown
--------------------------------------------------------------------------------------------------------------------------------------------------
    Thread       L CPU      P Core     NUMA           Time       Dev T        Data       Dev D         Speed       Dev S      Cycles      Errors
--------------------------------------------------------------------------------------------------------------------------------------------------
         0           2           1      0            45.7s       +0.1s  106.67 GiB   +0.00 GiB 2386.7 MiB/s        +4.2          10           ✅
         [...]
--------------------------------------------------------------------------------------------------------------------------------------------------

=== Cycle 2 ===

📊 Test Cycle 2 - MirrorMove512 - Runtime: 00:00:45  (No init/verify - already initialized)
--------------------------------------------------------------------------------------------------------------------------------------------------
    Thread       L CPU      P Core     NUMA           Time       Dev T        Data       Dev D         Speed       Dev S      Cycles      Errors
--------------------------------------------------------------------------------------------------------------------------------------------------
         [...]
--------------------------------------------------------------------------------------------------------------------------------------------------

... (cycles 3-20 without init/verify)

=== Final Summary ===

================================================================================
📊 Final Test Summary - Per-Test Performance
----------------------------------------------------------------------------------------------------------------------------------
    #       Test Name           Init      Test      Verify     Total      Test Data            Test Throughput    Errors
----------------------------------------------------------------------------------------------------------------------------------
    1       MirrorMove512       2.3s    915.3s       2.1s    919.7s   12800.00 GiB   14310.5 MiB/s (13.98 GiB/s)      ✅
                              30.0 GiB 12800 GiB    30.0 GiB 12860 GiB
                           2171.0 MiB/s            2376.4 MiB/s
----------------------------------------------------------------------------------------------------------------------------------
```

**Key Features**:
1. **First cycle**: Shows init + test + verify
2. **Subsequent cycles**: Test only (no reinit)
3. **Final summary**: Aggregates all phases across all cycles
4. **Clean metrics**: Test throughput excludes init/verify

---

## Implementation Notes

### Thread Barriers

```rust
use std::sync::Barrier;

struct TestRunner {
    barrier: Arc<Barrier>,
    // ...
}

impl TestRunner {
    fn run_cycle(&mut self) {
        // === INIT PHASE ===
        self.barrier.wait();  // Sync point 1: Ready to init

        let init_stats = thread::scope(|s| {
            for thread_state in &mut self.threads {
                s.spawn(|| {
                    let start = Instant::now();
                    let bytes = test.init(&mut thread_state.memory);
                    (start.elapsed(), bytes)
                });
            }
        });

        self.barrier.wait();  // Sync point 2: Init complete

        // === TEST PHASE ===
        self.barrier.wait();  // Sync point 3: Ready to test

        let test_stats = thread::scope(|s| {
            // ... parallel test ...
        });

        self.barrier.wait();  // Sync point 4: Test complete

        // === VERIFY PHASE ===
        self.barrier.wait();  // Sync point 5: Ready to verify

        let verify_stats = thread::scope(|s| {
            // ... parallel verify ...
        });

        self.barrier.wait();  // Sync point 6: Verify complete
    }
}
```

### One-Time Init Handling

```rust
struct TestSuite {
    init_done: bool,
}

impl TestSuite {
    fn run_cycles(&mut self) {
        for cycle in 0..self.config.cycles {
            if cycle == 0 && !self.init_done {
                // First cycle: do init
                self.init_stats = self.run_init_phase();
                self.init_done = true;
            }

            // All cycles: do test
            self.test_stats[cycle] = self.run_test_phase();

            if cycle == self.config.cycles - 1 {
                // Last cycle: do verify
                self.verify_stats = self.run_verify_phase();
            }
        }
    }
}
```

---

## Summary

**Recommended Approach (Option D + Mock-up 5)**:

1. **Per-cycle**: Side-by-side phase summary + detailed test breakdown
2. **Final summary**: Enhanced table with all phases
3. **Init**: Once before all cycles (first cycle only)
4. **Verify**: Once after all cycles (last cycle only)
5. **Barriers**: Sync all threads between phases
6. **Metrics**: Test throughput is pure (excludes init/verify)

**Benefits**:
- ✅ Clean, TM5-compatible architecture
- ✅ Accurate per-phase metrics
- ✅ Test throughput not skewed by init
- ✅ Easy to debug (can see if init/verify is slow)
- ✅ Minimal vertical space in per-cycle reports
- ✅ Comprehensive final summary

**What do you think?** Would Mock-up Option 5 work for your needs? Or would you prefer one of the other layouts?
