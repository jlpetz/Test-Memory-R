# Script to export ETL trace data to text formats for analysis
param(
    [Parameter(Mandatory=$true)]
    [string]$EtlFile
)

if (!(Test-Path $EtlFile)) {
    Write-Host "ETL file not found: $EtlFile" -ForegroundColor Red
    exit 1
}

$wpaExporter = "C:\Program Files (x86)\Windows Kits\10\Windows Performance Toolkit\wpaexporter.exe"
$xperf = "C:\Program Files (x86)\Windows Kits\10\Windows Performance Toolkit\xperf.exe"

# Create export directory
$exportDir = "profiles\exports"
if (!(Test-Path $exportDir)) {
    New-Item -ItemType Directory -Path $exportDir | Out-Null
}

$baseName = [System.IO.Path]::GetFileNameWithoutExtension($EtlFile)
$timestamp = Get-Date -Format "yyyy-MM-dd_HH-mm-ss"

Write-Host "`nExporting ETL data from: $EtlFile" -ForegroundColor Cyan
Write-Host "=================================" -ForegroundColor Cyan

# Method 1: Use xperf to dump symbols and summary
Write-Host "`n1. Extracting summary information..." -ForegroundColor Yellow
$summaryFile = "$exportDir\${baseName}_summary.txt"

& $xperf -i $EtlFile -a sysconfig > $summaryFile 2>$null
& $xperf -i $EtlFile -a tracestats >> $summaryFile 2>$null

Write-Host "   Summary saved to: $summaryFile" -ForegroundColor Green

# Method 2: Export CPU sampling data with xperf
Write-Host "`n2. Extracting CPU sampling data..." -ForegroundColor Yellow
$cpuFile = "$exportDir\${baseName}_cpu_samples.txt"

# This will show function names and sample counts
& $xperf -i $EtlFile -o $cpuFile -a dumper -provider "SampledProfile" 2>$null

# Also try to get a CPU usage summary
$cpuSummaryFile = "$exportDir\${baseName}_cpu_summary.txt"
& $xperf -i $EtlFile -a cpusample -detail > $cpuSummaryFile 2>$null

if (Test-Path $cpuSummaryFile) {
    Write-Host "   CPU samples saved to: $cpuSummaryFile" -ForegroundColor Green
}

# Method 3: Get top functions by CPU usage
Write-Host "`n3. Analyzing top CPU-consuming functions..." -ForegroundColor Yellow
$topFunctionsFile = "$exportDir\${baseName}_top_functions.txt"

# Run xperf analysis for hot functions
$output = & $xperf -i $EtlFile -symbols -a stacks -butterfly 2>$null

# Parse for TMR-specific functions
$hotFunctions = @"
TMR Hot Functions Analysis
==========================
Generated: $timestamp
ETL File: $EtlFile

Searching for key functions in the trace...

Functions of interest:
- should_continue
- TestLoop
- TestTiming  
- elapsed
- test_memory
- mirror_move
- simple_test

"@

# Search the output for our target functions
$targetFunctions = @(
    "should_continue",
    "TestLoop", 
    "TestTiming",
    "elapsed",
    "test_memory",
    "mirror_move",
    "simple_test",
    "check_duration",
    "Instant::now"
)

foreach ($func in $targetFunctions) {
    $matches = $output | Select-String -Pattern $func
    if ($matches) {
        $hotFunctions += "`n=== $func ===" + "`n"
        $matches | ForEach-Object { $hotFunctions += $_.Line + "`n" }
    }
}

$hotFunctions | Out-File $topFunctionsFile
Write-Host "   Top functions saved to: $topFunctionsFile" -ForegroundColor Green

# Method 4: Create a simple CSV of process/thread activity
Write-Host "`n4. Extracting process and thread data..." -ForegroundColor Yellow
$processFile = "$exportDir\${baseName}_processes.csv"

$processData = @"
"Process","PID","CPU Time (ms)","Context Switches"
"@

# Get basic process info
$tmrProcess = & $xperf -i $EtlFile -a process 2>$null | Select-String "tmr.exe"
if ($tmrProcess) {
    $processData += "`n" + ($tmrProcess -join "`n")
}

$processData | Out-File $processFile
Write-Host "   Process data saved to: $processFile" -ForegroundColor Green

# Method 5: Try WPA exporter if available (may not work without profiles)
if (Test-Path $wpaExporter) {
    Write-Host "`n5. Attempting WPA export..." -ForegroundColor Yellow
    $wpaOutput = "$exportDir\${baseName}_wpa_export.csv"
    
    # Try to export CPU usage table
    & $wpaExporter $EtlFile -exporters "CPU Usage (Sampled)" -outputfolder $exportDir 2>$null
    
    if ($LASTEXITCODE -eq 0) {
        Write-Host "   WPA export successful" -ForegroundColor Green
    } else {
        Write-Host "   WPA export not available (normal without custom profiles)" -ForegroundColor Gray
    }
}

# Create a combined readable report
Write-Host "`n6. Creating combined analysis report..." -ForegroundColor Yellow
$reportFile = "$exportDir\${baseName}_analysis_report.txt"

$report = @"
TMR Performance Analysis Report
===============================
Generated: $timestamp
ETL File: $EtlFile

## Quick Analysis Commands

To analyze in WPA GUI:
1. Open Windows Performance Analyzer
2. File -> Open -> $EtlFile
3. In Graph Explorer, expand "Computation"
4. Drag "CPU Usage (Sampled)" to the analysis pane
5. In the table, search (Ctrl+F) for these functions:
   - should_continue
   - TestLoop::should_continue
   - TestTiming::should_continue
   - elapsed
   
Look at the "Weight" column to see % of CPU time.

## What We're Looking For

Good performance (should_continue overhead is minimal):
- should_continue functions: < 0.1% total CPU
- Test functions (mirror_move, simple_test): > 90% CPU
- No excessive context switches

Poor performance (needs optimization):
- should_continue functions: > 1% CPU
- Frequent elapsed/timing calls: > 0.5% CPU
- High context switch rate

## Files Generated

1. Summary: ${baseName}_summary.txt
   - System configuration
   - Trace statistics
   
2. CPU Samples: ${baseName}_cpu_summary.txt
   - Raw CPU sampling data
   - Function addresses and counts

3. Top Functions: ${baseName}_top_functions.txt
   - Filtered for TMR-specific functions
   - Shows hot spots

4. Process Data: ${baseName}_processes.csv
   - Process and thread information
   - Context switch counts

"@

# Try to extract some key metrics if available
if (Test-Path $cpuSummaryFile) {
    $cpuContent = Get-Content $cpuSummaryFile -Raw
    $shouldContinueMatches = ($cpuContent | Select-String -Pattern "should_continue" -AllMatches).Matches.Count
    $report += "`n## Quick Metrics`n"
    $report += "- 'should_continue' occurrences in samples: $shouldContinueMatches`n"
}

$report | Out-File $reportFile
Write-Host "   Analysis report saved to: $reportFile" -ForegroundColor Green

# Display summary
Write-Host "`n=== Export Complete ===" -ForegroundColor Green
Write-Host "`nGenerated files in $exportDir`:" -ForegroundColor Cyan
Get-ChildItem $exportDir -Filter "${baseName}*" | ForEach-Object {
    Write-Host "  - $($_.Name) ($([math]::Round($_.Length / 1KB, 2)) KB)" -ForegroundColor White
}

Write-Host "`nTo share these results:" -ForegroundColor Yellow
Write-Host "1. Share the text files from: $exportDir" -ForegroundColor Gray
Write-Host "2. Especially important: ${baseName}_top_functions.txt" -ForegroundColor Gray
Write-Host "3. And: ${baseName}_analysis_report.txt" -ForegroundColor Gray

# Try to find specific function percentages
Write-Host "`nSearching for specific functions..." -ForegroundColor Cyan
foreach ($func in @("should_continue", "elapsed", "TestLoop", "TestTiming")) {
    Write-Host "  Searching for: $func" -ForegroundColor Gray
    $found = $false
    Get-ChildItem $exportDir -Filter "${baseName}*.txt" | ForEach-Object {
        $content = Get-Content $_.FullName -Raw
        if ($content -match $func) {
            $found = $true
        }
    }
    if ($found) {
        Write-Host "    ✓ Found in exports" -ForegroundColor Green
    } else {
        Write-Host "    ✗ Not found (may need manual WPA analysis)" -ForegroundColor Yellow
    }
}

Write-Host "`nNote: For exact CPU percentages, manual analysis in WPA GUI is most accurate" -ForegroundColor Cyan