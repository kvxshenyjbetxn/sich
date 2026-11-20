# Pure helper functions for release.ps1 (no side effects; unit-tested by release.tests.ps1).

function ConvertTo-SemVerParts([string]$Version) {
    if ($Version -notmatch '^(\d+)\.(\d+)\.(\d+)$') { throw "Invalid version '$Version' (expected x.y.z)" }
    return @([int]$Matches[1], [int]$Matches[2], [int]$Matches[3])
}

# Returns -1, 0 or 1 like a comparer.
function Compare-SemVer([string]$A, [string]$B) {
    $pa = ConvertTo-SemVerParts $A
    $pb = ConvertTo-SemVerParts $B
    for ($i = 0; $i -lt 3; $i++) {
        if ($pa[$i] -lt $pb[$i]) { return -1 }
        if ($pa[$i] -gt $pb[$i]) { return 1 }
    }
    return 0
}

function Get-BumpedVersion([string]$Current, [ValidateSet('patch', 'minor', 'major')][string]$Bump) {
    $p = ConvertTo-SemVerParts $Current
    switch ($Bump) {
        'major' { return "$($p[0] + 1).0.0" }
        'minor' { return "$($p[0]).$($p[1] + 1).0" }
        'patch' { return "$($p[0]).$($p[1]).$($p[2] + 1)" }
    }
}

# Reads the version line inside [workspace.package] from Cargo.toml text.
function Get-WorkspaceVersion([string]$Toml) {
    $inSection = $false
    foreach ($line in ($Toml -split "`r?`n")) {
        if ($line -match '^\s*\[(.+?)\]\s*$') { $inSection = ($Matches[1] -eq 'workspace.package'); continue }
        if ($inSection -and $line -match '^\s*version\s*=\s*"([^"]+)"') { return $Matches[1] }
    }
    throw 'No version found in [workspace.package]'
}

# Replaces only the version line inside [workspace.package]; preserves line endings.
function Set-WorkspaceVersion([string]$Toml, [string]$Version) {
    $inSection = $false
    $done = $false
    $parts = [regex]::Split($Toml, '(?<=\n)')
    for ($i = 0; $i -lt $parts.Length; $i++) {
        $line = $parts[$i]
        if ($line -match '^\s*\[(.+?)\]\s*$') { $inSection = ($Matches[1] -eq 'workspace.package'); continue }
        if ($inSection -and -not $done -and $line -match '^(\s*version\s*=\s*)"[^"]*"([^\r\n]*)(\r?\n?)$') {
            $parts[$i] = "$($Matches[1])`"$Version`"$($Matches[2])$($Matches[3])"
            $done = $true
        }
    }
    if (-not $done) { throw 'No version line found in [workspace.package]' }
    return ($parts -join '')
}

# sha256sum format line (without trailing newline).
function Format-Sha256Line([string]$Hash, [string]$FileName) {
    return "$($Hash.ToLowerInvariant())  $FileName"
}
