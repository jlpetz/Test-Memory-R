<#
Select the same serviced Windows SDK locally and in CI. Installs nothing unless -InstallSdk is given.
-Output writes the selected environment for build_release.py; -GitHub exports it to subsequent job steps.
The SDK installer is Microsoft's September 2026 release, checked by its SHA-256 and Authenticode signature.
Only named build variables are exported; never dump the process environment (CI can contain credentials).
#>
param([string]$Output, [switch]$InstallSdk, [switch]$GitHub)
$ErrorActionPreference = 'Stop'
# A PowerShell 7 parent can pass its PSModulePath to Windows PowerShell. Load the host's own modules.
Import-Module (Join-Path $PSHOME 'Modules\Microsoft.PowerShell.Utility\Microsoft.PowerShell.Utility.psd1')
Import-Module (Join-Path $PSHOME 'Modules\Microsoft.PowerShell.Security\Microsoft.PowerShell.Security.psd1')
$sdk = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'windows_sdk.json') -Raw | ConvertFrom-Json
$kits = Get-ItemPropertyValue -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots' -Name KitsRoot10
$rc = Join-Path $kits "bin\$($sdk.directory_version)\x64\rc.exe"

function SdkReady {
    if (-not (Test-Path -LiteralPath $rc)) { return $false }
    $version = (Get-Item -LiteralPath $rc).VersionInfo.FileVersion.Split(' ')[0]
    return $version -eq $sdk.file_version -and
        (Test-Path -LiteralPath (Join-Path $kits "Lib\$($sdk.directory_version)\um\x64\kernel32.lib")) -and
        (Test-Path -LiteralPath (Join-Path $kits "Lib\$($sdk.directory_version)\ucrt\x64\ucrt.lib"))
}

if (-not (SdkReady)) {
    if (-not $InstallSdk) {
        throw "SDK $($sdk.file_version) is required. Run powershell -File tools\build_environment.ps1 -InstallSdk once, elevated."
    }
    $download = Join-Path ([IO.Path]::GetTempPath()) ("tmr-sdk-" + [guid]::NewGuid().ToString('N') + '.exe')
    Invoke-WebRequest -Uri $sdk.installer_url -OutFile $download
    if ((Get-FileHash -LiteralPath $download -Algorithm SHA256).Hash.ToLowerInvariant() -ne $sdk.installer_sha256) {
        throw 'The Windows SDK installer checksum does not match tools/windows_sdk.json.'
    }
    $signature = Get-AuthenticodeSignature -LiteralPath $download
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch '(^|, )O=Microsoft Corporation(,|$)') {
        throw 'The Windows SDK installer does not have a valid Microsoft signature.'
    }
    $installed = Start-Process -FilePath $download -ArgumentList '/quiet', '/norestart' -WindowStyle Hidden -Wait -PassThru
    if ($installed.ExitCode -notin 0, 3010 -or -not (SdkReady)) {
        throw "Windows SDK installation failed or installed a different version (exit $($installed.ExitCode))."
    }
}

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vs = (& $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -format json | ConvertFrom-Json)
if ($LASTEXITCODE -ne 0 -or -not $vs) { throw 'Visual Studio C++ build tools were not found.' }
$vs = @($vs)[0]
Import-Module (Join-Path $vs.installationPath 'Common7\Tools\Microsoft.VisualStudio.DevShell.dll')
Enter-VsDevShell -VsInstallPath $vs.installationPath -SkipAutomaticLocation `
    -DevCmdArguments "-arch=x64 -host_arch=x64 -winsdk=$($sdk.directory_version)" | Out-Null
if ($env:WindowsSDKVersion.TrimEnd('\') -ne $sdk.directory_version) { throw 'Visual Studio selected the wrong SDK.' }
$libPaths = @($env:LIB.Split(';') | Where-Object { $_ } | ForEach-Object { [IO.Path]::GetFullPath($_).TrimEnd('\') })
foreach ($part in 'um', 'ucrt') {
    $requiredLib = [IO.Path]::GetFullPath((Join-Path $kits "Lib\$($sdk.directory_version)\$part\x64")).TrimEnd('\')
    if ($requiredLib -notin $libPaths) { throw "The selected LIB path does not contain $requiredLib." }
}

$linker = (Get-Command link.exe -CommandType Application | Select-Object -First 1).Source
# Pin Cargo's linker to the one whose version is recorded, even when another VS installation exists. First match, not every match:
# Enter-VsDevShell prepends its own directory, so the first is this VS's; Get-Command's .Source on every match (e.g. Git for
# Windows' unrelated usr\bin\link.exe, also on PATH) is an array, which stringifies into both paths joined by a space.
$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = $linker
$names = @('Path', 'LIB', 'LIBPATH', 'INCLUDE', 'VSINSTALLDIR', 'VCINSTALLDIR', 'VCToolsInstallDir', 'VCToolsVersion',
    'WindowsSdkDir', 'WindowsSDKVersion', 'WindowsSdkBinPath', 'WindowsSdkVerBinPath', 'UniversalCRTSdkDir', 'UCRTVersion',
    'VSCMD_ARG_TGT_ARCH', 'VSCMD_ARG_HOST_ARCH', 'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER')
$selected = [ordered]@{}
foreach ($name in $names) {
    $value = [Environment]::GetEnvironmentVariable($name)
    if ($null -ne $value) { $selected[$name] = $value }
}

function FileRecord([string]$path) {
    $file = Get-Item -LiteralPath $path
    return [ordered]@{ path = $file.FullName; version = $file.VersionInfo.FileVersion;
        sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() }
}
$native = [ordered]@{
    visual_studio = [ordered]@{ version = $vs.installationVersion; path = $vs.installationPath }
    linker = (FileRecord $linker)
    msvc_version = $env:VCToolsVersion
    sdk = [ordered]@{ directory_version = $sdk.directory_version; file_version = $sdk.file_version; resource_compiler = (FileRecord $rc) }
    libraries = @(
        (FileRecord (Join-Path $env:VCToolsInstallDir 'lib\x64\delayimp.lib')),
        (FileRecord (Join-Path $env:VCToolsInstallDir 'lib\x64\msvcrt.lib')),
        (FileRecord (Join-Path $kits "Lib\$($sdk.directory_version)\um\x64\kernel32.lib")),
        (FileRecord (Join-Path $kits "Lib\$($sdk.directory_version)\ucrt\x64\ucrt.lib"))
    )
    library_search_paths = $libPaths
}
if ($Output) {
    $json = [ordered]@{ native = $native; environment = $selected } | ConvertTo-Json -Depth 8
    [IO.File]::WriteAllText($Output, $json, (New-Object Text.UTF8Encoding($false)))
}
if ($GitHub) {
    . (Join-Path $PSScriptRoot 'github_environment.ps1')
    Export-GitHubBuildEnvironment -Environment $selected -EnvironmentFile $env:GITHUB_ENV -PathFile $env:GITHUB_PATH
}
Write-Host "Build tools: MSVC $($env:VCToolsVersion), Windows SDK $($sdk.file_version) ($($sdk.directory_version))"
