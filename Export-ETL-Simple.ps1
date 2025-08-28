# Simple ETL export script with symbols
param(
    [Parameter(Mandatory=$true)]
    [string]$EtlFile
)

if (!(Test-Path $EtlFile)) {
    Write-Host "ETL file not found: $EtlFile" -ForegroundColor Red
    exit 1
}

$xperf = "C:\Program Files (x86)\Windows Kits\10\Windows Performance Toolkit\xperf.exe"

# Create export directory
$exportDir = "profiles\exports"
if (!(Test-Path $exportDir)) {
    New-Item -ItemType Directory -Path $exportDir | Out-Null
}

$baseName = [System.IO.Path]::GetFileNameWithoutExtension($EtlFile)
Write-Host "Exporting ETL data from: $EtlFile" -ForegroundColor Cyan

# Set symbol path
$env:_NT_SYMBOL_PATH = "srv*C:\Symbols*https://msdl.microsoft.com/download/symbols"
Write-Host "Symbol path configured" -ForegroundColor Gray

# Export CPU sampling profile data with symbols
Write-Host "Loading symbols and analyzing CPU profile..." -ForegroundColor Yellow
Write-Host "This may take a few minutes on first run..." -ForegroundColor Gray

# Use the 'profile' action which shows sampled CPU data
$outputFile = "$exportDir\$baseName-cpu-profile.txt"
& $xperf -i $EtlFile -symbols -a profile > $outputFile 2>&1

# Also try to get stack data
$stackFile = "$exportDir\$baseName-stacks.txt"
& $xperf -i $EtlFile -symbols -a stacks > $stackFile 2>&1

if (Test-Path $outputFile) {
    $size = [math]::Round((Get-Item $outputFile).Length / 1KB, 2)
    Write-Host "CPU analysis saved: $outputFile ($size KB)" -ForegroundColor Green
    
    # Search for our functions
    Write-Host "`nSearching for timing functions..." -ForegroundColor Yellow
    $content = Get-Content $outputFile -Raw
    
    $functions = @("should_continue", "TestLoop", "TestTiming", "elapsed")
    foreach ($func in $functions) {
        if ($content -match $func) {
            Write-Host "  Found: $func" -ForegroundColor Green
        } else {
            Write-Host "  Not found: $func" -ForegroundColor Gray
        }
    }
    
    Write-Host "`nFiles to share:" -ForegroundColor Cyan
    Write-Host "  1. $outputFile" -ForegroundColor White
    if (Test-Path $stackFile) {
        $stackSize = [math]::Round((Get-Item $stackFile).Length / 1KB, 2)
        Write-Host "  2. $stackFile ($stackSize KB)" -ForegroundColor White
    }
} else {
    Write-Host "Failed to generate analysis" -ForegroundColor Red
}