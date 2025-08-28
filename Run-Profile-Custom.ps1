# PowerShell script for profiling with custom WPR profile
# This approach is more reliable and produces smaller, cleaner trace files

param(
    [Parameter(ValueFromRemainingArguments=$true)]
    [string[]]$TmrArgs  # All arguments to pass to TMR
)

Write-Host "`nTMR-APP Custom Profile Recording" -ForegroundColor Cyan
Write-Host "==================================" -ForegroundColor Cyan

# Check if running as administrator
$isAdmin = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole] "Administrator")
if (-not $isAdmin) {
    Write-Host "`nAdministrator privileges required for ETW tracing!" -ForegroundColor Red
    Write-Host "Please run this script as Administrator" -ForegroundColor Yellow
    exit 1
}

# Create profiles directory if needed
$profileDir = "profiles"
if (!(Test-Path $profileDir)) {
    New-Item -ItemType Directory -Path $profileDir | Out-Null
}

# Check for WPR
$wprPath = "C:\Program Files (x86)\Windows Kits\10\Windows Performance Toolkit\wpr.exe"
if (!(Test-Path $wprPath)) {
    Write-Host "Windows Performance Toolkit not found!" -ForegroundColor Red
    exit 1
}

# Check if WPR is already running
$wprStatus = & $wprPath -status 2>$null
if ($wprStatus -match "WPR recording is in progress") {
    Write-Host "ERROR: An ETW trace is already running!" -ForegroundColor Red
    Write-Host "Run 'wpr -cancel' to stop it first" -ForegroundColor Yellow
    exit 1
}

# Build with debug symbols
Write-Host "`nBuilding with debug symbols..." -ForegroundColor Yellow
cargo build --profile=release-with-debug
if ($LASTEXITCODE -ne 0) {
    Write-Host "Build failed!" -ForegroundColor Red
    exit 1
}

# Set up test arguments
$ourDefaults = @("duration=30", "cputype=cores", "--skip-cores=0")
$testArgs = if ($TmrArgs) { 
    $TmrArgs | Where-Object { 
        -not ($_ -like "duration=*" -or $_ -like "cputype=*" -or $_ -like "--skip-cores=*")
    }
} else { 
    @() 
}
$testArgs += $ourDefaults

# Generate filenames
$timestamp = Get-Date -Format "yyyy-MM-dd_HH-mm-ss"
$etlFile = "$profileDir\tmr_profile_$timestamp.etl"
$tmrExe = ".\target\release-with-debug\tmr.exe"

Write-Host "`nConfiguration:" -ForegroundColor Yellow
Write-Host "  TMR: $tmrExe" -ForegroundColor Gray
Write-Host "  Args: $($testArgs -join ' ')" -ForegroundColor Gray
Write-Host "  Output: $etlFile" -ForegroundColor Gray

# Start profiling with custom profile
Write-Host "`nStarting ETW trace with custom profile..." -ForegroundColor Green
& $wprPath -start "$profileDir\tmr-cpu-profile.wprp!CPUProfile" -filemode

if ($LASTEXITCODE -ne 0) {
    Write-Host "Failed to start tracing!" -ForegroundColor Red
    exit 1
}

Write-Host "ETW tracing started successfully" -ForegroundColor Green

# Set up cleanup handler
$global:etlFilePath = $etlFile
$global:wprExePath = $wprPath

$null = Register-EngineEvent -SourceIdentifier PowerShell.Exiting -Action {
    Write-Host "`nStopping ETW trace..." -ForegroundColor Yellow
    Start-Sleep -Seconds 1
    & $global:wprExePath -stop $global:etlFilePath
}

try {
    Write-Host "`nRunning TMR for 30 seconds..." -ForegroundColor Cyan
    Write-Host "Press Ctrl+C to stop early" -ForegroundColor Gray
    
    & $tmrExe $testArgs
    
    Write-Host "`nTMR completed" -ForegroundColor Green
} finally {
    Write-Host "`nStopping ETW trace..." -ForegroundColor Yellow
    Start-Sleep -Seconds 1  # Allow buffers to flush
    
    & $wprPath -stop $etlFile
    
    if (Test-Path $etlFile) {
        $fileInfo = Get-Item $etlFile
        Write-Host "`nTrace saved successfully!" -ForegroundColor Green
        Write-Host "  File: $etlFile" -ForegroundColor White
        Write-Host "  Size: $([math]::Round($fileInfo.Length / 1MB, 2)) MB" -ForegroundColor Gray
        
        # Try to open in WPA
        $wpaPath = $wprPath.Replace("wpr.exe", "wpa.exe")
        if (Test-Path $wpaPath) {
            Write-Host "`nOpening in Windows Performance Analyzer..." -ForegroundColor Cyan
            Start-Process $wpaPath -ArgumentList $etlFile
        } else {
            Write-Host "`nTo analyze: Open $etlFile in Windows Performance Analyzer" -ForegroundColor Yellow
        }
    } else {
        Write-Host "Failed to save trace file!" -ForegroundColor Red
    }
}

Write-Host "`nDone!" -ForegroundColor Green