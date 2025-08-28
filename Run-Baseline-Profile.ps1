# PowerShell script for baseline profiling with xperf
# Generates ETW trace data for analysis with WPA
# 
# Usage:
#   .\Run-Baseline-Profile-XPerf.ps1                    # Use our defaults
#   .\Run-Baseline-Profile-XPerf.ps1 memory=4GiB        # Override any parameter
#   .\Run-Baseline-Profile-XPerf.ps1 memory=4GiB config=test.json  # Multiple params

param(
    [switch]$OpenBrowser = $false,
    [Parameter(ValueFromRemainingArguments=$true)]
    [string[]]$TmrArgs  # All arguments to pass to TMR
)

Write-Host "`nTMR-APP Baseline Profiling (using xperf)" -ForegroundColor Cyan
Write-Host "==========================================" -ForegroundColor Cyan

# Create profiles directory
$profileDir = "profiles"
if (!(Test-Path $profileDir)) {
    New-Item -ItemType Directory -Path $profileDir | Out-Null
    Write-Host "Created profiles directory" -ForegroundColor Green
}

# Generate timestamp for unique filenames
$timestamp = Get-Date -Format "yyyy-MM-dd_HH-mm-ss"
$profileName = "baseline_$timestamp"

# Build with debug symbols
Write-Host "`nBuilding with debug symbols..." -ForegroundColor Yellow
cargo build --profile=release-with-debug
if ($LASTEXITCODE -ne 0) {
    Write-Host "Build failed!" -ForegroundColor Red
    exit 1
}

# Our default overrides
$ourDefaults = @(
    "duration=120",
    "cputype=cores",
    "--skip-cores=0"
)

# Start with user arguments or empty array
$testArgs = if ($TmrArgs) { $TmrArgs } else { @() }

# Check if user provided conflicting arguments and remove them
$testArgs = $testArgs | Where-Object { 
    -not ($_ -like "duration=*" -or 
          $_ -like "cputype=*" -or 
          $_ -like "--skip-cores=*")
}

# Add our defaults
$testArgs += $ourDefaults

Write-Host "`nTest Configuration:" -ForegroundColor Yellow
foreach ($arg in $testArgs) {
    Write-Host "  $arg" -ForegroundColor Gray
}

# Debug: Show current location and paths
Write-Host "`nPath Information:" -ForegroundColor Cyan
Write-Host "  Current Directory: $(Get-Location)" -ForegroundColor Gray
Write-Host "  Script Directory: $PSScriptRoot" -ForegroundColor Gray

# Get the full path to the executable - now in x86_64-pc-windows-msvc subdirectory
$tmrExe = Join-Path $PSScriptRoot "target\x86_64-pc-windows-msvc\release-with-debug\tmr.exe"
$tmrPdb = Join-Path $PSScriptRoot "target\x86_64-pc-windows-msvc\release-with-debug\tmr.pdb"

# Check fallback path if primary doesn't exist
if (!(Test-Path $tmrExe)) {
    $tmrExe = Join-Path $PSScriptRoot "target\release-with-debug\tmr.exe"
    $tmrPdb = Join-Path $PSScriptRoot "target\release-with-debug\tmr.pdb"
}

Write-Host "  Expected TMR path: $tmrExe" -ForegroundColor Gray

# Verify the executable exists
if (Test-Path $tmrExe) {
    $fileInfo = Get-Item $tmrExe
    Write-Host "  TMR Found: Yes" -ForegroundColor Green
    Write-Host "  TMR Size: $([math]::Round($fileInfo.Length / 1MB, 2)) MB" -ForegroundColor Gray
    Write-Host "  TMR Modified: $($fileInfo.LastWriteTime)" -ForegroundColor Gray
    
    # Check for PDB file
    if (Test-Path $tmrPdb) {
        $pdbInfo = Get-Item $tmrPdb
        Write-Host "  PDB Found: Yes" -ForegroundColor Green
        Write-Host "  PDB Size: $([math]::Round($pdbInfo.Length / 1MB, 2)) MB" -ForegroundColor Gray
    } else {
        Write-Host "  PDB Found: No (symbols may not be available)" -ForegroundColor Yellow
    }
} else {
    Write-Host "  TMR Found: No" -ForegroundColor Red
    Write-Host "`nSearching for tmr.exe in target directory..." -ForegroundColor Yellow
    Get-ChildItem -Path "target" -Filter "tmr.exe" -Recurse -ErrorAction SilentlyContinue | 
        Select-Object -First 5 | 
        ForEach-Object { 
            Write-Host "  Found: $($_.FullName)" -ForegroundColor Gray
            $tmrExe = $_.FullName
            $tmrPdb = $_.FullName -replace '\.exe$', '.pdb'
        }
    
    if (!(Test-Path $tmrExe)) {
        Write-Host "`nBuild may have failed or executable is in a different location." -ForegroundColor Red
        Write-Host "Run: cargo build --profile=release-with-debug" -ForegroundColor Yellow
        exit 1
    }
}

# Check for xperf (part of Windows Performance Toolkit)
Write-Host "`nChecking for xperf..." -ForegroundColor Cyan

$xperfPaths = @(
    "C:\Program Files (x86)\Windows Kits\10\Windows Performance Toolkit\xperf.exe",
    "C:\Program Files\Windows Kits\10\Windows Performance Toolkit\xperf.exe",
    "C:\Program Files (x86)\Windows Kits\8.1\Windows Performance Toolkit\xperf.exe",
    "C:\Program Files\Windows Kits\8.1\Windows Performance Toolkit\xperf.exe"
)

$xperfPath = $null
foreach ($path in $xperfPaths) {
    if (Test-Path $path) {
        $xperfPath = $path
        break
    }
}

# Try to find xperf in PATH if not in standard locations
if (-not $xperfPath) {
    $xperfInPath = Get-Command xperf -ErrorAction SilentlyContinue
    if ($xperfInPath) {
        $xperfPath = $xperfInPath.Path
    }
}

if ($xperfPath) {
    Write-Host "  xperf found: $xperfPath" -ForegroundColor Green
} else {
    Write-Host "  xperf not found!" -ForegroundColor Red
    Write-Host "  Please install Windows SDK with Windows Performance Toolkit component" -ForegroundColor Yellow
    Write-Host "  Download from: https://developer.microsoft.com/en-us/windows/downloads/windows-sdk/" -ForegroundColor Yellow
    exit 1
}

# Check if running as administrator (required for xperf)
$isAdmin = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole] "Administrator")

if (-not $isAdmin) {
    Write-Host "`nAdministrator privileges required for ETW tracing!" -ForegroundColor Red
    Write-Host "Please run this script as Administrator or use 'Run as administrator'" -ForegroundColor Yellow
    exit 1
}

Write-Host "`nStarting xperf profiling..." -ForegroundColor Cyan
Write-Host "  Executable: $tmrExe" -ForegroundColor Gray
Write-Host "  Arguments: $($testArgs -join ' ')" -ForegroundColor Gray

# ETW trace file
$etlFile = "$profileDir\${profileName}.etl"
$etlFileFull = (Resolve-Path $profileDir).Path + "\${profileName}.etl"
Write-Host "  ETW trace output: $etlFileFull" -ForegroundColor Gray

# Check if xperf is already running
Write-Host "`nChecking for existing ETW sessions..." -ForegroundColor Yellow
Write-Host "& $xperfPath -query"
$xperfQuery = & $xperfPath -query 2>&1

if ($xperfQuery -match "NT Kernel Logger" -or $xperfQuery -match "Logger Name") {
    if ($xperfQuery -notmatch "No Active") {
        Write-Host "WARNING: ETW trace sessions may already be running!" -ForegroundColor Yellow
        Write-Host "Attempting to stop any existing kernel logger sessions..." -ForegroundColor Yellow
		Write-Host "& $xperfPath -stop -d nul"
        & $xperfPath -stop -d nul 2>&1 | Out-Null
		Write-Host "& $xperfPath -stop "NT Kernel Logger" -d nul"
        & $xperfPath -stop "NT Kernel Logger" -d nul 2>&1 | Out-Null
        Start-Sleep -Seconds 2
    }
}

# Start xperf tracing with CPU profiling and stack walking
Write-Host "`nStarting ETW trace with xperf..." -ForegroundColor Green

# Build xperf command
$xperfProviders = "PROC_THREAD+LOADER+PROFILE+DISPATCHER"
$xperfStackWalk = "Profile"
$bufferSize = 1024
$maxFile = 2048

Write-Host "& $xperfPath -on $xperfProviders -stackwalk $xperfStackWalk -buffersize $bufferSize -MaxFile $maxFile -FileMode Circular"
& $xperfPath -on $xperfProviders -stackwalk $xperfStackWalk -buffersize $bufferSize -MaxFile $maxFile -FileMode Circular

if ($LASTEXITCODE -ne 0) {
    Write-Host "Failed to start ETW tracing!" -ForegroundColor Red
    Write-Host "Error code: $LASTEXITCODE" -ForegroundColor Red
    
    # Try to diagnose the issue
    Write-Host "`nTrying to diagnose..." -ForegroundColor Yellow
	Write-Host "& $xperfPath -query"
    & $xperfPath -query
    
    Write-Host "`nMake sure:" -ForegroundColor Yellow
    Write-Host "  1. You're running as Administrator" -ForegroundColor Gray
    Write-Host "  2. No other ETW sessions are running" -ForegroundColor Gray
    Write-Host "  3. Try running: xperf -stop" -ForegroundColor Gray
    exit 1
}

Write-Host "ETW tracing started successfully" -ForegroundColor Green

# Set up signal handling for Ctrl+C
$etlFileFullScript = $etlFileFull
$xperfPathScript = $xperfPath

# Create a scriptblock for cleanup
$cleanupScript = {
    param($etlFile, $xperfPath)
    Write-Host "`nCtrl+C detected - stopping ETW trace and saving..." -ForegroundColor Yellow
    Write-Host "Flushing buffers before stopping..." -ForegroundColor Gray
    Start-Sleep -Seconds 2  # Give time for buffers to flush
    Write-Host "Running: xperf -d $etlFile" -ForegroundColor Gray
	Write-Host "& $xperfPath -d $etlFile"
    & $xperfPath -d $etlFile
    if ($LASTEXITCODE -eq 0) {
        Write-Host "ETW trace saved to: $etlFile" -ForegroundColor Green
    } else {
        Write-Host "Failed to save ETW trace!" -ForegroundColor Red
    }
    exit 0
}

# Register the cleanup handler
Register-EngineEvent -SourceIdentifier PowerShell.Exiting -Action { 
    & $cleanupScript $etlFileFullScript $xperfPathScript 
}

try {
    # Run TMR with profiling
    Write-Host "`nRunning TMR (this will take up to 120 seconds)..." -ForegroundColor Cyan
    Write-Host "Press Ctrl+C to stop early (trace will be saved automatically)" -ForegroundColor Gray
    
    & $tmrExe $testArgs
    
    # If TMR completed normally, stop xperf with proper flushing
    Write-Host "`nTMR completed - stopping ETW trace and saving..." -ForegroundColor Green
    Write-Host "Flushing buffers and finalizing trace..." -ForegroundColor Gray
    Start-Sleep -Seconds 2  # Give time for buffers to flush
    
    Write-Host "Running: xperf -d $etlFileFull" -ForegroundColor Gray
    & $xperfPath -d $etlFileFull
    
    if ($LASTEXITCODE -ne 0) {
        Write-Host "ETW trace save failed!" -ForegroundColor Red
        Write-Host "Error code: $LASTEXITCODE" -ForegroundColor Red
        
        # Try alternative stop command
        Write-Host "Trying alternative stop command..." -ForegroundColor Yellow
		Write-Host "& $xperfPath -stop -d $etlFileFull"
        & $xperfPath -stop -d $etlFileFull
    }
} catch {
    # Handle any other errors during TMR execution
    Write-Host "`nError during TMR execution - stopping ETW trace..." -ForegroundColor Yellow
    & $xperfPath -d $etlFileFull
    throw
} finally {
    # Ensure we always try to stop xperf
    # Check if xperf is still running first
	Write-Host "& $xperfPath -query"
    $xperfQuery = & $xperfPath -query 2>&1
    if ($xperfQuery -notmatch "No Active") {
        Write-Host "`nCleaning up ETW trace..." -ForegroundColor Yellow
		Write-Host "& $xperfPath -stop -d nul"
        & $xperfPath -stop -d nul 2>&1 | Out-Null
    }
}

if (Test-Path $etlFileFull) {
    $etlFileInfo = Get-Item $etlFileFull
    Write-Host "`nETW trace saved successfully!" -ForegroundColor Green
    Write-Host "  File: $etlFileFull" -ForegroundColor White
    Write-Host "  Size: $([math]::Round($etlFileInfo.Length / 1MB, 2)) MB" -ForegroundColor Gray
} else {
    Write-Host "`nWarning: ETL file may not have been created properly" -ForegroundColor Yellow
}

# Also run with detailed logging for text analysis
Write-Host "`nRunning again with detailed logging for text analysis..." -ForegroundColor Yellow
$env:RUST_LOG = "tmr=debug"
$logFile = "$profileDir\${profileName}_detailed.log"

& $tmrExe $testArgs 2>&1 | Tee-Object -FilePath $logFile

# Extract timing information
Write-Host "`nExtracting timing data..." -ForegroundColor Yellow
$timingFile = "$profileDir\${profileName}_timing.txt"

@"
TMR Timing Analysis
===================
Generated: $timestamp
Configuration: $($testArgs -join ' ')

Hot Loop Functions:
-------------------
"@ | Out-File $timingFile

# Search for timing-related log entries
$patterns = @(
    "should_continue",
    "elapsed",
    "TestLoop",
    "TestTiming",
    "SuiteTiming",
    "cycle.*complete",
    "throughput"
)

foreach ($pattern in $patterns) {
    Add-Content $timingFile "`n=== Pattern: $pattern ==="
    Select-String -Path $logFile -Pattern $pattern | 
        Select-Object -First 20 | 
        ForEach-Object { $_.Line } |
        Out-File -Append $timingFile
}

# Create analysis summary
$summaryFile = "$profileDir\${profileName}_summary.md"
@"
# Profiling Summary (xperf)

**Date**: $timestamp  
**Configuration**: $($testArgs -join ' ')

## Files Generated

1. **${profileName}.etl** - Windows ETW trace file
   - Open with Windows Performance Analyzer (WPA)
   - Contains CPU sampling with stack traces
   - Shows kernel events, CPU usage, call stacks

2. **${profileName}_detailed.log** - Full execution log
   - Contains debug-level logging from TMR
   - Can be searched for timing patterns

3. **${profileName}_timing.txt** - Extracted timing data
   - Pre-filtered for relevant functions
   - Text format for quick analysis

## How to Analyze

### Opening in WPA:
1. Install Windows Performance Analyzer if not already installed
2. Open: **$etlFileFull**
3. **IMPORTANT**: Load symbols immediately:
   - Trace → Load Symbols (Ctrl+E)
   - Or: Trace → Configure Symbol Paths
   - Add the directory containing tmr.pdb
4. Add graphs from Graph Explorer:
   - CPU Usage (Sampled)
   - CPU Usage (Precise) if available
5. In the CPU graph table view:
   - Right-click columns → Show Column → Stack
   - Group by: Process Name → Stack
6. Filter to tmr.exe process
7. Look for these key functions:
   - should_continue
   - TestLoop
   - TestTiming
   - elapsed

### What to Look For:
- Functions taking > 1% of CPU time that aren't actual test work
- Overhead in timing/control functions
- Unexpected hotspots in the call stack

## xperf Trace Details

**Providers**: PROC_THREAD + LOADER + PROFILE + DISPATCHER
**Stack Walking**: Enabled for Profile events
**Buffer Size**: 1024 KB
**Max File Size**: 2048 MB
**Mode**: Circular buffer

## Expected Results

Good performance indicators:
- should_continue functions: < 0.1% of total time
- Throughput: > 10 GB/s for simple tests
- No single overhead function > 1% except actual memory operations
"@ | Out-File $summaryFile

Write-Host "`n=== Profiling Complete ===" -ForegroundColor Green
Write-Host "`nGenerated files:" -ForegroundColor Cyan
Write-Host "  ETW Trace: $etlFileFull" -ForegroundColor White
Write-Host "  Detailed Log: $logFile" -ForegroundColor White
Write-Host "  Timing Data: $timingFile" -ForegroundColor White
Write-Host "  Summary: $summaryFile" -ForegroundColor White

Write-Host "`nTo view the profile:" -ForegroundColor Yellow
Write-Host "  1. Install Windows Performance Analyzer (WPA) if not available" -ForegroundColor Gray
Write-Host "  2. Open: $etlFileFull in WPA" -ForegroundColor Gray
Write-Host "  3. CRITICAL: Load symbols with Trace → Load Symbols (Ctrl+E)" -ForegroundColor Red
Write-Host "     - Or use Trace → Configure Symbol Paths" -ForegroundColor Gray
Write-Host "     - Add the directory: $(Split-Path $tmrPdb)" -ForegroundColor Gray
Write-Host "  4. Add 'CPU Usage (Sampled)' graph from Graph Explorer" -ForegroundColor Gray
Write-Host "  5. In the table view, right-click → View Column → Stack" -ForegroundColor Gray
Write-Host "  6. Filter to tmr.exe process" -ForegroundColor Gray
Write-Host "  7. Search for these functions (Ctrl+F):" -ForegroundColor Gray
Write-Host "     - should_continue" -ForegroundColor Cyan
Write-Host "     - TestLoop" -ForegroundColor Cyan
Write-Host "     - TestTiming" -ForegroundColor Cyan
Write-Host "     - elapsed" -ForegroundColor Cyan
Write-Host "  8. Note the '% Weight' column for CPU usage percentage" -ForegroundColor Gray

if (Test-Path $tmrPdb) {
    Write-Host "`nPDB symbols detected at: $tmrPdb" -ForegroundColor Green
    Write-Host "Function names should be visible in WPA after loading symbols!" -ForegroundColor Green
} else {
    Write-Host "`nNo PDB found - rebuild with 'cargo build --profile=release-with-debug'" -ForegroundColor Yellow
}

Write-Host "`nTroubleshooting Tips:" -ForegroundColor Yellow
Write-Host "  - If WPA shows only addresses, symbols aren't loaded properly" -ForegroundColor Gray
Write-Host "  - Try manually adding symbol path: $(Split-Path $tmrPdb)" -ForegroundColor Gray
Write-Host "  - Ensure tmr.pdb is in the same directory as tmr.exe" -ForegroundColor Gray
Write-Host "  - If trace is empty, try running xperf -stop before re-running" -ForegroundColor Gray

# Open the profiles folder
Write-Host "`nOpening profiles folder..." -ForegroundColor Cyan
explorer $profileDir