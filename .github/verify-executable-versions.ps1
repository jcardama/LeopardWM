#Requires -Version 5.1
# Usage: pwsh .github/verify-executable-versions.ps1 [-RepoRoot <path>] [-Targets <string[]>]
param(
    [string]$RepoRoot = $env:GITHUB_WORKSPACE,
    [string[]]$Targets = @()
)

$ErrorActionPreference = "Stop"

if (-not $RepoRoot) {
    $RepoRoot = Split-Path -Parent $PSScriptRoot
}
$RepoRoot = (Resolve-Path -LiteralPath $RepoRoot).Path

$manifest = [IO.File]::ReadAllText((Join-Path $RepoRoot "Cargo.toml"))
$package = [regex]::Match($manifest, '(?ms)^\[workspace\.package\][ \t]*\r?\n(?<package>.*?)(?=^\[|\z)')
$version = [regex]::Match($package.Groups['package'].Value, '(?m)^[ \t]*version[ \t]*=[ \t]*"(?<version>[^"]+)"')
if (-not $version.Success) {
    throw "Cargo.toml under $RepoRoot has no [workspace.package] version"
}
$expectedVersion = $version.Groups['version'].Value

if ($Targets.Count -eq 0) {
    $candidates = @("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc")
    $Targets = $candidates | Where-Object { Test-Path (Join-Path $RepoRoot "target/$_/release/leopardwm.exe") }
    if ($Targets.Count -eq 0) {
        if (Test-Path (Join-Path $RepoRoot "target/release/leopardwm.exe")) {
            $Targets = @("")
        } else {
            throw "No built release executables found under $RepoRoot; build with 'cargo build --release' first"
        }
    }
}

foreach ($target in $Targets) {
    $targetDir = if ([string]::IsNullOrEmpty($target)) { "target/release" } else { "target/$target/release" }
    $paths = @(
        "$targetDir/leopardwm.exe",
        "$targetDir/leopardwm-watchdog.exe",
        "$targetDir/leopardwm-cli.exe",
        "$targetDir/lwm.exe"
    )

    foreach ($relative in $paths) {
        $path = Join-Path $RepoRoot $relative
        if (-not (Test-Path -LiteralPath $path)) {
            throw "$relative not found under $RepoRoot; build with 'cargo build --release' first"
        }
        $info = (Get-Item -LiteralPath $path).VersionInfo
        if ([string]::IsNullOrWhiteSpace($info.ProductVersion) -or [string]::IsNullOrWhiteSpace($info.FileVersion)) {
            throw "$relative is unversioned; expected ProductVersion and fixed FileVersion $expectedVersion"
        }
        if ($info.ProductVersion -cne $expectedVersion) {
            throw "$relative ProductVersion is '$($info.ProductVersion)'; expected '$expectedVersion'"
        }
        $fixedVersion = "$($info.FileMajorPart).$($info.FileMinorPart).$($info.FileBuildPart)"
        if ($fixedVersion -cne $expectedVersion) {
            throw "$relative fixed FileVersion is '$fixedVersion'; expected '$expectedVersion'"
        }
        Write-Host "$relative version OK (ProductVersion $($info.ProductVersion), fixed FileVersion $fixedVersion)"
    }
}
