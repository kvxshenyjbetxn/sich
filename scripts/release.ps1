#requires -Version 7
<#
.SYNOPSIS
  Cut a Sich release to GitHub Releases (kvxshenyjbetxn/sich). See docs/releasing.md.
#>
[CmdletBinding()]
param(
    [string]$Version,
    [ValidateSet('patch', 'minor', 'major')][string]$Bump,
    [string]$Notes,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'release-lib.ps1')

function Invoke-Native([string]$Description, [scriptblock]$Block) {
    & $Block
    if ($LASTEXITCODE -ne 0) { throw "$Description failed (exit code $LASTEXITCODE)" }
}

$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $root
$cargoToml = Join-Path $root 'Cargo.toml'

# 1. Preconditions
if (-not $Version -and -not $Bump) { throw 'Specify -Version x.y.z or -Bump patch|minor|major' }
if ($Version -and $Bump) { throw 'Specify only one of -Version and -Bump' }
$current = Get-WorkspaceVersion (Get-Content $cargoToml -Raw)
if ($Bump) { $Version = Get-BumpedVersion $current $Bump }
ConvertTo-SemVerParts $Version | Out-Null
if ((Compare-SemVer $Version $current) -le 0) { throw "New version $Version must be greater than current $current" }

$tag = "v$Version"
$status = git status --porcelain
if ($LASTEXITCODE -ne 0) { throw 'git status failed' }
if ($status) { throw 'Working tree is not clean' }
$branch = (git rev-parse --abbrev-ref HEAD).Trim()
if ($LASTEXITCODE -ne 0) { throw 'git rev-parse failed' }
if ($branch -ne 'master') { throw "Must be on master (currently '$branch')" }
Invoke-Native 'gh auth status' { gh auth status }
if (git tag --list $tag) { throw "Tag $tag already exists locally" }
$remoteTag = git ls-remote --tags origin "refs/tags/$tag"
if ($LASTEXITCODE -ne 0) { throw 'git ls-remote failed' }
if ($remoteTag) { throw "Tag $tag already exists on origin" }

Write-Host "Releasing $current -> $Version$(if ($DryRun) { ' (dry run)' })"

$name = "sich-$Version-windows-x64"
$dist = Join-Path $root 'dist'
$stage = Join-Path $dist $name
$zip = Join-Path $dist "$name.zip"
$shaFile = "$zip.sha256"
$utf8 = [System.Text.UTF8Encoding]::new($false)

try {
    # 2. Version bump
    $toml = Get-Content $cargoToml -Raw
    [System.IO.File]::WriteAllText($cargoToml, (Set-WorkspaceVersion $toml $Version), $utf8)
    Invoke-Native 'cargo update' { cargo update --workspace }

    # 3. Checks and tests
    Invoke-Native 'cargo check' { cargo check --workspace --all-targets }
    Invoke-Native 'cargo test' { cargo test --workspace }

    # 4. Release build (SICH_RELEASE enables the updater)
    $env:SICH_RELEASE = '1'
    try { Invoke-Native 'cargo build --release' { cargo build --release } }
    finally { Remove-Item Env:SICH_RELEASE -ErrorAction SilentlyContinue }

    # 5. Stage + zip + sha256
    $files = 'sich.exe', 'sich-cli.exe', 'conpty.dll', 'OpenConsole.exe'
    foreach ($f in $files) {
        if (-not (Test-Path (Join-Path $root "target/release/$f"))) { throw "Missing build output: target/release/$f" }
    }
    if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
    New-Item -ItemType Directory -Path $stage -Force | Out-Null
    foreach ($f in $files) { Copy-Item (Join-Path $root "target/release/$f") $stage }
    if (Test-Path $zip) { Remove-Item $zip -Force }
    Compress-Archive -Path ($files | ForEach-Object { Join-Path $stage $_ }) -DestinationPath $zip
    $hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    $line = Format-Sha256Line $hash "$name.zip"
    [System.IO.File]::WriteAllText($shaFile, "$line`n", $utf8)

    # 6. Publish
    $url = '(dry run, not published)'
    if (-not $DryRun) {
        Invoke-Native 'git commit' { git commit -am "release v$Version" }
        Invoke-Native 'git tag' { git tag $tag }
        Invoke-Native 'git push master' { git push origin master }
        Invoke-Native 'git push tag' { git push origin $tag }
        $notesArgs = if ($Notes) { @('--notes', $Notes) } else { @('--generate-notes') }
        Invoke-Native 'gh release create' { gh release create $tag $zip $shaFile --title "Sich v$Version" @notesArgs }
        $url = (gh release view $tag --json url --jq .url | Out-String).Trim()
    }
}
finally {
    if ($DryRun) {
        git checkout -- Cargo.toml Cargo.lock
    }
}

# 7. Summary
$size = '{0:N1} MB' -f ((Get-Item $zip).Length / 1MB)
Write-Host ''
Write-Host "Version : $Version"
Write-Host "Zip     : $zip ($size)"
Write-Host "SHA256  : $hash"
Write-Host "Release : $url"
