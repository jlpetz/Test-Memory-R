# Script to export ETL trace data with symbols loaded
param(
    [Parameter(Mandatory=$true)]
    [string]$EtlFile
)

if (!(Test-Path $EtlFile)) {
    Write-Host "ETL file not found: $EtlFile" -ForegroundColor Red
    exit 1
}

$xperf = "C:\Program Files (x86)\Windows Kits\10\Windows Performance Toolkit\xperf.exe"
$wpaExporter = "C:\Program Files (x86)\Windows Kits\10\Windows Performance Toolkit\wpaexporter.exe"

# Create export directory
$exportDir = "profiles\exports"
if (!(Test-Path $exportDir)) {
    New-Item -ItemType Directory -Path $exportDir | Out-Null
}

$baseName = [System.IO.Path]::GetFileNameWithoutExtension($EtlFile)
$timestamp = Get-Date -Format "yyyy-MM-dd_HH-mm-ss"

Write-Host "`nExporting ETL data with symbols from: $EtlFile" -ForegroundColor Cyan
Write-Host "==========================================" -ForegroundColor Cyan

# Set symbol path to include Microsoft symbol server and local paths
$env:_NT_SYMBOL_PATH = "srv*C:\Symbols*https://msdl.microsoft.com/download/symbols"
Write-Host "Symbol path set to: $env:_NT_SYMBOL_PATH" -ForegroundColor Gray

# Method 1: Export CPU sampling data with symbols using xperf
Write-Host "`n1. Loading symbols and extracting CPU sampling data..." -ForegroundColor Yellow
Write-Host "   This may take a few minutes on first run while downloading symbols..." -ForegroundColor Gray

$cpuFile = "$exportDir\${baseName}_cpu_analysis_symbols.txt"

# Use xperf with -symbols flag to load symbols and analyze CPU usage
& $xperf -i $EtlFile -symbols -o $cpuFile -a cpusample -detail 2>&1 | ForEach-Object {
    if ($_ -match "Loading symbols") {
        Write-Host "   Loading symbols..." -ForegroundColor Gray
    }
}

if (Test-Path $cpuFile) {
    Write-Host "   CPU analysis with symbols saved to: $cpuFile" -ForegroundColor Green
}

# Method 2: Get stack analysis with symbols
Write-Host "`n2. Analyzing call stacks with symbols..." -ForegroundColor Yellow
$stackFile = "$exportDir\${baseName}_stacks_symbols.txt"

# This gets detailed stack information with function names
& $xperf -i $EtlFile -symbols -a stacks -butterfly > $stackFile 2>$null

if (Test-Path $stackFile) {
    $fileSize = (Get-Item $stackFile).Length / 1KB
    Write-Host "   Stack analysis saved to: $stackFile ($([math]::Round($fileSize, 2)) KB)" -ForegroundColor Green
}

# Method 3: Get hot functions sorted by weight
Write-Host "`n3. Extracting hot functions by CPU weight..." -ForegroundColor Yellow
$hotFile = "$exportDir\${baseName}_hot_functions_symbols.txt"

# Use dumper to get function-level CPU usage
& $xperf -i $EtlFile -symbols -a dumper -provider "SampledProfile" | Out-File $hotFile 2>$null

# Also try CPU analysis with grouping
$cpuGroupFile = "$exportDir\${baseName}_cpu_by_function.txt"
& $xperf -i $EtlFile -symbols -a cpu -tree process -tree module -tree function > $cpuGroupFile 2>$null

if (Test-Path $cpuGroupFile) {
    Write-Host "   CPU by function saved to: $cpuGroupFile" -ForegroundColor Green
}

# Method 4: Extract TMR-specific functions
Write-Host "`n4. Searching for TMR-specific functions..." -ForegroundColor Yellow
$tmrFunctionsFile = "$exportDir\${baseName}_tmr_functions.txt"

$tmrAnalysis = @"
TMR Function Analysis
=====================
Generated: $timestamp
ETL File: $EtlFile

Searching for timing-related functions...

"@

# Search all generated files for our target functions
$targetFunctions = @(
    "should_continue",
    "TestLoop",
    "TestTiming", 
    "elapsed",
    "Instant::now",
    "check_duration",
    "test_memory",
    "mirror_move",
    "simple_test"
)

foreach ($func in $targetFunctions) {
    Write-Host "   Searching for: $func" -ForegroundColor Gray
    
    $found = $false
    $matches = @()
    
    # Search in all exported files
    Get-ChildItem $exportDir -Filter "${baseName}*symbols*.txt" | ForEach-Object {
        $content = Get-Content $_.FullName -Raw
        $lines = $content -split "`n" | Where-Object { $_ -match $func }
        if ($lines) {
            $found = $true
            $matches += $lines
        }
    }
    
    if ($found) {
        $tmrAnalysis += "`n=== $func ===" + "`n"
        $matches | Select-Object -First 10 | ForEach-Object {
            $tmrAnalysis += $_ + "`n"
        }
        Write-Host "     ✓ Found" -ForegroundColor Green
    } else {
        Write-Host "     ✗ Not found" -ForegroundColor Yellow
    }
}

$tmrAnalysis | Out-File $tmrFunctionsFile
Write-Host "   TMR function analysis saved to: $tmrFunctionsFile" -ForegroundColor Green

# Method 5: Try to get CPU percentage data
Write-Host "`n5. Calculating CPU percentages..." -ForegroundColor Yellow
$percentFile = "$exportDir\${baseName}_cpu_percentages.txt"

# Run action to get CPU usage percentages
$cpuOutput = & $xperf -i $EtlFile -symbols -a cpu 2>$null

if ($cpuOutput) {
    $percentAnalysis = @"
CPU Usage Percentages
====================
Generated: $timestamp

Top Functions by CPU Usage:
---------------------------

"@
    
    # Parse the output for percentages
    $cpuOutput | Where-Object { $_ -match '^\s*\d+\.\d+%.*tmr\.exe' } | 
        Select-Object -First 50 | 
        ForEach-Object { $percentAnalysis += $_ + "`n" }
    
    # Look specifically for our functions
    $percentAnalysis += "`n`nTiming Function Analysis:`n"
    $percentAnalysis += "-------------------------`n"
    
    $cpuOutput | Where-Object { 
        $_ -match "should_continue|TestLoop|TestTiming|elapsed|Instant::now" 
    } | ForEach-Object { 
        $percentAnalysis += $_ + "`n" 
    }
    
    $percentAnalysis | Out-File $percentFile
    Write-Host "   CPU percentages saved to: $percentFile" -ForegroundColor Green
}

# Create summary report
Write-Host "`n6. Creating analysis summary..." -ForegroundColor Yellow
$summaryFile = "$exportDir\${baseName}_analysis_summary.txt"

$summary = @"
ETL Analysis Summary with Symbols
==================================
Generated: $timestamp
ETL File: $EtlFile

Files Generated:
----------------
"@

Get-ChildItem $exportDir -Filter "$baseName*" | Where-Object { $_.Length -gt 0 } | ForEach-Object {
    $summary += "- $($_.Name) ($([math]::Round($_.Length / 1KB, 2)) KB)`n"
}

$summary += "`n`nQuick Analysis Guide:`n"
$summary += "--------------------`n"
$summary += "1. Check " + $baseName + "_cpu_percentages.txt for CPU usage percentages`n"
$summary += "2. Check " + $baseName + "_tmr_functions.txt for timing function matches`n"
$summary += "3. Check " + $baseName + "_cpu_by_function.txt for function-level breakdown`n"

$summary += @"

What to Look For:
-----------------
GOOD (low overhead):
  • should_continue: < 0.1% CPU
  • TestLoop/TestTiming: < 0.1% CPU combined
  • Main CPU usage in memory test functions

BAD (needs optimization):
  • should_continue: > 1% CPU
  • Timing functions: > 0.5% CPU combined
  • elapsed/Instant::now appearing frequently

"@

# Try to extract key metrics if available
$metricsFound = $false
if (Test-Path $cpuGroupFile) {
    $cpuContent = Get-Content $cpuGroupFile -Raw
    $shouldContinue = $cpuContent | Select-String "should_continue.*?(\d+\.\d+)%" | ForEach-Object { $_.Matches[0].Groups[1].Value }
    if ($shouldContinue) {
        $summary += "`nDetected Metrics:`n"
        $summary += "-----------------`n"
        $summary += "should_continue CPU: $shouldContinue%`n"
        $metricsFound = $true
    }
}

if (-not $metricsFound) {
    $summary += "`nNote: Could not automatically extract CPU percentages.`n"
    $summary += "Please check the individual files for detailed analysis.`n"
}

$summary | Out-File $summaryFile
Write-Host "   Summary saved to: $summaryFile" -ForegroundColor Green

# Display results
Write-Host "`n=== Export Complete ===" -ForegroundColor Green
Write-Host "`nGenerated files in $exportDir`:" -ForegroundColor Cyan
Get-ChildItem $exportDir -Filter "$baseName*" | Where-Object { $_.Length -gt 0 } | ForEach-Object {
    $sizeKB = [math]::Round($_.Length / 1KB, 2)
    if ($sizeKB -gt 100) {
        Write-Host "  - $($_.Name) ($sizeKB KB)" -ForegroundColor White
    } elseif ($sizeKB -gt 10) {
        Write-Host "  - $($_.Name) ($sizeKB KB)" -ForegroundColor Gray
    } else {
        Write-Host "  - $($_.Name) ($sizeKB KB)" -ForegroundColor DarkGray
    }
}

Write-Host "`nKey files to share:" -ForegroundColor Yellow
Write-Host "  1. $baseName`_cpu_percentages.txt" -ForegroundColor White
Write-Host "  2. $baseName`_tmr_functions.txt" -ForegroundColor White
Write-Host "  3. $baseName`_cpu_by_function.txt" -ForegroundColor White

Write-Host "`nSymbol loading status:" -ForegroundColor Cyan
if (Test-Path "C:\Symbols") {
    $symbolCount = (Get-ChildItem "C:\Symbols" -Recurse -File).Count
    Write-Host "  Local symbol cache: $symbolCount files" -ForegroundColor Green
} else {
    Write-Host "  No local symbol cache found (will download on demand)" -ForegroundColor Yellow
}

Write-Host "`nNote: First run may be slow due to symbol downloading." -ForegroundColor Gray
Write-Host "Subsequent runs will use cached symbols and be much faster." -ForegroundColor Gray