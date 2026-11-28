# Two bounded, offline launches differing only in the saved uiOverlay value.
# Do not invoke while another VIctory title process is running.
param(
    [string] $Settings = (Join-Path $env:LOCALAPPDATA 'victory\data\7952987A\storage\settings\settings.xml'),
    [string] $Output = '',
    [ValidateRange(1, 5)] [double] $Minutes = 2
)

$ErrorActionPreference = 'Stop'
$harness = Join-Path $PSScriptRoot 'test-rx5700-offline.ps1'
$loader = Join-Path $PSScriptRoot '..\target\release\victory.exe'

function Sha256([byte[]] $Bytes) {
    $hasher = [System.Security.Cryptography.SHA256]::Create()
    try { return [BitConverter]::ToString($hasher.ComputeHash($Bytes)).Replace('-', '') }
    finally { $hasher.Dispose() }
}

function Assert-NoTitle {
    $running = @(Get-CimInstance Win32_Process -Filter "Name='victory.exe'" -ErrorAction Stop)
    if ($running.Count) {
        throw "A VIctory title process is still running (PID $($running[0].ProcessId)); the A/B test must run alone."
    }
}

function Replace-Once([byte[]] $Source, [byte[]] $Needle, [byte[]] $Replacement) {
    $at = -1
    for ($i = 0; $i -le $Source.Length - $Needle.Length; $i++) {
        $match = $true
        for ($j = 0; $j -lt $Needle.Length; $j++) {
            if ($Source[$i + $j] -ne $Needle[$j]) { $match = $false; break }
        }
        if ($match) {
            if ($at -ge 0) { throw 'settings.xml has more than one uiOverlay=true tag' }
            $at = $i
        }
    }
    if ($at -lt 0) { throw 'settings.xml must contain exactly one uiOverlay=true tag' }
    $result = New-Object byte[] ($Source.Length - $Needle.Length + $Replacement.Length)
    [Array]::Copy($Source, 0, $result, 0, $at)
    [Array]::Copy($Replacement, 0, $result, $at, $Replacement.Length)
    [Array]::Copy($Source, $at + $Needle.Length, $result, $at + $Replacement.Length,
                  $Source.Length - $at - $Needle.Length)
    return ,$result
}

function Summarize-Run([string] $Folder, [string] $Arm) {
    $stderr = Join-Path $Folder 'stderr.log'
    $stdout = Join-Path $Folder 'stdout.log'
    $lines = @("arm=$Arm", "settings_sha256=$(Sha256 ([IO.File]::ReadAllBytes($settingsPath)))")
    if (Test-Path -LiteralPath $stdout -PathType Leaf) {
        $lines += @(Select-String -LiteralPath $stdout -Pattern '^graphics ' | ForEach-Object { $_.Line })
    }
    if (Test-Path -LiteralPath $stderr -PathType Leaf) {
        $lines += @(Select-String -LiteralPath $stderr -Pattern '^VIctory: title\+0xa9438(cc|d0|ec|f0|f4)' |
            ForEach-Object { $_.Line })
        $lines += @(Select-String -LiteralPath $stderr -Pattern 'title created a 4K texture' |
            Select-Object -First 10 | ForEach-Object { $_.Line })
        $lines += @(Select-String -LiteralPath $stderr -Pattern 'first PresentX plane|first viewport' |
            Select-Object -First 2 | ForEach-Object { $_.Line })
    }
    $monitor = Join-Path $Folder 'monitor.csv'
    if (Test-Path -LiteralPath $monitor -PathType Leaf) {
        $samples = @(Import-Csv -LiteralPath $monitor)
        $maxTcp = 0
        $maxUdp = 0
        foreach ($sample in $samples) {
            $maxTcp = [Math]::Max($maxTcp, [int]$sample.tcp_endpoints)
            $maxUdp = [Math]::Max($maxUdp, [int]$sample.udp_endpoints)
        }
        $lines += "network_samples=$($samples.Count) max_tcp=$maxTcp max_udp=$maxUdp"
    }
    $lines | Set-Content -LiteralPath (Join-Path $Folder 'ui-overlay-summary.txt')
}

Assert-NoTitle
$settingsPath = [IO.Path]::GetFullPath($Settings)
if (-not (Test-Path -LiteralPath $settingsPath -PathType Leaf)) {
    throw "Saved title settings not found: $settingsPath"
}
if (-not $Output) {
    $stamp = (Get-Date).ToUniversalTime().ToString('yyyyMMdd-HHmmssZ')
    $Output = Join-Path $PSScriptRoot "..\target\test-runs\rx5700-ui-overlay-ab-$stamp"
}
$outPath = [IO.Path]::GetFullPath($Output)
if (Test-Path -LiteralPath $outPath) {
    throw "Output already exists: $outPath"
}
[void](New-Item -ItemType Directory -Path $outPath -Force)

$original = [IO.File]::ReadAllBytes($settingsPath)
$originalHash = Sha256 $original
$originalInfo = Get-Item -LiteralPath $settingsPath
$lastWriteUtc = $originalInfo.LastWriteTimeUtc
$attributes = $originalInfo.Attributes
$trueTag = [Text.Encoding]::ASCII.GetBytes('<uiOverlay value="true" />')
$falseTag = [Text.Encoding]::ASCII.GetBytes('<uiOverlay value="false" />')
$variant = Replace-Once $original $trueTag $falseTag
$variantHash = Sha256 $variant
$backup = Join-Path $outPath 'settings.original.bin'
[IO.File]::WriteAllBytes($backup, $original)
@("settings=$settingsPath", "original_sha256=$originalHash", "variant_sha256=$variantHash",
  "backup=$backup", "changed_tag=<uiOverlay value=`"false`" />") |
    Set-Content -LiteralPath (Join-Path $outPath 'manifest.txt')

$previousWatch = [Environment]::GetEnvironmentVariable('VICTORY_WATCH', 'Process')
$previousPoke = [Environment]::GetEnvironmentVariable('VICTORY_POKE', 'Process')
try {
    $env:VICTORY_WATCH = '0xa9438cc,0xa9438d0,0xa9438ec,0xa9438f0,0xa9438f4'
    [Environment]::SetEnvironmentVariable('VICTORY_POKE', $null, 'Process')

    $armA = Join-Path $outPath 'overlay-true'
    & $harness -Output $armA -Minutes $Minutes -Resolution '720p'
    if ($LASTEXITCODE -ne 0) { throw "Baseline harness failed with exit code $LASTEXITCODE" }
    Assert-NoTitle
    Summarize-Run $armA 'uiOverlay=true'
    $afterA = Sha256 ([IO.File]::ReadAllBytes($settingsPath))
    "baseline_settings_preserved=$($afterA -eq $originalHash) after_sha256=$afterA" |
        Add-Content -LiteralPath (Join-Path $outPath 'manifest.txt')
    $baselineExeHash = (Get-FileHash -LiteralPath $loader -Algorithm SHA256).Hash
    "loader_sha256_after_baseline=$baselineExeHash" |
        Add-Content -LiteralPath (Join-Path $outPath 'manifest.txt')
    # The title may save settings on exit. Start arm B from the same bytes.
    [IO.File]::WriteAllBytes($settingsPath, $original)
    if ((Sha256 ([IO.File]::ReadAllBytes($settingsPath))) -ne $originalHash) {
        throw 'Could not restore baseline settings before arm B'
    }

    [IO.File]::WriteAllBytes($settingsPath, $variant)
    if ((Sha256 ([IO.File]::ReadAllBytes($settingsPath))) -ne $variantHash) {
        throw 'Could not apply byte-exact uiOverlay=false variant'
    }
    $armB = Join-Path $outPath 'overlay-false'
    & $harness -Output $armB -Minutes $Minutes -Resolution '720p'
    if ($LASTEXITCODE -ne 0) { throw "Variant harness failed with exit code $LASTEXITCODE" }
    Assert-NoTitle
    Summarize-Run $armB 'uiOverlay=false'
    $afterB = Sha256 ([IO.File]::ReadAllBytes($settingsPath))
    "variant_settings_preserved=$($afterB -eq $variantHash) after_sha256=$afterB" |
        Add-Content -LiteralPath (Join-Path $outPath 'manifest.txt')
    $variantExeHash = (Get-FileHash -LiteralPath $loader -Algorithm SHA256).Hash
    "loader_sha256_after_variant=$variantExeHash" |
        Add-Content -LiteralPath (Join-Path $outPath 'manifest.txt')
    if ($afterB -ne $variantHash -or $baselineExeHash -ne $variantExeHash) {
        throw 'A/B comparison is invalid: settings changed during arm B or the loader build changed between arms'
    }
    Write-Output "A/B logs: $outPath"
} finally {
    [Environment]::SetEnvironmentVariable('VICTORY_WATCH', $previousWatch, 'Process')
    [Environment]::SetEnvironmentVariable('VICTORY_POKE', $previousPoke, 'Process')
    [IO.File]::WriteAllBytes($settingsPath, $original)
    [IO.File]::SetLastWriteTimeUtc($settingsPath, $lastWriteUtc)
    [IO.File]::SetAttributes($settingsPath, $attributes)
    $restoredHash = Sha256 ([IO.File]::ReadAllBytes($settingsPath))
    "restored_sha256=$restoredHash" | Add-Content -LiteralPath (Join-Path $outPath 'manifest.txt')
    if ($restoredHash -ne $originalHash) {
        throw "Settings restore hash mismatch; recover from $backup"
    }
}
