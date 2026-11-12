# Export only the build variables selected by build_environment.ps1. Kept separate so the file-command
# behavior can be tested without loading Visual Studio or installing an SDK.
function Export-GitHubBuildEnvironment {
    param(
        [System.Collections.IDictionary]$Environment,
        [string]$EnvironmentFile,
        [string]$PathFile
    )
    if (-not $EnvironmentFile -or -not $PathFile) { throw '-GitHub requires a GitHub Actions job.' }
    $utf8 = New-Object Text.UTF8Encoding($false)
    foreach ($name in $Environment.Keys) {
        if ($name -eq 'Path') {
            # GITHUB_PATH is a series of prepends: the runner searches its last entry first.
            # Write backwards to preserve the selected PATH's first-match precedence, including
            # setup-python ahead of WindowsApps and MSVC ahead of Git's unrelated link.exe.
            $entries = @($Environment[$name].Split(';') | Where-Object { $_ })
            [Array]::Reverse($entries)
            foreach ($entry in $entries) {
                [IO.File]::AppendAllText($PathFile, "$entry`n", $utf8)
            }
        } else {
            [IO.File]::AppendAllText($EnvironmentFile, "$name=$($Environment[$name])`n", $utf8)
        }
    }
}
