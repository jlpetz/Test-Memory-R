# Script to analyze ETL file in WPA and export specific data
param(
    [Parameter(Mandatory=$true)]
    [string]$EtlFile
)

if (!(Test-Path $EtlFile)) {
    Write-Host "ETL file not found: $EtlFile" -ForegroundColor Red
    exit 1
}

$wpa = "C:\Program Files (x86)\Windows Kits\10\Windows Performance Toolkit\wpa.exe"
if (!(Test-Path $wpa)) {
    Write-Host "WPA not found!" -ForegroundColor Red
    exit 1
}

Write-Host "`nOpening ETL file in Windows Performance Analyzer..." -ForegroundColor Cyan
Write-Host "ETL File: $EtlFile" -ForegroundColor Gray

Write-Host "`n=== MANUAL ANALYSIS STEPS ===" -ForegroundColor Yellow
Write-Host "WPA will now open. Please follow these steps:" -ForegroundColor Cyan

Write-Host "`n1. LOAD SYMBOLS:" -ForegroundColor Green
Write-Host "   - In WPA menu: Trace -> Load Symbols" -ForegroundColor White
Write-Host "   - Or press Ctrl+E" -ForegroundColor White
Write-Host "   - Wait for symbols to load (see status bar)" -ForegroundColor Gray

Write-Host "`n2. ADD CPU SAMPLING GRAPH:" -ForegroundColor Green
Write-Host "   - In left 'Graph Explorer' panel, expand 'Computation'" -ForegroundColor White
Write-Host "   - Double-click or drag 'CPU Usage (Sampled)' to the analysis area" -ForegroundColor White

Write-Host "`n3. CONFIGURE THE TABLE:" -ForegroundColor Green
Write-Host "   - In the CPU Usage table, right-click and select 'Column Chooser'" -ForegroundColor White
Write-Host "   - Make sure these columns are visible:" -ForegroundColor White
Write-Host "     • Process" -ForegroundColor Gray
Write-Host "     • Module" -ForegroundColor Gray
Write-Host "     • Function" -ForegroundColor Gray
Write-Host "     • Stack" -ForegroundColor Gray
Write-Host "     • Weight (ms)" -ForegroundColor Gray
Write-Host "     • % Weight" -ForegroundColor Gray

Write-Host "`n4. FILTER TO TMR:" -ForegroundColor Green
Write-Host "   - In the Process column, find and select 'tmr.exe'" -ForegroundColor White
Write-Host "   - Right-click -> 'Filter To Selection'" -ForegroundColor White

Write-Host "`n5. SEARCH FOR HOT FUNCTIONS:" -ForegroundColor Green
Write-Host "   - Press Ctrl+F to open search" -ForegroundColor White
Write-Host "   - Search for each of these functions:" -ForegroundColor White
Write-Host "     • should_continue" -ForegroundColor Yellow
Write-Host "     • TestLoop" -ForegroundColor Yellow
Write-Host "     • TestTiming" -ForegroundColor Yellow
Write-Host "     • elapsed" -ForegroundColor Yellow
Write-Host "     • Instant::now" -ForegroundColor Yellow
Write-Host "   - Note the '% Weight' column for each" -ForegroundColor White

Write-Host "`n6. EXPAND TOP FUNCTIONS:" -ForegroundColor Green
Write-Host "   - Sort by '% Weight' column (descending)" -ForegroundColor White
Write-Host "   - Expand the top 10-20 functions" -ForegroundColor White
Write-Host "   - Look for any timing/condition checking overhead" -ForegroundColor White

Write-Host "`n7. EXPORT THE DATA:" -ForegroundColor Green
Write-Host "   - Select all rows (Ctrl+A)" -ForegroundColor White
Write-Host "   - Copy (Ctrl+C)" -ForegroundColor White
Write-Host "   - Paste into a text file and save as:" -ForegroundColor White
Write-Host "     profiles\exports\wpa_cpu_analysis.txt" -ForegroundColor Yellow

Write-Host "`n=== WHAT TO LOOK FOR ===" -ForegroundColor Cyan

Write-Host "`nGOOD (low overhead):" -ForegroundColor Green
Write-Host "  • should_continue functions: < 0.1% total" -ForegroundColor White
Write-Host "  • Memory test functions: > 90% total" -ForegroundColor White
Write-Host "  • No timing functions in top 20" -ForegroundColor White

Write-Host "`nBAD (needs optimization):" -ForegroundColor Red
Write-Host "  • should_continue functions: > 1% total" -ForegroundColor White
Write-Host "  • elapsed/Instant::now: > 0.5% total" -ForegroundColor White
Write-Host "  • Timing checks in top functions" -ForegroundColor White

Write-Host "`n=== EXAMPLE OUTPUT FORMAT ===" -ForegroundColor Cyan
Write-Host @"
Process     Module      Function                        % Weight
-------     ------      --------                        --------
tmr.exe     tmr.exe     test_memory_block                 45.2%
tmr.exe     tmr.exe     mirror_move_test                  38.1%
tmr.exe     tmr.exe     simple_test                       12.3%
tmr.exe     ntdll.dll   memcpy                             3.8%
tmr.exe     tmr.exe     TestLoop::should_continue          0.05%  <- This is what we measure
tmr.exe     tmr.exe     TestTiming::should_continue        0.02%  <- And this
"@ -ForegroundColor Gray

# Create export directory if needed
$exportDir = "profiles\exports"
if (!(Test-Path $exportDir)) {
    New-Item -ItemType Directory -Path $exportDir | Out-Null
}

# Open WPA
Write-Host "`nOpening WPA now..." -ForegroundColor Green
Start-Process $wpa -ArgumentList $EtlFile

Write-Host "`nAfter you complete the analysis, save the exported data to:" -ForegroundColor Yellow
Write-Host "  $exportDir\wpa_cpu_analysis.txt" -ForegroundColor White
Write-Host "`nThen share that file for analysis." -ForegroundColor Cyan