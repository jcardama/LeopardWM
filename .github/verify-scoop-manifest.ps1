#Requires -Version 5.1
# Verifies the local Scoop manifest matches the current stable release contract.
# Usage: pwsh .github/verify-scoop-manifest.ps1 [-RepoRoot <path>]
param(
    [string]$RepoRoot = $env:GITHUB_WORKSPACE
)

$ErrorActionPreference = "Stop"

if (-not $RepoRoot) {
    $RepoRoot = Split-Path -Parent $PSScriptRoot
}
$RepoRoot = (Resolve-Path -LiteralPath $RepoRoot).Path
$manifestPath = Join-Path $RepoRoot "dist/scoop/leopardwm.json"

if (-not (Test-Path -LiteralPath $manifestPath)) {
    throw "Scoop manifest not found: $manifestPath"
}

try {
    $manifest = [IO.File]::ReadAllText($manifestPath) | ConvertFrom-Json
}
catch {
    throw "Scoop manifest is not valid JSON: $manifestPath`n$($_.Exception.Message)"
}

function Assert-Equal {
    param(
        [string]$Name,
        [string]$Expected,
        [object]$Actual
    )

    if ($null -eq $Actual -or [string]$Actual -cne $Expected) {
        throw "$Name must be '$Expected'; found '$Actual'."
    }
}

if ($null -eq $manifest.version) {
    throw "Scoop manifest must define a version."
}
$expectedVersion = [string]$manifest.version
if ($expectedVersion -notmatch '^\d+\.\d+\.\d+$') {
    throw "version must be a stable SemVer value such as '0.2.7'; found '$expectedVersion'."
}

$expectedUrl = "https://github.com/jcardama/LeopardWM/releases/download/v$expectedVersion/LeopardWM-$expectedVersion-x86_64-windows.zip"
$expectedExtractDir = "LeopardWM-$expectedVersion-x86_64-windows"
$expectedAutoupdateUrl = 'https://github.com/jcardama/LeopardWM/releases/download/v$version/LeopardWM-$version-x86_64-windows.zip'
$expectedAutoupdateExtractDir = 'LeopardWM-$version-x86_64-windows'
$expectedBins = @("leopardwm.exe", "leopardwm-cli.exe", "lwm.exe")

Assert-Equal "architecture.64bit.url" $expectedUrl $manifest.architecture.'64bit'.url
Assert-Equal "architecture.64bit.extract_dir" $expectedExtractDir $manifest.architecture.'64bit'.extract_dir

$hash = [string]$manifest.architecture.'64bit'.hash
if ($hash -notmatch '^[0-9a-fA-F]{64}$') {
    throw "architecture.64bit.hash must be a 64-character hexadecimal SHA256; found '$hash'."
}
Assert-Equal "checkver" "github" $manifest.checkver
Assert-Equal "autoupdate.architecture.64bit.url" $expectedAutoupdateUrl $manifest.autoupdate.architecture.'64bit'.url
Assert-Equal "autoupdate.architecture.64bit.extract_dir" $expectedAutoupdateExtractDir $manifest.autoupdate.architecture.'64bit'.extract_dir

$bins = @($manifest.bin)
if ($bins.Count -ne $expectedBins.Count) {
    throw "bin must contain exactly $($expectedBins.Count) entries; found $($bins.Count)."
}

for ($index = 0; $index -lt $expectedBins.Count; $index++) {
    Assert-Equal "bin[$index]" $expectedBins[$index] $bins[$index]
}

Write-Host "Scoop manifest OK (v$expectedVersion)"
