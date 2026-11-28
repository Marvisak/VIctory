# Launch a staged title only after verifying local inbound/outbound isolation.
# The WFP preflight elevates separately so the title can run at user integrity.
param(
    [Parameter(Mandatory = $true)] [string] $Exe,
    [ValidateRange(1, 900)] [int] $TimeoutSeconds = 30,
    [string] $LogDirectory = '',
    [string] $PackageDirectory = '',
    [switch] $CaptureFrames
)

$ErrorActionPreference = 'Stop'
function Assert-FixedLocalPath([string] $Path, [string] $Label) {
    $full = [IO.Path]::GetFullPath($Path)
    if ($full -notmatch '^[A-Za-z]:\\' -or
        [IO.DriveInfo]::new($full.Substring(0, 1)).DriveType -ne [IO.DriveType]::Fixed) {
        throw "$Label must be on a fixed local drive."
    }
    $partPath = $full.Substring(0, 3)
    foreach ($part in ($full.Substring(3) -split '\\')) {
        if (-not $part) { continue }
        $partPath = Join-Path $partPath $part
        if (Test-Path -LiteralPath $partPath) {
            if ([IO.File]::GetAttributes($partPath) -band [IO.FileAttributes]::ReparsePoint) {
                throw "$Label cannot use a reparse point: $partPath"
            }
        }
    }
    return $full
}
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
$isAdministrator = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

$scriptDirectory = Assert-FixedLocalPath -Path $PSScriptRoot -Label 'Script directory'
$repo = Assert-FixedLocalPath -Path (Join-Path $scriptDirectory '..') -Label 'Repository'
$exePath = Assert-FixedLocalPath -Path $Exe -Label 'Title EXE'
$exePath = (Resolve-Path -LiteralPath $exePath).Path
if ((Get-Item -LiteralPath $exePath).Extension -ine '.exe') { throw 'The target must be an EXE' }
$packagePath = if ($PackageDirectory) {
    Assert-FixedLocalPath -Path $PackageDirectory -Label 'Package directory'
} else {
    Split-Path -Parent $exePath
}
$packagePath = Assert-FixedLocalPath $packagePath 'Package directory'
$packagePath = (Resolve-Path -LiteralPath $packagePath).Path
if (-not (Test-Path -LiteralPath (Join-Path $packagePath 'appdata0.rpf') -PathType Leaf)) {
    throw "Package directory does not contain appdata0.rpf: $packagePath"
}
if (-not $LogDirectory) {
    $stamp = (Get-Date).ToUniversalTime().ToString('yyyyMMdd-HHmmssZ')
    $LogDirectory = Join-Path $repo "target\first-run\$stamp"
}
$logPath = [IO.Path]::GetFullPath($LogDirectory)
$logPath = Assert-FixedLocalPath $logPath 'Log directory'
if (Test-Path -LiteralPath $logPath) {
    if (Get-ChildItem -LiteralPath $logPath -Force | Select-Object -First 1) {
        throw "Log directory is not empty: $logPath"
    }
}
New-Item -ItemType Directory -Force -Path $logPath | Out-Null
$capturePath = if ($CaptureFrames) { Join-Path $logPath 'frames' } else { $null }

$metadata = [ordered]@{
    timestamp_utc = (Get-Date).ToUniversalTime().ToString('o')
    executable = $exePath
    package_directory = $packagePath
    title_integrity = if ($isAdministrator) { 'administrator' } else { 'user' }
    sha256 = (Get-FileHash -LiteralPath $exePath -Algorithm SHA256).Hash
    size_bytes = (Get-Item -LiteralPath $exePath).Length
    os_version = [Environment]::OSVersion.VersionString
    process_architecture = $env:PROCESSOR_ARCHITECTURE
    timeout_seconds = $TimeoutSeconds
    log_directory = $logPath
    capture_directory = $capturePath
}
$metadata | ConvertTo-Json -Depth 3 | Set-Content -LiteralPath (Join-Path $logPath 'metadata.json')

$blockScript = Join-Path $PSScriptRoot 'block-native-title-outbound.ps1'
$isolationLog = Join-Path $logPath 'isolation.log'
try {
    if ($isAdministrator) {
        & $blockScript -Exe $exePath *>&1 | Out-File -LiteralPath $isolationLog -Encoding UTF8
    } else {
        $helper = Join-Path $PSScriptRoot 'isolation-preflight.ps1'
        $arguments = '-NoProfile -ExecutionPolicy Bypass -File "{0}" -Exe "{1}" -Log "{2}"' -f $helper, $exePath, $isolationLog
        $preflight = Start-Process powershell.exe -ArgumentList $arguments -Verb RunAs -Wait -PassThru -WindowStyle Hidden
        if ($preflight.ExitCode -ne 0) {
            throw "Elevated isolation preflight exited with $($preflight.ExitCode)"
        }
    }
} catch {
    ($_ | Out-String) | Add-Content -LiteralPath $isolationLog
    throw "Isolation preflight failed; title not started. See $isolationLog"
}

$gcc = (Get-Command gcc.exe -ErrorAction SilentlyContinue).Source
if (-not $gcc) { throw 'gcc.exe is required to build the first-run launcher' }
$gcc = Assert-FixedLocalPath $gcc 'GCC executable'
$source = Assert-FixedLocalPath (Join-Path $scriptDirectory 'victory-first-run.c') 'First-run helper source'
$launcher = Assert-FixedLocalPath (Join-Path $repo 'target\offline-wfp\victory-first-run.exe') 'First-run helper'
if (-not (Test-Path -LiteralPath $launcher) -or
    (Get-Item -LiteralPath $source).LastWriteTimeUtc -gt (Get-Item -LiteralPath $launcher).LastWriteTimeUtc) {
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $launcher) | Out-Null
    & $gcc -O2 -Wall -Wextra -municode -o $launcher $source
    if ($LASTEXITCODE -ne 0) { throw 'First-run launcher build failed' }
}

$mappedDrives = New-Object 'System.Collections.Generic.List[string]'
$driveLog = Join-Path $logPath 'drive-mappings.log'
$eventLog = Join-Path $logPath 'application-errors.log'
$exitCode = 1
$isolatedEnvNames = @(
    'VICTORY_CAPTURE', 'VICTORY_CAPTURE_FRAMES', 'VICTORY_AUDIO_CAPTURE',
    'VICTORY_XMA_DUMP', 'VICTORY_PIPELINE_LOG', 'VICTORY_DUMP_LIGHTING_BC_DIR',
    'VICTORY_TRAP_LOG', 'VICTORY_EMIT_DUMP', 'VICTORY_AMBIENT_FILE',
    'VICTORY_COOKED_PROFILE_MANIFEST',
    'VICTORY_CONFIG', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR'
)
$previousEnv = @{}
try {
    foreach ($name in $isolatedEnvNames) {
        $previousEnv[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        [Environment]::SetEnvironmentVariable($name, $null, 'Process')
    }
    if ($CaptureFrames) {
        New-Item -ItemType Directory -Force -Path $capturePath | Out-Null
        $env:VICTORY_CAPTURE = $capturePath
        $env:VICTORY_CAPTURE_FRAMES = '30,300,600,900,1200,1800,2400,3600,5400,7200'
    }
    # Xbox titles address package and temporary files through fixed drive letters.
    # Refuse an occupied drive rather than changing the user's existing mapping.
    $titleTemp = Join-Path $logPath 'title-temp'
    New-Item -ItemType Directory -Force -Path $titleTemp | Out-Null
    foreach ($mapping in @(@('G:', $packagePath), @('T:', $titleTemp))) {
        $drive = $mapping[0]
        $destination = $mapping[1]
        if (Test-Path -LiteralPath "$drive\") { throw "Drive $drive is already in use" }
        & subst.exe $drive $destination
        if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath "$drive\")) {
            throw "Could not map $drive to $destination"
        }
        $mappedDrives.Add($drive)
        "$drive -> $destination" | Add-Content -LiteralPath $driveLog
    }

    $started = Get-Date
    & $launcher $exePath $logPath $TimeoutSeconds
    $exitCode = $LASTEXITCODE
    $events = @()
    for ($attempt = 0; $attempt -lt 3; $attempt++) {
        $events = @(Get-WinEvent -FilterHashtable @{ LogName = 'Application'; Id = 1000; StartTime = $started.AddSeconds(-2) } -ErrorAction SilentlyContinue |
            Where-Object { $_.ProviderName -eq 'Application Error' -and $_.Message.Contains($exePath) } |
            Select-Object -First 5)
        if ($events.Count) { break }
        Start-Sleep -Seconds 1
    }
    if ($events.Count) {
        $events | ForEach-Object { "[$($_.TimeCreated)] $($_.Message)" } | Set-Content -LiteralPath $eventLog
    } else {
        'No matching Application Error event was recorded during this run.' | Set-Content -LiteralPath $eventLog
    }
} finally {
    foreach ($name in $isolatedEnvNames) {
        [Environment]::SetEnvironmentVariable($name, $previousEnv[$name], 'Process')
    }
    for ($i = $mappedDrives.Count - 1; $i -ge 0; $i--) {
        & subst.exe $mappedDrives[$i] /D
        if ($LASTEXITCODE -ne 0) {
            "Failed to remove $($mappedDrives[$i]) mapping" | Add-Content -LiteralPath $driveLog
        }
    }
}
Write-Output "First-run exit code: $exitCode"
Write-Output "Diagnostics: $logPath"
Write-Output "Isolation: $isolationLog"
Write-Output "Process: $(Join-Path $logPath 'first-run.log')"
Write-Output "Application errors: $eventLog"
Write-Output "Native shim: $(Join-Path $logPath 'victory-native.log')"
if ($exitCode -ne 0) { exit $exitCode }
