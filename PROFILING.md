# Profiling TMR-APP with Windows Performance Toolkit

## Table of Contents
1. [Overview](#overview)
2. [Setup and Installation](#setup-and-installation)
3. [Build Configuration](#build-configuration)
4. [Running Profiling](#running-profiling)
5. [Analyzing Results](#analyzing-results)
6. [Optimal Release Builds](#optimal-release-builds)
7. [Performance Targets](#performance-targets)

## Overview

TMR-APP profiling uses **Windows Performance Toolkit (WPA/WPR/ETW)** - Microsoft's official profiling solution. This provides the most comprehensive and reliable profiling on Windows, with both visual analysis tools and exportable data.

### Why Windows Performance Toolkit?

- **Rock solid reliability** - Microsoft's own tooling, works perfectly on Windows
- **Most comprehensive data** - Kernel events, CPU usage, context switches, memory patterns
- **Both visual and text outputs** - WPA GUI for interactive analysis, CSV exports for automation
- **Zero code changes** - External profiling, no build modifications needed
- **Deep system insights** - Can see if `should_continue()` causes performance issues like branch mispredictions or context switches

### What We're Measuring

The goal is to measure overhead from condition checking in hot loops:
- **TestLoop::should_continue()** at `src/test_framework.rs:21-28`
- **TestTiming::should_continue()** at `src/tests.rs`
- **SuiteTiming::should_continue_suite()** at `src/runner.rs`

## Setup and Installation

### Requirements

1. **Windows Performance Toolkit** - Part of Windows SDK
2. **Administrator privileges** - Required for ETW kernel tracing
3. **Visual Studio** (if installed) - WPT is often included

### Installation

**Option 1: Via Visual Studio (Recommended)**
- If you have Visual Studio 2022, WPT is likely already installed
- Check: `C:\Program Files (x86)\Windows Kits\10\Windows Performance Toolkit\`

**Option 2: Standalone Installation**
1. Download Windows SDK: https://developer.microsoft.com/en-us/windows/downloads/windows-sdk/
2. During installation, select "Windows Performance Toolkit" component
3. Complete installation

**Verification:**
```powershell
# Check if WPR (Windows Performance Recorder) is available
Test-Path "C:\Program Files (x86)\Windows Kits\10\Windows Performance Toolkit\wpr.exe"
```

## Build Configuration

### Profile-Optimized Build
```toml
# Already configured in Cargo.toml
[profile.release-with-debug]
inherits = "release"
strip = false      # Keep debug symbols for profiling
debug = true       # Include debug information
```

### Build Commands
```powershell
# Build with debug symbols for profiling
cargo build --profile=release-with-debug

# Production build (no debug overhead)
cargo build --release
```

## Running Profiling

### Using the Profiling Script (Recommended)

```powershell
# Run as Administrator (required for ETW)
.\Run-Baseline-Profile.ps1

# With custom parameters
.\Run-Baseline-Profile.ps1 memory=4GiB cycles=200

# The script handles:
# - Building with debug symbols
# - Starting ETW trace
# - Running TMR with specified parameters
# - Stopping trace and exporting data
# - Generating analysis files
```

### Manual Profiling

```powershell
# 1. Build with symbols
cargo build --profile=release-with-debug

# 2. Start ETW recording (as Administrator)
wpr -start CPU

# 3. Run your test
.\target\release-with-debug\tmr.exe duration=120 cputype=cores --skip-cores=0

# 4. Stop and save trace
wpr -stop profile.etl

# 5. Open in Windows Performance Analyzer
wpa profile.etl
```

## Analyzing Results

### Files Generated

The profiling script creates several files:

1. **`baseline_YYYY-MM-DD_HH-mm-ss.etl`** - ETW trace file
   - Primary analysis file
   - Open with Windows Performance Analyzer (WPA)
   - Contains complete system profiling data

2. **`baseline_YYYY-MM-DD_HH-mm-ss_cpu_samples.csv`** - CPU sample data
   - Exported from ETL for programmatic analysis
   - Function names and CPU time percentages
   - Text format for automated processing

3. **`baseline_YYYY-MM-DD_HH-mm-ss_detailed.log`** - TMR execution log
   - Debug-level logging from TMR application
   - Timing information and performance metrics

4. **`baseline_YYYY-MM-DD_HH-mm-ss_timing.txt`** - Extracted timing patterns
   - Pre-filtered for timing-related functions
   - Easy to search and analyze

5. **`baseline_YYYY-MM-DD_HH-mm-ss_summary.md`** - Analysis guide
   - Instructions for viewing results
   - Expected performance indicators

### Visual Analysis in WPA

1. **Open the ETL file in Windows Performance Analyzer**
   ```powershell
   wpa profiles\baseline_YYYY-MM-DD_HH-mm-ss.etl
   ```

2. **Navigate to CPU Usage**
   - Look for "CPU Usage (Sampled)" in the Graph Explorer
   - Drag it to the analysis pane

3. **Configure the Analysis**
   - Group by: `Process Name` → `Thread ID` → `Stack`
   - Filter to your tmr.exe process

4. **Search for Hot Functions**
   - Use Ctrl+F to search for:
     - `should_continue`
     - `TestLoop`
     - `TestTiming`
     - `elapsed`

5. **Analyze the Results**
   - Look at `Weight (%)` column for CPU time percentage
   - Check `Count` for frequency of calls
   - Examine call stack to understand context

### Key Metrics to Check

| Function | Target | What to Look For |
|----------|--------|------------------|
| `should_continue` total | < 0.1% | Combined time in all condition checks |
| `elapsed()` calls | < 0.05% | Time spent getting current time |
| Call frequency | < 1M/sec | Avoid excessive checking |
| Branch prediction | > 99% | Predictable conditional branches |

### Text Analysis

**CSV Data Analysis:**
```powershell
# Search for should_continue functions in CSV
Select-String -Path "profiles\*_cpu_samples.csv" -Pattern "should_continue"

# Look for timing patterns
Select-String -Path "profiles\*_timing.txt" -Pattern "elapsed|throughput"
```

## Optimal Release Builds

### Development vs Production

```powershell
# Development build (with debug symbols for profiling)
cargo build --profile=release-with-debug

# Production build (maximum performance, no debug overhead)
cargo build --release
```

### Performance Comparison

| Profile | Debug Info | Binary Size | Performance | Use Case |
|---------|------------|-------------|-------------|----------|
| `release-with-debug` | Yes | ~8MB | ~99% of release | Profiling |
| `release` | No | ~1.5MB | Maximum | Production |

### Verification Commands

Ensure production builds have no profiling overhead:

```powershell
# Build production version
cargo build --release

# Check binary size
Get-Item .\target\release\tmr.exe | Select-Object Name, Length

# Verify symbols are stripped
dumpbin /symbols .\target\release\tmr.exe | Select-String "should_continue"
# Should return nothing if properly stripped

# Performance test
.\target\release\tmr.exe memory=1GiB cycles=10
```

## Performance Targets

### Expected Overhead

Well-optimized condition checking should show:
- **Total condition checking time**: < 0.1% of runtime
- **Individual check time**: 2-5ns per check
- **Branch prediction rate**: > 99%
- **Call frequency**: Reasonable (not millions per second)

### Performance Indicators

**Good Performance:**
```
CPU Usage Analysis:
├─ tmr.exe (100%)
   ├─ memory_test_functions (95%+)
   ├─ should_continue (< 0.1%)
   └─ other_overhead (< 5%)
```

**Poor Performance (needs optimization):**
```
CPU Usage Analysis:
├─ tmr.exe (100%)
   ├─ memory_test_functions (90%)
   ├─ should_continue (> 1%)    ← Problem!
   └─ elapsed_time_calls (> 1%) ← Problem!
```

## Troubleshooting

### Common Issues

1. **"Administrator privileges required"**
   - Right-click PowerShell → "Run as administrator"
   - ETW tracing requires kernel access

2. **"Windows Performance Toolkit not found"**
   - Install Windows SDK with WPT component
   - Check Visual Studio installation includes WPT

3. **Large ETL files**
   - Normal - ETL files can be 100MB+ for 2-minute runs
   - Use shorter duration tests if disk space is limited

4. **WPA won't open ETL**
   - Ensure WPA version matches Windows version
   - Try opening WPA first, then File → Open

### Performance Analysis Checklist

- [ ] `should_continue` functions show < 0.1% CPU time
- [ ] No excessive `elapsed()` call overhead
- [ ] Memory test functions dominate CPU usage
- [ ] No unexpected context switches or kernel time
- [ ] Branch prediction rates are good (WPA can show this)

## Quick Reference

### Workflow
1. Run: `.\Run-Baseline-Profile.ps1` (as Administrator)
2. Open generated `.etl` file in WPA
3. Analyze CPU usage by function
4. Look for `should_continue` overhead
5. Implement signal-based optimization if needed
6. Re-profile to verify improvement

### Key Files
- **Build script**: `Run-Baseline-Profile.ps1`
- **Documentation**: This file (`PROFILING.md`)
- **Results**: `profiles\baseline_*.etl` (open in WPA)
- **Text data**: `profiles\baseline_*_cpu_samples.csv`