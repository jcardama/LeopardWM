#Requires -Version 5.1
# Verifies that release executables are Windows GUI-subsystem PE binaries
# (subsystem 2), so direct launches never open a console window.
# Usage: pwsh .github/verify-gui-subsystems.ps1 [-RepoRoot <path>] [-Targets <string[]>]
param(
    [string]$RepoRoot = $env:GITHUB_WORKSPACE,
    [string[]]$Targets = @()
)

$ErrorActionPreference = "Stop"

if (-not $RepoRoot) {
    # Fall back to the repository root containing this script (.github/..).
    $RepoRoot = Split-Path -Parent $PSScriptRoot
}
$RepoRoot = (Resolve-Path -LiteralPath $RepoRoot).Path

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
        "$targetDir/leopardwm-watchdog.exe"
    )

    foreach ($relative in $paths) {
        $path = Join-Path $RepoRoot $relative
        if (-not (Test-Path -LiteralPath $path)) {
            throw "$relative not found under $RepoRoot; build with 'cargo build --release' first"
        }
    $bytes = [IO.File]::ReadAllBytes($path)
    if ($bytes.Length -lt 64) {
        throw "$relative is too small to contain a PE header"
    }
    $peOffset = [BitConverter]::ToInt32($bytes, 0x3c)
    if ($peOffset -lt 0 -or $peOffset + 94 -gt $bytes.Length) {
        throw "$relative has an invalid PE header offset"
    }
    if ([Text.Encoding]::ASCII.GetString($bytes, $peOffset, 4) -ne "PE`0`0") {
        throw "$relative has an invalid PE signature"
    }
    $optionalHeader = $peOffset + 24
    $magic = [BitConverter]::ToUInt16($bytes, $optionalHeader)
    if ($magic -notin @(0x10b, 0x20b)) {
        throw "$relative has an unsupported PE optional-header magic: $magic"
    }
    $subsystem = [BitConverter]::ToUInt16($bytes, $optionalHeader + 68)
    if ($subsystem -ne 2) {
        throw "$relative subsystem is $subsystem; expected 2 (Windows GUI)"
    }
        Write-Host "$relative subsystem OK (2, Windows GUI)"
    }
}
