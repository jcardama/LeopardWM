# Final validation. Each stage runs quietly and prints one line when it passes; the first failing stage
# prints its first diagnostic and stops the run.
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path $PSScriptRoot -Parent)

$diagnosticStart = '^(error(\[\w+\])?:|---- .+ stdout ----|FAIL:|ERROR:)'
$diagnosticEnd = '^(error(\[\w+\])?:|---- .+ stdout ----|failures:|FAIL:|ERROR:)'

function Select-FirstDiagnostic([string[]]$Lines) {
    $start = -1
    for ($i = 0; $i -lt $Lines.Count; $i++) { if ($Lines[$i] -match $diagnosticStart) { $start = $i; break } }
    if ($start -lt 0) { return $Lines | Select-Object -Last 40 }
    $end = [Math]::Min($Lines.Count, $start + 40)
    for ($i = $start + 1; $i -lt $end; $i++) { if ($Lines[$i] -match $diagnosticEnd) { $end = $i; break } }
    return $Lines[$start..($end - 1)]
}

function Invoke-Stage([string]$Label, [string]$FilePath, [string[]]$ArgumentList) {
    $stdout = [IO.Path]::GetTempFileName()
    $stderr = [IO.Path]::GetTempFileName()
    try {
        $process = Start-Process -FilePath $FilePath -ArgumentList $ArgumentList -NoNewWindow -PassThru `
            -RedirectStandardOutput $stdout -RedirectStandardError $stderr
        $null = $process.Handle
        $process.WaitForExit()
        $lines = @(Get-Content $stdout) + @(Get-Content $stderr)
        if ($process.ExitCode -eq 0) {
            $passed = 0
            foreach ($line in $lines) { if ($line -match '^test result: ok\. (\d+) passed') { $passed += [int]$Matches[1] } }
            $summary = if ($passed) { " ($passed tests)" } else { '' }
            Write-Output "  pass  $Label$summary"
            return
        }
        Write-Output "  FAIL  $Label"
        Select-FirstDiagnostic $lines | ForEach-Object { "    $_" }
        exit $process.ExitCode
    } finally {
        Remove-Item $stdout, $stderr -ErrorAction SilentlyContinue
    }
}

Invoke-Stage 'Clippy' 'cargo' @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings')
Invoke-Stage 'Test' 'cargo' @('test', '--workspace')
Invoke-Stage 'Tools tests' 'python' @('-m', 'unittest', 'discover', '-s', 'tools', '-p', 'test_desktop_acceptance.py')
