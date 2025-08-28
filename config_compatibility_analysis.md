# TM5 vs TMR Configuration Compatibility Analysis

Based on analysis of actual TM5 config files and TMR implementation, here are all the differences that need to be addressed:

## 1. Test Sequence Execution ⚠️ **CRITICAL DIFFERENCE**

### TM5 Behavior:
```ini
Test Sequence=6,12,2,10,5,1,4,3,0,13,9,14,7,8,1,11,15
```
- **Executes tests in SPECIFIC ORDER** as defined in Test Sequence
- **Can repeat tests multiple times** (note "1" appears twice: positions 6 and 15)  
- **Test numbers reference [TestN] sections** (Test0, Test1, etc.)
- **Critical for memory stress progression** - specific test ordering may be important for error detection

### TMR Behavior:
```rust
// TMR executes ALL enabled tests in sequence order (0,1,2,3...)
self.test_sequence.iter().filter(|t| t.enabled)
```
- **Ignores TM5 Test Sequence completely**
- **Executes tests in definition order** (Test0, Test1, Test2...)
- **Cannot repeat individual tests**
- **May miss test ordering dependencies**

**Status**: ❌ **MAJOR INCOMPATIBILITY** - TMR doesn't follow TM5 test sequence

## 2. Time Percentage Per Test ⚠️ **TIMING DIFFERENCE**

### TM5 Behavior:
```ini
[Main Section]
Time (%)=100         # Global time multiplier

[Test1]
Time (%)=100         # Individual test time percentage
```
- **Global Time % controls overall test duration multiplier**
- **Individual Time % controls relative test duration within that multiplier**
- **Test duration = (Global Time % * Individual Time % * base_duration)**

### TMR Behavior:
```rust
// TMR uses direct cycles/duration, not percentage-based timing
TestTiming {
    cycles: test.cycles.or(self.system.timing.default_test_cycles),
    duration_secs: test.duration_secs.or(self.system.timing.default_test_duration_secs),
}
```
- **Uses absolute timing (cycles/seconds)**
- **No percentage-based timing system**
- **May not match TM5 test duration expectations**

**Status**: ⚠️ **TIMING INCOMPATIBILITY** - Different timing calculation methods

## 3. Test Block Size (Chunk Size) ✅ **HANDLED CORRECTLY**

### TM5 Behavior:
```ini
Test Block Size (Mb)=0    # Use default/auto size
Test Block Size (Mb)=16   # Use 16MB chunks
Test Block Size (Mb)=32   # Use 32MB chunks
```

### TMR Behavior:
```rust
chunk_mode: if test.test_chunk_size_mb > 0 {
    Some(ChunkMode::Fixed(test.test_chunk_size_mb * 1024 * 1024))
} else {
    None  // "window-size" mode doesn't need a value
}
```

**Status**: ✅ **COMPATIBLE** - TMR correctly handles Test Block Size

## 4. Global Memory Configuration 📝 **NEEDS DOCUMENTATION**

### TM5 Global Settings:
```ini
[Global Memory Setup]
Testing Window Size (Mb)=880
Lock Memory Granularity (Mb)=16
Reserved Memory for Windows (Mb)=128
Channels=2
Interleave Type=1
Single DIMM width, bits=64
Operation Block, byts=64
```

### TMR Equivalent:
```bash
# Command line arguments
tmr.exe memory=880MiB           # Testing Window Size
# No direct equivalents for other TM5 global settings
```

**Status**: ⚠️ **PARTIAL SUPPORT** - TMR doesn't use most TM5 global memory settings

## 5. Cycles vs Duration 📝 **DIFFERENT APPROACH**

### TM5 Behavior:
```ini
[Main Section]
Cycles=3              # Run entire test suite 3 times

[Test1] 
Time (%)=100          # Each test runs for percentage of allocated time
```
- **Cycles = number of complete test suite runs**
- **Time % controls individual test duration within each cycle**

### TMR Behavior:
```rust
pub struct TestSuiteTiming {
    pub global_cycles: Option<u32>,    // Max cycles for entire suite
    pub global_duration_secs: Option<u32>, // Max time for entire suite
}

pub struct TestTiming {
    pub cycles: Option<u32>,           // Max cycles for individual test
    pub duration_secs: Option<u32>,    // Max time for individual test
}
```
- **Individual test cycles/duration**
- **Global suite limits**
- **Different timing model than TM5**

**Status**: ⚠️ **DIFFERENT MODEL** - TMR uses individual test timing vs TM5's suite cycling

## 6. Function Name Mapping ✅ **HANDLED CORRECTLY**

### TM5 Functions:
- `SimpleTest`
- `MirrorMove` 
- `MirrorMove128`
- `RefreshStable`
- `BlockMove`

### TMR Functions:
```rust
fn map_function_name(legacy_name: &str) -> Result<String, String> {
    match legacy_name {
        "SimpleTest" => Ok("SimpleTest".to_string()),
        "MirrorMove" => Ok("MirrorMove128".to_string()),  // TM5 base -> TMR 128-bit
        "MirrorMove128" => Ok("MirrorMove128".to_string()),
        // ... other mappings
    }
}
```

**Status**: ✅ **COMPATIBLE** - Function names correctly mapped

## 7. Parameter Field Usage ❌ **ALGORITHM INCOMPATIBILITY**

**Status**: ❌ **MAJOR INCOMPATIBILITY** - Covered in detailed algorithm analysis

## Critical Issues Summary

### Must Fix:
1. **Test Sequence Execution** - TMR must follow TM5's Test Sequence order and repetition
2. **Time Percentage System** - TMR needs to implement TM5's percentage-based timing
3. **Parameter Algorithm Variants** - Cross-regional algorithms missing

### Should Document:
1. **Global Memory Settings** - TMR uses different memory allocation approach
2. **Timing Model Differences** - Individual vs suite-based timing

### Already Compatible:
1. **Test Block Size** - Correctly converted to chunk_mode
2. **Function Names** - Properly mapped
3. **Pattern Mode/Param0/Param1** - Exactly implemented

## Recommended Actions

### Phase 1: Critical Fixes
1. Implement TM5 Test Sequence execution order
2. Add TM5 time percentage calculation system
3. Implement cross-regional Parameter algorithms

### Phase 2: Documentation
1. Document global memory setting differences
2. Document timing model differences  
3. Create TM5→TMR conversion guide

### Phase 3: Validation
1. Run identical TM5 configs through both tools
2. Compare timing behavior
3. Compare test execution order and repetition
4. Validate error detection rates