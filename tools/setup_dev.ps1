<#
.SYNOPSIS
    Sets up a Windows machine to build, test and check TMR as CI does (doc/dev_setup.md).

.DESCRIPTION
    Installs only what is missing and leaves what is there: Git, the GitHub CLI, Python 3.11+, the C++ build tools (the linker and the
    Windows SDK, from any Visual Studio 2022 or later with the C++ tools, else Build Tools 2026), rustup with the nightly (rustfmt,
    clippy) as the default toolchain, and the cargo tools CI uses (cargo-nextest, cargo-audit, cargo-about). System packages come from
    winget, the cargo tools from crates.io (--locked). -Check reports what is missing and changes nothing.

.PARAMETER Check
    Report only: list what is there and what is missing, install nothing. Exits 1 when something is missing.
.PARAMETER Window
    Also the Windows App Runtime 2, which the window needs to run (not to build).
.PARAMETER Installer
    Also Inno Setup 7.1.0, the version release.yml pins, to build dist\tmr.iss.
.PARAMETER Debugger
    Also WinDbg, for crash dumps.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File tools\setup_dev.ps1 -Check
.EXAMPLE
    powershell -ExecutionPolicy Bypass -File tools\setup_dev.ps1 -Window -Debugger
#>
param([switch]$Check, [switch]$Window, [switch]$Installer, [switch]$Debugger)

$ErrorActionPreference = 'Stop'
$missing = New-Object System.Collections.Generic.List[string]

function Report([string]$what, [bool]$there, [string]$detail) {
    $mark = if ($there) { 'ok     ' } else { 'MISSING' }
    Write-Host ("  {0}  {1}{2}" -f $mark, $what, $(if ($detail) { "  ($detail)" } else { '' }))
    if (-not $there) { $missing.Add($what) }
}

function Have([string]$command) { [bool](Get-Command $command -ErrorAction SilentlyContinue) }

function ToolReady([string]$command) {
    if (-not (Have $command)) { return $false }
    if ($command -ne 'python') { return $true }
    & python -c "import sys; sys.exit(sys.version_info < (3, 11))" *> $null
    return $LASTEXITCODE -eq 0
}

# A winget package installed, by its id: `winget list` exits non-zero when it finds none
function WingetHas([string]$id) {
    & winget list --id $id --exact --accept-source-agreements --disable-interactivity *> $null
    return $LASTEXITCODE -eq 0
}

function WingetInstall([string]$id, [string[]]$extra) {
    Write-Host "  installing $id"
    $wingetArgs = @('install', '--id', $id, '--exact', '--source', 'winget', '--accept-package-agreements', '--accept-source-agreements',
        '--disable-interactivity') + $extra
    & winget @wingetArgs
    if ($LASTEXITCODE -ne 0) { throw "winget couldn't install $id (exit $LASTEXITCODE)" }
}

# A new install's folders aren't on this session's PATH yet: add the usual ones
function RefreshPath {
    $machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $user = [Environment]::GetEnvironmentVariable('Path', 'User')
    $env:Path = "$machine;$user;$env:USERPROFILE\.cargo\bin"
}

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
function CppTools {
    if (-not (Test-Path $vswhere)) { return $null }
    $name = & $vswhere -products '*' -version '[17.0,' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property displayName
    return ($name | Select-Object -First 1)
}

if (-not (Have 'winget')) {
    throw "winget isn't here: install App Installer (Windows Server: see doc/dev_setup.md), or install the tools by hand from that list"
}

Write-Host 'TMR development machine'
Write-Host 'Needed:'

# Git, the GitHub CLI, Python: by command, so a copy installed another way counts
foreach ($tool in @(@('git', 'Git.Git'), @('gh', 'GitHub.cli'), @('python', 'Python.Python.3.13'))) {
    $command, $id = $tool
    $there = ToolReady $command
    if (-not $there -and -not $Check) { WingetInstall $id @(); RefreshPath; $there = ToolReady $command }
    Report $command $there $(if ($there) { (Get-Command $command).Source } else { "winget $id" })
}

# The linker and the Windows SDK: any Visual Studio 2022 or later with the C++ tools will do
$cpp = CppTools
if (-not $cpp -and -not $Check) {
    WingetInstall 'Microsoft.VisualStudio.BuildTools' @('--override',
        '--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended')
    $cpp = CppTools
}
Report 'C++ build tools' ([bool]$cpp) $(if ($cpp) { $cpp } else { 'winget Microsoft.VisualStudio.BuildTools, the C++ workload' })

# CI and local release builds select one serviced SDK; the C++ workload alone may install another.
$sdk = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'windows_sdk.json') -Raw | ConvertFrom-Json
$sdkRoot = Get-ItemPropertyValue -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots' -Name KitsRoot10 -ErrorAction SilentlyContinue
$sdkRc = if ($sdkRoot) { Join-Path $sdkRoot "bin\$($sdk.directory_version)\x64\rc.exe" } else { '' }
$sdkThere = $sdkRc -and (Test-Path -LiteralPath $sdkRc) -and
    (Get-Item -LiteralPath $sdkRc).VersionInfo.FileVersion.Split(' ')[0] -eq $sdk.file_version
if (-not $sdkThere -and -not $Check) {
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'build_environment.ps1') -InstallSdk
    if ($LASTEXITCODE -ne 0) { throw 'The matching Windows SDK could not be installed.' }
    $sdkThere = $true
}
Report 'Windows SDK' ([bool]$sdkThere) $sdk.file_version

# rustup and the nightly, the default toolchain: CLAUDE.md's commands call plain cargo, and the engine needs nightly features
if (-not (Have 'rustup') -and -not $Check) { WingetInstall 'Rustlang.Rustup' @(); RefreshPath }
$rustup = Have 'rustup'
Report 'rustup' $rustup ''
if ($rustup) {
    if (-not $Check) {
        & rustup toolchain install nightly --profile minimal --component rustfmt clippy
        & rustup default nightly
    }
    $components = (& rustup component list --installed --toolchain nightly 2>$null) -join ' '
    $default = (& rustup default 2>$null) -join ' '
    Report 'nightly (the default)' ($default -like 'nightly*') $default
    Report 'rustfmt, clippy' (($components -like '*rustfmt*') -and ($components -like '*clippy*')) ''
}

# The cargo tools CI installs, built from crates.io
foreach ($tool in @(@('nextest', 'cargo-nextest'), @('audit', 'cargo-audit'), @('about', 'cargo-about'))) {
    $sub, $crate = $tool
    $version = if ($rustup) { & cargo $sub --version 2>$null | Select-Object -First 1 } else { '' }
    if (-not $version -and $rustup -and -not $Check) {
        & cargo install --locked $crate
        $version = & cargo $sub --version 2>$null | Select-Object -First 1
    }
    Report $crate ([bool]$version) $version
}

if ($Window -or $Installer -or $Debugger) { Write-Host 'Asked for:' }
$optional = @()
if ($Window) { $optional += , @('Windows App Runtime 2', 'Microsoft.WindowsAppRuntime.2', @()) }
if ($Installer) { $optional += , @('Inno Setup 7 (release.yml pins 7.1.0)', 'JRSoftware.InnoSetup.7', @('--version', '7.1.0')) }
if ($Debugger) { $optional += , @('WinDbg', 'Microsoft.WinDbg', @()) }
foreach ($package in $optional) {
    $what, $id, $extra = $package
    $there = WingetHas $id
    if (-not $there -and -not $Check) { WingetInstall $id $extra; $there = WingetHas $id }
    Report $what $there "winget $id"
}

if ($missing.Count -gt 0) {
    Write-Host ("Missing: {0}" -f ($missing -join ', '))
    exit 1
}
Write-Host 'Ready. Next: python tools\ci_local.py (the checks CI runs); doc/dev_setup.md has the rest.'
