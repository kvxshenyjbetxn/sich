# Self-test for release-lib.ps1. Run: pwsh scripts/release.tests.ps1
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'release-lib.ps1')

function Assert-Equal($Actual, $Expected, [string]$What) {
    if ($Actual -ne $Expected) { throw "FAIL $What : expected '$Expected', got '$Actual'" }
}
function Assert-Throws([scriptblock]$Block, [string]$What) {
    $threw = $false
    try { & $Block } catch { $threw = $true }
    if (-not $threw) { throw "FAIL $What : expected an exception" }
}

Assert-Equal (Compare-SemVer '0.0.1' '0.0.2') -1 'cmp lt'
Assert-Equal (Compare-SemVer '1.0.0' '1.0.0') 0 'cmp eq'
Assert-Equal (Compare-SemVer '0.10.0' '0.9.9') 1 'cmp numeric not lexical'
Assert-Equal (Compare-SemVer '2.0.0' '1.99.99') 1 'cmp major'
Assert-Throws { Compare-SemVer '1.0' '1.0.0' } 'cmp invalid'
Assert-Throws { ConvertTo-SemVerParts 'v1.0.0' } 'parse v prefix'

Assert-Equal (Get-BumpedVersion '0.0.9' 'patch') '0.0.10' 'bump patch'
Assert-Equal (Get-BumpedVersion '0.3.9' 'minor') '0.4.0' 'bump minor'
Assert-Equal (Get-BumpedVersion '1.2.3' 'major') '2.0.0' 'bump major'

$toml = "[workspace]`nmembers = [`"a`"]`n`n[package]`nversion = `"9.9.9`"`n`n[workspace.package]`nedition = `"2024`"`nversion = `"0.0.1`"`n`n[workspace.dependencies]`nversion = `"7`"`n"
Assert-Equal (Get-WorkspaceVersion $toml) '0.0.1' 'get version in section'
$new = Set-WorkspaceVersion $toml '0.1.0'
Assert-Equal (Get-WorkspaceVersion $new) '0.1.0' 'set version'
Assert-Equal ($new -replace '0\.1\.0', '0.0.1') $toml 'only version line changed'
if ($new -notmatch 'version = "9\.9\.9"') { throw 'FAIL other [package] version touched' }
if ($new -notmatch 'version = "7"') { throw 'FAIL dependencies version touched' }
$crlf = $toml -replace "`n", "`r`n"
$newCrlf = Set-WorkspaceVersion $crlf '0.2.0'
if ($newCrlf -notmatch "version = `"0\.2\.0`"`r`n") { throw 'FAIL CRLF not preserved' }
Assert-Throws { Set-WorkspaceVersion "[package]`nversion = `"1.0.0`"`n" '2.0.0' } 'no workspace.package'

Assert-Equal (Format-Sha256Line 'ABCDEF01' 'x.zip') 'abcdef01  x.zip' 'sha line'

$gen = @(Get-ReleaseNotesArgs '')
Assert-Equal $gen.Count 1 'generate-notes is one argument'
Assert-Equal $gen[0] '--generate-notes' 'generate-notes flag'
$own = @(Get-ReleaseNotesArgs 'Fix things')
Assert-Equal $own.Count 2 'notes are two arguments'
Assert-Equal ($own -join '|') '--notes|Fix things' 'notes flag and text'

Write-Host 'release.tests.ps1: all checks passed'
