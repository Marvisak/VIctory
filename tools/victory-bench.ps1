<#
.SYNOPSIS
  Run a bounded Windows benchmark and write a privacy-preserving Markdown report.
.EXAMPLE
  .\tools\victory-bench.ps1 -Exe .\target\release\victory.exe -GameExe $env:VICTORY_GAME_EXE -Label 'main'
.NOTES
  Reports counts, timings, memory, and a crash RVA only. Keep the game executable,
  logs, captures, and derived data outside this repository.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf })]
    [string] $Exe,
    [string] $GameExe,
    [ValidateSet('series-x', 'series-s')]
    [string] $Console = 'series-s',
    [ValidateRange(1, 86400)]
    [int] $Seconds = 180,
    [ValidateRange(0.5, 128)]
    [double] $MaxRamGB = 8,
    [ValidateRange(0.25, 128)]
    [double] $MinSysGB = 3,
    [string] $Label = 'unlabelled build',
    [string] $Baseline,
    [string] $CrashLog,
    [string] $Output = (Join-Path $env:TEMP 'victory-bench')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-MetricValue([string] $Text, [string] $Metric) {
    $line = $Text -split "`r?`n" | Where-Object { $_ -match "\|\s*$([regex]::Escape($Metric))\s*\|" } | Select-Object -First 1
    if ($line -and $line -match '\|\s*([^|]+?)\s*\|\s*$') { return $Matches[1].Trim() }
    return $null
}

function Format-Value($Value, [string] $Suffix = '') {
    if ($null -eq $Value) { return 'n/a' }
    return "$Value$Suffix"
}

function Get-ReasonCounts([string] $Text) {
    $patterns = [ordered]@{
        'descriptor read only as data' = 'descriptor read only as data'
        'structured buffer of unknown stride' = 'structured buffer of unknown stride'
        'console wave64 lanes' = 'console wave64 lanes'
        'host texture refused' = 'host texture refused'
    }
    foreach ($name in $patterns.Keys) {
        $count = ([regex]::Matches($Text, [regex]::Escape($patterns[$name]), 'IgnoreCase')).Count
        if ($count) { "  {0}x  {1}" -f $count, $name }
    }
}

$run = Join-Path ([IO.Path]::GetTempPath()) ("victory-bench-{0}" -f [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $run | Out-Null
New-Item -ItemType Directory -Path $Output -Force | Out-Null
$stdout = Join-Path $run 'stdout.log'
$stderr = Join-Path $run 'stderr.log'
$arguments = @()
if ($GameExe) {
    if (-not (Test-Path -LiteralPath $GameExe -PathType Leaf)) { throw "Game executable not found: $GameExe" }
    $arguments += 'run', ('"{0}"' -f $GameExe), '--execute', '--console', $Console
}

$started = Get-Date
$exePath = (Resolve-Path -LiteralPath $Exe).Path
$start = @{ FilePath = $exePath; RedirectStandardOutput = $stdout; RedirectStandardError = $stderr; PassThru = $true }
if ($arguments.Count) { $start.ArgumentList = $arguments }
$process = Start-Process @start
$peakRam = 0L
$stopReason = 'completed'
$samples = [System.Collections.Generic.List[object]]::new()
try {
    while (-not $process.HasExited) {
        Start-Sleep -Seconds 3
        $process.Refresh()
        if ($process.WorkingSet64 -gt $peakRam) { $peakRam = $process.WorkingSet64 }
        $elapsed = ((Get-Date) - $started).TotalSeconds
        $free = (Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory * 1KB
        $samples.Add([pscustomobject]@{
            seconds = [math]::Round($elapsed, 1)
            process_ram_gb = [math]::Round($process.WorkingSet64 / 1GB, 2)
            free_system_gb = [math]::Round($free / 1GB, 2)
        })
        if ($process.WorkingSet64 -gt ($MaxRamGB * 1GB)) { $stopReason = 'process memory limit'; break }
        if ($free -lt ($MinSysGB * 1GB)) { $stopReason = 'system free-memory limit'; break }
        if ($elapsed -ge $Seconds) { $stopReason = 'time limit'; break }
    }
    if (-not $process.HasExited) { Stop-Process -Id $process.Id -Force }
    $process.WaitForExit()
} finally {
    $process.Dispose()
}

$text = ((Get-Content -Raw $stdout -ErrorAction SilentlyContinue), (Get-Content -Raw $stderr -ErrorAction SilentlyContinue) -join "`n")
$translationFailures = ([regex]::Matches($text, '(?im)(shader does not translate|translation failed|no host shader)')).Count
$textureFailures = ([regex]::Matches($text, '(?im)(host texture refused|texture creation failed|texture refused)')).Count
$fps = [double] $null
$draws = [double] $null
foreach ($line in ($text -split "`r?`n")) {
    if ($line -match '(?<fps>\d+(?:\.\d+)?)\s+fps,.*?(?<draws>\d+(?:\.\d+)?)\s+draws/frame') {
        $fps = [math]::Max($fps, [double] $Matches.fps)
        $draws = [math]::Max($draws, [double] $Matches.draws)
    }
}
$crashText = if ($CrashLog) {
    if (-not (Test-Path -LiteralPath $CrashLog -PathType Leaf)) { throw "Crash log not found: $CrashLog" }
    Get-Content -Raw -LiteralPath $CrashLog
} else { $text }
$crashRva = if ($crashText -match '(?i)(?:rva|title\+)\s*[:=]?\s*(0x[0-9a-f]+)') { $Matches[1] } else { '(none)' }
$report = Join-Path $Output ("report-{0}.md" -f (Get-Date -Format 'yyyyMMdd-HHmmss'))
$peak = [math]::Round($peakRam / 1GB, 2)
$duration = [math]::Round(((Get-Date) - $started).TotalSeconds, 1)
$reasons = @(Get-ReasonCounts $text)

$baselineRows = ''
if ($Baseline) {
    if (-not (Test-Path -LiteralPath $Baseline -PathType Leaf)) { throw "Baseline report not found: $Baseline" }
    $old = Get-Content -Raw -LiteralPath $Baseline
    $baselineRows = @(
        "| translation failures | $((Get-MetricValue $old 'translation failures')) | $translationFailures |",
        "| texture creation failures | $((Get-MetricValue $old 'texture creation failures')) | $textureFailures |",
        "| peak draws/frame | $((Get-MetricValue $old 'peak draws/frame')) | $(Format-Value $draws) |",
        "| peak fps | $((Get-MetricValue $old 'peak fps')) | $(Format-Value $fps) |",
        "| peak RAM | $((Get-MetricValue $old 'peak RAM')) | $peak GB |"
    ) -join "`n"
}

$content = @"
# VIctory benchmark

| field | value |
|---|---|
| label | $Label |
| console | $Console |
| duration | $duration s ($stopReason) |
| translation failures | $translationFailures |
| texture creation failures | $textureFailures |
| peak draws/frame | $(Format-Value $draws) |
| peak fps | $(Format-Value $fps) |
| peak RAM | $peak GB |
| crash RVA | $crashRva |

The report contains counts, timings, memory, and the faulting RVA only. It does not copy game files,
logs, screenshots, shader data, or other proprietary material.
"@
if ($reasons.Count) {
    $content += "`n## Failures by reason`n`n$($reasons -join "`n")`n"
}
if ($samples.Count) {
    $content += "`n## Timeline (3-second samples)`n`n| seconds | process RAM (GB) | free system RAM (GB) |`n|---:|---:|---:|`n"
    foreach ($sample in $samples) {
        $content += "| $($sample.seconds) | $($sample.process_ram_gb) | $($sample.free_system_gb) |`n"
    }
}
if ($baselineRows) {
    $content += @"

## Baseline comparison

| metric | baseline | this run |
|---|---|---|
$baselineRows
"@
}
Set-Content -LiteralPath $report -Value $content -Encoding utf8
Remove-Item -LiteralPath $run -Recurse -Force
Write-Output $report
