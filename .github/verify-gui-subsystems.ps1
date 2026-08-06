#Requires -Version 5.1
# Verifies that release executables are Windows GUI-subsystem PE binaries
# (subsystem 2), so direct launches never open a console window.
# Usage: pwsh .github/verify-gui-subsystems.ps1 [-RepoRoot <path>]
param(
    [string]$RepoRoot = $env:GITHUB_WORKSPACE
)

$ErrorActionPreference = "Stop"

if (-not $RepoRoot) {
    # Fall back to the repository root containing this script (.github/..).
    $RepoRoot = Split-Path -Parent $PSScriptRoot
}
$RepoRoot = (Resolve-Path -LiteralPath $RepoRoot).Path

$paths = @(
    "target/x86_64-pc-windows-msvc/release/leopardwm.exe",
    "target/x86_64-pc-windows-msvc/release/leopardwm-watchdog.exe"
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
