# Reproducible, bounded RX 5700 XT world test. The title is never launched
# unless enabled inbound and outbound firewall block rules match this loader.
param(
    [string] $Title = $env:VICTORY_TEST_TITLE,
    [string] $Loader = (Join-Path $PSScriptRoot '..\target\release\victory.exe'),
    [string] $FidelityFX = $env:VICTORY_TEST_FIDELITYFX_DIR,
    [string] $Output = '',
    [ValidateRange(1, 30)] [double] $Minutes = 8,
    [ValidatePattern('^(720p|1080p|[0-9]{3,4}x[0-9]{3,4})$')] [string] $Resolution = '720p',
    [ValidatePattern('^$|^[0-9]{3,4}x[0-9]{3,4}$')] [string] $PresentMax = '',
    [ValidateRange(0, 600)] [int] $WorldSeconds = 0,
    [ValidateRange(500, 100000)] [int] $WorldMinDraws = 500,
    [string] $Keys = '',
    [ValidateRange(1, 16)] [int] $MinimumFreeGB = 3,
    [ValidateRange(1000, 20000)] [int] $StreamBudgetMB = 9000,
    [ValidateRange(10, 95)] [double] $StreamLocalPercent = 85,
    [ValidateRange(0, 32)] [int] $StreamPristineMB = 0,
    [ValidateRange(0, 2048)] [int] $StreamMaxEdge = 0,
    [ValidateRange(0, 1536)] [int] $RootPressureTrimMB = 0,
    [ValidateRange(3750, 5000)] [int] $RootSystemMB = 3750,
    [string] $PrioritySources = '',
    [switch] $PriorityFullMips,
    [switch] $BoundPriorityMips,
    [switch] $DumpLightingBC,
    [switch] $Capture348EInputs,
    [switch] $Capture348EOutput,
    [switch] $CaptureEarlyLit,
    [switch] $CaptureEarlyDraws,
    [switch] $CaptureEarlyDrawRange,
    [ValidateRange(-1, 120)] [int] $Capture348EOffset = -1,
    [switch] $Capture577Inputs,
    [switch] $Capture731,
    [ValidateRange(0, 120)] [int] $Capture577Offset = 0,
    [switch] $CaptureFfx,
    [ValidateRange(0, 120)] [int] $CaptureFfxOffset = 0,
    [switch] $CaptureFfxAfterLit,
    [switch] $StopAfterLitCapture,
    [ValidateRange(0.25, 64.0)] [double] $FfxExposure = 1.0,
    [ValidateRange(0.0, 4.0)] [double] $FfxHighlightKnee = 0.0,
    [string] $Ambient = '',
    [string] $AmbientSky = '',
    [string] $AmbientShade = '',
    [string] $AmbientFile = '',
    [ValidatePattern('^$|^[0-9]{1,3}(,[0-9]{1,3})*$')] [string] $WorldCaptureOffsets = '',
    [switch] $CorrectFfxOutput,
    [switch] $Force348ELod0,
    [switch] $Force577Lod0,
    [switch] $Zero348EBC1,
    [switch] $Constant348ELuts,
    [switch] $CapturePresentedWorld,
    [ValidateRange(0, 120)] [int] $WorldOffsetStop = 0,
    [switch] $DumpPackedSkyRaw,
    [switch] $RepackPackedSkyR9,
    [switch] $SeparatePackedSkyR9,
    [switch] $SeparateFfxR9,
    [switch] $RootReuseOnly,
    [switch] $R9SameMemoryAlias,
    [string] $DumpWriter = '',
    [string] $DumpShaderHash = '',
    [string] $TraceShaderHash = '',
    [switch] $Disable731Composite,
    [string] $SkipPipelines = '',
    [switch] $D3D12Debug,
    [switch] $DredOnly,
    [ValidateRange(0, 256)] [int] $GpuProfileStride = 0,
    [switch] $ReleaseNoDiagnostics,
    [switch] $Coverage,
    [switch] $RequireNoUnexpectedDrops,
    [switch] $RequireNoUsedBudgetNulls,
    [switch] $TraceLitWrites,
    [switch] $TraceLitWritersOnly,
    [switch] $CheckpointLitWriters,
    [switch] $TraceLitStages,
    [switch] $TraceExit
)

$ErrorActionPreference = 'Stop'
if (-not $Title) { throw 'Provide -Title or VICTORY_TEST_TITLE with the local title EXE path.' }
if (-not $FidelityFX) { throw 'Provide -FidelityFX or VICTORY_TEST_FIDELITYFX_DIR with the local FidelityFX DLL directory.' }
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
$Title = Assert-FixedLocalPath $Title 'Title'
$FidelityFX = Assert-FixedLocalPath $FidelityFX 'FidelityFX directory'
$Loader = Assert-FixedLocalPath $Loader 'Loader'
if ($DumpLightingBC -and -not $BoundPriorityMips) {
    throw 'DumpLightingBC requires BoundPriorityMips to select the four shader-bound sources.'
}
if ($Capture348EOffset -eq 0) {
    throw 'Capture348EOffset starts at 1: the world marker is known only after its frame is presented.'
}
if ($Capture348EOffset -gt 0 -and -not ($Capture348EOutput -or $CaptureEarlyLit)) {
    throw 'Capture348EOffset requires Capture348EOutput or CaptureEarlyLit.'
}
if ($CaptureEarlyDraws -and -not $CaptureEarlyLit) {
    throw 'CaptureEarlyDraws requires CaptureEarlyLit.'
}
if ($CaptureEarlyDrawRange -and -not $CaptureEarlyLit) {
    throw 'CaptureEarlyDrawRange requires CaptureEarlyLit.'
}
if ($CaptureEarlyDraws -and $CaptureEarlyDrawRange) {
    throw 'Choose one early draw capture profile.'
}
$shaderVariants = 0
if ($Force348ELod0) { $shaderVariants++ }
if ($Zero348EBC1) { $shaderVariants++ }
if ($Constant348ELuts) { $shaderVariants++ }
if ($shaderVariants -gt 1) {
    throw 'Choose one 348e shader diagnostic variant per run.'
}
if ($Force577Lod0 -and $SkipPipelines -match '577248728544d74b') {
    throw 'Force577Lod0 cannot be tested while skipping shader 577248728544d74b.'
}
$scriptDirectory = Assert-FixedLocalPath $PSScriptRoot 'Harness script directory'
$repo = Assert-FixedLocalPath (Join-Path $scriptDirectory '..') 'Repository'
if ($ReleaseNoDiagnostics -and ($D3D12Debug -or $DredOnly -or $GpuProfileStride -or $Coverage -or $TraceLitWrites -or $TraceLitWritersOnly -or $CheckpointLitWriters -or $TraceLitStages -or $Force577Lod0 -or
    $Capture348EInputs -or $Capture348EOutput -or $CaptureEarlyLit -or $CaptureEarlyDraws -or $CaptureEarlyDrawRange -or $Capture577Inputs -or $Capture731 -or $CaptureFfx -or $CapturePresentedWorld -or
    $DumpWriter -or $DumpShaderHash -or $TraceShaderHash -or $DumpPackedSkyRaw)) {
    throw 'ReleaseNoDiagnostics cannot use diagnostic captures or D3D12 debug.'
}
if ($AmbientFile -and $ReleaseNoDiagnostics) {
    throw 'AmbientFile requires a diagnostics build.'
}
if ($AmbientFile -and (-not $Ambient -or $Ambient -eq 'off')) {
    throw 'AmbientFile requires Ambient to enable the fallback.'
}
$ambientFilePath = ''
if ($AmbientFile) {
    $ambientFilePath = Assert-FixedLocalPath $AmbientFile 'Ambient file'
    if (-not (Test-Path -LiteralPath $ambientFilePath -PathType Leaf)) {
        throw "AmbientFile does not exist: $ambientFilePath"
    }
}
if ($GpuProfileStride -and ($D3D12Debug -or $DredOnly)) {
    throw 'GpuProfileStride cannot be combined with D3D12 debug or DRED markers.'
}
if ($StopAfterLitCapture -and (-not $Capture577Inputs -or -not $CaptureFfx)) {
    throw 'StopAfterLitCapture requires Capture577Inputs and CaptureFfx.'
}
if ($CaptureFfxAfterLit -and (-not $Capture577Inputs -or -not $CaptureFfx)) {
    throw 'CaptureFfxAfterLit requires Capture577Inputs and CaptureFfx.'
}
$cargoCommand = Get-Command cargo.exe -ErrorAction SilentlyContinue
$cargo = if ($cargoCommand) { $cargoCommand.Source } else { Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe' }
if (-not (Test-Path -LiteralPath $cargo -PathType Leaf)) { throw "cargo.exe was not found: $cargo" }
Push-Location $repo
try {
    if ($ReleaseNoDiagnostics) {
        & $cargo build --offline --release -p victory
    } else {
        & $cargo build --offline --release -p victory --features diagnostics
    }
    if ($LASTEXITCODE -ne 0) { throw 'Loader build failed' }
} finally {
    Pop-Location
}
$loaderPath = (Resolve-Path -LiteralPath $Loader).Path
$titlePath = (Resolve-Path -LiteralPath $Title).Path
$gameDirectory = Split-Path -Parent $titlePath
if (-not (Test-Path -LiteralPath $FidelityFX -PathType Container)) {
    throw "AMD FidelityFX DLL directory is missing: $FidelityFX"
}
$fidelityFXPath = (Resolve-Path -LiteralPath $FidelityFX).Path
if (-not $Output) {
    $stamp = (Get-Date).ToUniversalTime().ToString('yyyyMMdd-HHmmssZ')
    $Output = Join-Path $PSScriptRoot "..\target\test-runs\rx5700-offline-$stamp"
}
$outPath = [IO.Path]::GetFullPath($Output)
$outPath = Assert-FixedLocalPath $outPath 'Test output'
if (Test-Path -LiteralPath $outPath) {
    if (Get-ChildItem -LiteralPath $outPath -Force | Select-Object -First 1) {
        throw "Output directory is not empty: $outPath"
    }
}

New-Item -ItemType Directory -Force -Path $outPath | Out-Null
# Pin the loader's config and persistent files to checked local paths. An
# inherited config, or victory.toml in the title's working directory, could
# otherwise redirect caches and saves to a network share outside the EXE WFP
# rules. This test owns its config file and does not read title-side settings.
$localAppData = Assert-FixedLocalPath $env:LOCALAPPDATA 'LOCALAPPDATA'
$cachePath = Assert-FixedLocalPath (Join-Path $localAppData 'victory\cache') 'Cache directory'
$dataPath = Assert-FixedLocalPath (Join-Path $localAppData 'victory\data') 'Data directory'
New-Item -ItemType Directory -Force -Path $cachePath, $dataPath | Out-Null
$cachePath = Assert-FixedLocalPath $cachePath 'Cache directory'
$dataPath = Assert-FixedLocalPath $dataPath 'Data directory'
$configPath = Assert-FixedLocalPath (Join-Path $outPath 'victory.toml') 'Test config'
Set-Content -LiteralPath $configPath -Value '[paths]' -Encoding ascii
$env:VICTORY_CONFIG = $configPath
$env:VICTORY_CACHE_DIR = $cachePath
$env:VICTORY_DATA_DIR = $dataPath
$env:VICTORY_GAME_DIR = $gameDirectory
$env:VICTORY_FIDELITYFX_DIR = $fidelityFXPath
foreach ($name in @(
    'VICTORY_EMIT_DUMP', 'VICTORY_AUDIO_CAPTURE', 'VICTORY_XMA_DUMP',
    'VICTORY_PIPELINE_LOG', 'VICTORY_DUMP_LIGHTING_BC_DIR', 'VICTORY_TRAP_LOG'
)) {
    Remove-Item -Path "Env:$name" -ErrorAction SilentlyContinue
}
if ($env:VICTORY_COOKED_PROFILE_MANIFEST) {
    $env:VICTORY_COOKED_PROFILE_MANIFEST = Assert-FixedLocalPath $env:VICTORY_COOKED_PROFILE_MANIFEST 'Cooked profile manifest'
}
$isolationLog = Join-Path $outPath 'isolation.log'
$preflight = Join-Path $PSScriptRoot 'isolation-preflight.ps1'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    & $preflight -Exe $loaderPath -Log $isolationLog
    if ($LASTEXITCODE -ne 0) { throw "Offline isolation preflight failed: $isolationLog" }
} else {
    $arguments = '-NoProfile -ExecutionPolicy Bypass -File "{0}" -Exe "{1}" -Log "{2}"' -f $preflight, $loaderPath, $isolationLog
    $isolation = Start-Process powershell.exe -ArgumentList $arguments -Verb RunAs -Wait -PassThru -WindowStyle Hidden
    if ($isolation.ExitCode -ne 0) { throw "Offline isolation preflight failed: $isolationLog" }
}
# The elevated preflight installs and checks WFP filters (including loopback).
# Verify the active firewall profile and rule scopes again in this process.
$profiles = @(Get-NetFirewallProfile -PolicyStore ActiveStore)
if ($profiles.Count -ne 3 -or @($profiles | Where-Object { -not $_.Enabled }).Count) {
    throw 'All three active Windows Firewall profiles must be enabled.'
}
$matchingRules = @(Get-NetFirewallRule -PolicyStore ActiveStore -DisplayName 'VIctory offline*victory.exe' -ErrorAction SilentlyContinue |
    Where-Object { (Get-NetFirewallApplicationFilter -AssociatedNetFirewallRule $_).Program -ieq $loaderPath })
foreach ($direction in @('Inbound', 'Outbound')) {
    $valid = @($matchingRules | Where-Object {
        [string]$_.Direction -eq $direction -and [string]$_.Action -eq 'Block' -and
        [string]$_.Enabled -eq 'True' -and [string]$_.Profile -eq 'Any' -and
        [string]($_ | Get-NetFirewallPortFilter).Protocol -eq 'Any' -and
        [string]($_ | Get-NetFirewallPortFilter).LocalPort -eq 'Any' -and
        [string]($_ | Get-NetFirewallPortFilter).RemotePort -eq 'Any' -and
        [string]($_ | Get-NetFirewallAddressFilter).LocalAddress -eq 'Any' -and
        [string]($_ | Get-NetFirewallAddressFilter).RemoteAddress -eq 'Any'
    })
    if (-not $valid.Count) { throw "No full-scope active $direction block rule matches $loaderPath." }
}

$frames = Join-Path $outPath 'frames'
New-Item -ItemType Directory -Force -Path $frames | Out-Null
$env:VICTORY_LOG = 'info'
if ($Coverage) {
    $env:VICTORY_COVERAGE = '1'
} else {
    Remove-Item Env:VICTORY_COVERAGE -ErrorAction SilentlyContinue
}
# The analyzer cannot distinguish a measured zero from disabled coverage
# unless the run records which switch was passed to the child process.
$coverageMode = if ($Coverage) { 'enabled' } else { 'disabled' }
Set-Content -LiteralPath (Join-Path $outPath 'coverage-mode.txt') -Value $coverageMode -Encoding ascii
if ($TraceLitWritersOnly) {
    if (-not $Capture577Inputs) { throw 'TraceLitWritersOnly requires Capture577Inputs.' }
    $env:VICTORY_TRACE_LIT_WRITES = '1'
    $env:VICTORY_TRACE_LIT_WRITERS_ONLY = '1'
} elseif ($TraceLitWrites) {
    if (-not $Capture348EOutput) { throw 'TraceLitWrites requires Capture348EOutput.' }
    $env:VICTORY_TRACE_LIT_WRITES = '1'
    Remove-Item Env:VICTORY_TRACE_LIT_WRITERS_ONLY -ErrorAction SilentlyContinue
} else {
    Remove-Item Env:VICTORY_TRACE_LIT_WRITES -ErrorAction SilentlyContinue
    Remove-Item Env:VICTORY_TRACE_LIT_WRITERS_ONLY -ErrorAction SilentlyContinue
}
if ($Capture731) {
    $env:VICTORY_CAPTURE_731 = '1'
} else {
    Remove-Item Env:VICTORY_CAPTURE_731 -ErrorAction SilentlyContinue
}
if ($CheckpointLitWriters) {
    if (-not $TraceLitWritersOnly -or -not $CaptureFfx) {
        throw 'CheckpointLitWriters requires TraceLitWritersOnly and CaptureFfx.'
    }
    $env:VICTORY_TRACE_LIT_WRITER_CHECKPOINTS = '1'
} else {
    Remove-Item Env:VICTORY_TRACE_LIT_WRITER_CHECKPOINTS -ErrorAction SilentlyContinue
}
if ($TraceLitStages) {
    if (-not $Capture348EOutput -or -not $CaptureFfx) {
        throw 'TraceLitStages requires Capture348EOutput and CaptureFfx.'
    }
    $env:VICTORY_TRACE_LIT_STAGES = '1'
} else {
    Remove-Item Env:VICTORY_TRACE_LIT_STAGES -ErrorAction SilentlyContinue
}
if ($Force577Lod0) {
    $env:VICTORY_577_FORCE_LOD0 = '1'
} else {
    Remove-Item Env:VICTORY_577_FORCE_LOD0 -ErrorAction SilentlyContinue
}
if ($DumpShaderHash) {
    $hashes = @($DumpShaderHash -split ',' | ForEach-Object { $_.Trim() })
    if (@($hashes | Where-Object { $_ -notmatch '^[0-9a-fA-F]{16}$' }).Count) {
        throw 'DumpShaderHash must contain 16-digit shader hashes separated by commas.'
    }
    $env:VICTORY_DUMP_SHADER_HASH = ($hashes -join ',').ToLowerInvariant()
} else {
    Remove-Item Env:VICTORY_DUMP_SHADER_HASH -ErrorAction SilentlyContinue
}
if ($TraceShaderHash) {
    if ($TraceShaderHash -notmatch '^[0-9a-fA-F]{16}$') {
        throw 'TraceShaderHash must be one 16-digit shader hash.'
    }
    $env:VICTORY_TRACE_SHADERS = $TraceShaderHash.ToLowerInvariant()
} else {
    Remove-Item Env:VICTORY_TRACE_SHADERS -ErrorAction SilentlyContinue
}
if ($Disable731Composite) {
    $env:VICTORY_DISABLE_731_COMPOSITE = '1'
} else {
    Remove-Item Env:VICTORY_DISABLE_731_COMPOSITE -ErrorAction SilentlyContinue
}
$env:VICTORY_STREAM_MB = [string]$StreamBudgetMB
$env:VICTORY_STREAM_LOCAL_PERCENT = [string]$StreamLocalPercent
if ($StreamPristineMB -gt 0) {
    $env:VICTORY_STREAM_PRISTINE_MB = [string]$StreamPristineMB
} else {
    Remove-Item Env:VICTORY_STREAM_PRISTINE_MB -ErrorAction SilentlyContinue
}
if ($RootPressureTrimMB -ne 0 -and $RootPressureTrimMB -lt 256) {
    throw 'RootPressureTrimMB must be zero or at least 256.'
}
if ($RootPressureTrimMB) {
    $env:VICTORY_ROOT_PRESSURE_TRIM_MB = [string]$RootPressureTrimMB
} else {
    Remove-Item Env:VICTORY_ROOT_PRESSURE_TRIM_MB -ErrorAction SilentlyContinue
}
$env:VICTORY_ROOT_SYSTEM_MB = [string]$RootSystemMB
if ($BoundPriorityMips) {
    $env:VICTORY_STREAM_PRIORITY_BOUND_MIPS = '1'
} else {
    Remove-Item Env:VICTORY_STREAM_PRIORITY_BOUND_MIPS -ErrorAction SilentlyContinue
}
if ($DumpLightingBC) {
    $env:VICTORY_DUMP_LIGHTING_BC_DIR = Join-Path $outPath 'lighting-bc'
} else {
    Remove-Item Env:VICTORY_DUMP_LIGHTING_BC_DIR -ErrorAction SilentlyContinue
}
if ($RootReuseOnly) {
    $env:VICTORY_ROOT_REUSE_ONLY = '1'
} else {
    Remove-Item Env:VICTORY_ROOT_REUSE_ONLY -ErrorAction SilentlyContinue
}
if ($R9SameMemoryAlias) {
    $env:VICTORY_R9_SAME_MEMORY_ALIAS = '1'
} else {
    Remove-Item Env:VICTORY_R9_SAME_MEMORY_ALIAS -ErrorAction SilentlyContinue
}
if ($PriorityFullMips -and -not $PrioritySources) {
    throw 'PriorityFullMips requires four PrioritySources addresses.'
}
if ($PrioritySources) {
    $env:VICTORY_STREAM_PRIORITY_SOURCES = $PrioritySources
} else {
    Remove-Item Env:VICTORY_STREAM_PRIORITY_SOURCES -ErrorAction SilentlyContinue
}
if ($PriorityFullMips) {
    $env:VICTORY_STREAM_PRIORITY_FULL_MIPS = '1'
} else {
    Remove-Item Env:VICTORY_STREAM_PRIORITY_FULL_MIPS -ErrorAction SilentlyContinue
}
$env:VICTORY_KEYS = if ($Keys) { $Keys } else { '30:Enter,45:Enter,60:Enter,80:Enter,100:Enter,120:Enter,150:Enter,180:Enter,210:Enter,240:Enter,270:Enter,300:Enter,330:Enter,345:Enter,360:Enter,375:Enter,390:Enter,405:Enter,420:Enter,435:Enter,450:Enter,465:Enter,480:Enter' }
$env:VICTORY_WINDOW_MAX = if ($Resolution -eq '720p') { '1280x720' } elseif ($Resolution -eq '1080p') { '1920x1080' } else { $Resolution }
if ($PresentMax) { $env:VICTORY_PRESENT_MAX = $PresentMax } else { Remove-Item Env:VICTORY_PRESENT_MAX -ErrorAction SilentlyContinue }
$env:VICTORY_DISABLE_SHADOWS = '1'
$env:VICTORY_RAY_TRACING = '0'
if ($Coverage -and -not $ReleaseNoDiagnostics) {
    $env:VICTORY_PIPELINE_LOG = Join-Path $outPath 'pipelines.tsv'
} else {
    Remove-Item Env:VICTORY_PIPELINE_LOG -ErrorAction SilentlyContinue
}
if ($StreamMaxEdge -gt 0) {
    $env:VICTORY_STREAM_MAX_EDGE = [string]$StreamMaxEdge
} else {
    Remove-Item Env:VICTORY_STREAM_MAX_EDGE -ErrorAction SilentlyContinue
}
$env:VICTORY_IGNORE_WINDOW_CLOSE = '1'
$env:VICTORY_CAPTURE = $frames
if ($Capture348EInputs) {
    $env:VICTORY_CAPTURE_348E_INPUTS = '1'
} else {
    Remove-Item Env:VICTORY_CAPTURE_348E_INPUTS -ErrorAction SilentlyContinue
}
if ($Capture348EOutput) {
    $env:VICTORY_CAPTURE_348E_OUTPUT = '1'
} else {
    Remove-Item Env:VICTORY_CAPTURE_348E_OUTPUT -ErrorAction SilentlyContinue
}
if ($CaptureEarlyLit) {
    $env:VICTORY_CAPTURE_EARLY_LIT = '1'
} else {
    Remove-Item Env:VICTORY_CAPTURE_EARLY_LIT -ErrorAction SilentlyContinue
}
if ($CaptureEarlyDraws) {
    $env:VICTORY_CAPTURE_EARLY_DRAWS = '1'
} else {
    Remove-Item Env:VICTORY_CAPTURE_EARLY_DRAWS -ErrorAction SilentlyContinue
}
if ($CaptureEarlyDrawRange) {
    $env:VICTORY_CAPTURE_EARLY_DRAW_RANGE = '1'
} else {
    Remove-Item Env:VICTORY_CAPTURE_EARLY_DRAW_RANGE -ErrorAction SilentlyContinue
}
if (($Capture348EOutput -or $CaptureEarlyLit) -and $Capture348EOffset -gt 0) {
    $env:VICTORY_CAPTURE_348E_OFFSET = [string]$Capture348EOffset
} else {
    Remove-Item Env:VICTORY_CAPTURE_348E_OFFSET -ErrorAction SilentlyContinue
}
if ($Capture577Inputs) {
    $env:VICTORY_CAPTURE_577_INPUTS = '1'
    $env:VICTORY_CAPTURE_577_OFFSET = [string]$Capture577Offset
} else {
    Remove-Item Env:VICTORY_CAPTURE_577_INPUTS -ErrorAction SilentlyContinue
    Remove-Item Env:VICTORY_CAPTURE_577_OFFSET -ErrorAction SilentlyContinue
}
if ($CaptureFfx) {
    $env:VICTORY_CAPTURE_FFX = '1'
    $env:VICTORY_CAPTURE_FFX_OFFSET = [string]$CaptureFfxOffset
    if ($CaptureFfxAfterLit) {
        $env:VICTORY_CAPTURE_FFX_AFTER_LIT = '1'
    } else {
        Remove-Item Env:VICTORY_CAPTURE_FFX_AFTER_LIT -ErrorAction SilentlyContinue
    }
} else {
    Remove-Item Env:VICTORY_CAPTURE_FFX -ErrorAction SilentlyContinue
    Remove-Item Env:VICTORY_CAPTURE_FFX_OFFSET -ErrorAction SilentlyContinue
    Remove-Item Env:VICTORY_CAPTURE_FFX_AFTER_LIT -ErrorAction SilentlyContinue
}
if ($CorrectFfxOutput) {
    $env:VICTORY_FFX_PACKED_OUTPUT = '1'
} else {
    Remove-Item Env:VICTORY_FFX_PACKED_OUTPUT -ErrorAction SilentlyContinue
}
$env:VICTORY_FFX_EXPOSURE = [string]$FfxExposure
if ($FfxHighlightKnee -gt 0) {
    $env:VICTORY_FFX_HIGHLIGHT_KNEE = $FfxHighlightKnee.ToString([Globalization.CultureInfo]::InvariantCulture)
} else {
    Remove-Item Env:VICTORY_FFX_HIGHLIGHT_KNEE -ErrorAction SilentlyContinue
}
if ($Ambient) { $env:VICTORY_AMBIENT = $Ambient } else { Remove-Item Env:VICTORY_AMBIENT -ErrorAction SilentlyContinue }
if ($AmbientSky) { $env:VICTORY_AMBIENT_SKY = $AmbientSky } else { Remove-Item Env:VICTORY_AMBIENT_SKY -ErrorAction SilentlyContinue }
if ($AmbientShade) { $env:VICTORY_AMBIENT_SHADE = $AmbientShade } else { $env:VICTORY_AMBIENT_SHADE = '0,0.6,0.1,0.45' }
Remove-Item Env:VICTORY_AMBIENT_TINT -ErrorAction SilentlyContinue
if ($ambientFilePath) {
    $env:VICTORY_AMBIENT_FILE = $ambientFilePath
    Set-Content -LiteralPath (Join-Path $outPath 'ambient-file.txt') -Value $ambientFilePath -Encoding utf8
} else {
    Remove-Item Env:VICTORY_AMBIENT_FILE -ErrorAction SilentlyContinue
}
if ($Force348ELod0) {
    $env:VICTORY_348E_FORCE_LOD0 = '1'
} else {
    Remove-Item Env:VICTORY_348E_FORCE_LOD0 -ErrorAction SilentlyContinue
}
if ($Zero348EBC1) {
    $env:VICTORY_348E_ZERO_BC1 = '1'
} else {
    Remove-Item Env:VICTORY_348E_ZERO_BC1 -ErrorAction SilentlyContinue
}
if ($Constant348ELuts) {
    $env:VICTORY_348E_CONSTANT_LUTS = '1'
} else {
    Remove-Item Env:VICTORY_348E_CONSTANT_LUTS -ErrorAction SilentlyContinue
}
Remove-Item Env:VICTORY_26B8_FULL_WIDTH -ErrorAction SilentlyContinue
if ($DumpPackedSkyRaw) {
    $env:VICTORY_DUMP_PACKED_SKY_RAW = '1'
} else {
    Remove-Item Env:VICTORY_DUMP_PACKED_SKY_RAW -ErrorAction SilentlyContinue
}
if ($RepackPackedSkyR9) {
    $env:VICTORY_REPACK_PACKED_SKY_R9 = '1'
} else {
    Remove-Item Env:VICTORY_REPACK_PACKED_SKY_R9 -ErrorAction SilentlyContinue
}
if ($SeparatePackedSkyR9) {
    if (-not $RepackPackedSkyR9) { throw 'SeparatePackedSkyR9 requires RepackPackedSkyR9.' }
    $env:VICTORY_SEPARATE_PACKED_SKY_R9 = '1'
} else {
    Remove-Item Env:VICTORY_SEPARATE_PACKED_SKY_R9 -ErrorAction SilentlyContinue
}
if ($SeparateFfxR9) {
    if (-not $CorrectFfxOutput -or -not $RepackPackedSkyR9) {
        throw 'SeparateFfxR9 requires CorrectFfxOutput and RepackPackedSkyR9.'
    }
    $env:VICTORY_SEPARATE_FFX_R9 = '1'
} else {
    Remove-Item Env:VICTORY_SEPARATE_FFX_R9 -ErrorAction SilentlyContinue
}
if ($DumpWriter) {
    $env:VICTORY_DUMP_TARGETS = '1'
    $env:VICTORY_DUMP_WRITERS = $DumpWriter
    if ($CapturePresentedWorld) {
        $env:VICTORY_DUMP_WRITERS_ONLY = '1'
    } else {
        Remove-Item Env:VICTORY_DUMP_WRITERS_ONLY -ErrorAction SilentlyContinue
    }
} else {
    Remove-Item Env:VICTORY_DUMP_TARGETS -ErrorAction SilentlyContinue
    Remove-Item Env:VICTORY_DUMP_WRITERS -ErrorAction SilentlyContinue
    Remove-Item Env:VICTORY_DUMP_WRITERS_ONLY -ErrorAction SilentlyContinue
}
if ($WorldCaptureOffsets) {
    $env:VICTORY_CAPTURE_WORLD = $WorldCaptureOffsets
} elseif ($CapturePresentedWorld) {
    $env:VICTORY_CAPTURE_WORLD = (@(0, 1, 5, $WorldOffsetStop) | Sort-Object -Unique) -join ','
} else {
    $env:VICTORY_CAPTURE_WORLD = '0,1,5,30,120,300'
}
$env:VICTORY_CAPTURE_WORLD_MIN_DRAWS = [string]$WorldMinDraws
if ($SkipPipelines) {
    $env:VICTORY_SKIP_PIPELINES = $SkipPipelines
} else {
    Remove-Item Env:VICTORY_SKIP_PIPELINES -ErrorAction SilentlyContinue
}
Remove-Item Env:VICTORY_CAPTURE_FRAMES -ErrorAction SilentlyContinue
Remove-Item Env:VICTORY_PEEK -ErrorAction SilentlyContinue
$env:VICTORY_FIDELITYFX_DIR = $fidelityFXPath
if ($D3D12Debug) {
    $env:VICTORY_D3D12_DEBUG = '1'
    $env:VICTORY_D3D12_DRED = '1'
} elseif ($DredOnly) {
    Remove-Item Env:VICTORY_D3D12_DEBUG -ErrorAction SilentlyContinue
    $env:VICTORY_D3D12_DRED = '1'
} else {
    Remove-Item Env:VICTORY_D3D12_DEBUG -ErrorAction SilentlyContinue
    Remove-Item Env:VICTORY_D3D12_DRED -ErrorAction SilentlyContinue
}
Remove-Item Env:VICTORY_GPU_TIMING -ErrorAction SilentlyContinue
if ($GpuProfileStride) {
    $env:VICTORY_GPU_PROFILE_STRIDE = [string]$GpuProfileStride
} else {
    Remove-Item Env:VICTORY_GPU_PROFILE_STRIDE -ErrorAction SilentlyContinue
}

$launchArgs = @('run', ('"' + $titlePath + '"'), '--execute', '--console', 'series-s',
    '--resolution', $Resolution, '--quality', 'low', '--aa', 'off', '--anisotropic', '0')
$stdout = Join-Path $outPath 'stdout.log'
$stderr = Join-Path $outPath 'stderr.log'
$monitor = Join-Path $outPath 'monitor.csv'
'utc,pid,working_gb,free_gb,tcp_endpoints,udp_endpoints,cpu_seconds,cpu_cores' | Set-Content -LiteralPath $monitor
$started = Get-Date
if ($TraceExit) {
    $debuggerPath = 'C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\cdb.exe'
    if (-not (Test-Path -LiteralPath $debuggerPath -PathType Leaf)) {
        throw "Windows debugger is missing: $debuggerPath"
    }
    $debuggerCommands = Join-Path $PSScriptRoot 'exit-debugger.txt'
    $debuggerLog = Join-Path $outPath 'debugger.log'
    # Local symbols only. The debugged title still uses the verified exact-path
    # WFP and firewall blocks installed above.
    $debuggerArgs = @('-y', 'C:\Windows\System32', '-cf', $debuggerCommands,
        '-logo', $debuggerLog, $loaderPath) + $launchArgs
    $process = Start-Process -FilePath $debuggerPath -ArgumentList $debuggerArgs -WorkingDirectory $gameDirectory -RedirectStandardOutput $stdout -RedirectStandardError $stderr -WindowStyle Hidden -PassThru
} else {
    $process = Start-Process -FilePath $loaderPath -ArgumentList $launchArgs -WorkingDirectory $gameDirectory -RedirectStandardOutput $stdout -RedirectStandardError $stderr -WindowStyle Hidden -PassThru
}
$tracked = @{}
$tracked[[int]$process.Id] = $process
$cpuPrevious = @{}
$stopReason = 'all tracked processes exited'
$worldObservedAt = $null
$harnessError = $null
"$(if ($TraceExit) { 'Debugger' } else { 'Loader' }) PID $($process.Id); output $outPath" | Write-Output

try {
    while ($true) {
        Start-Sleep -Seconds 5
        # The loader can relaunch itself. Follow only same-path descendants
        # of the process started above, including a child of an exited parent.
        $newChild = $true
        while ($newChild) {
            $newChild = $false
            $candidates = @(Get-CimInstance Win32_Process -Filter "Name='victory.exe'" -ErrorAction Stop)
            foreach ($candidate in $candidates) {
                $id = [int]$candidate.ProcessId
                if (-not $tracked.ContainsKey($id) -and
                    $tracked.ContainsKey([int]$candidate.ParentProcessId) -and
                    $candidate.ExecutablePath -ieq $loaderPath -and
                    $candidate.CreationDate -ge $started) {
                    $tracked[$id] = Get-Process -Id $id -ErrorAction Stop
                    $newChild = $true
                }
            }
        }
        $active = @($tracked.Values | Where-Object { $_.Refresh(); -not $_.HasExited })
        if (-not $active.Count) { break }
        $os = Get-CimInstance Win32_OperatingSystem
        $free = [math]::Round($os.FreePhysicalMemory / 1MB, 2)
        $tcpEndpoints = @(Get-NetTCPConnection -ErrorAction Stop)
        $udpEndpoints = @(Get-NetUDPEndpoint -ErrorAction Stop)
        foreach ($running in $active) {
            $sampledAt = (Get-Date).ToUniversalTime()
            $working = [math]::Round($running.WorkingSet64 / 1GB, 2)
            $tcp = @($tcpEndpoints | Where-Object { $_.OwningProcess -eq $running.Id }).Count
            $udp = @($udpEndpoints | Where-Object { $_.OwningProcess -eq $running.Id }).Count
            $cpuSeconds = [double]$running.CPU
            $cpuCores = 0.0
            if ($cpuPrevious.ContainsKey([int]$running.Id)) {
                $previous = $cpuPrevious[[int]$running.Id]
                $seconds = ($sampledAt - $previous.At).TotalSeconds
                if ($seconds -gt 0) {
                    $cpuCores = [math]::Round([math]::Max(0, $cpuSeconds - $previous.Cpu) / $seconds, 2)
                }
            }
            $cpuPrevious[[int]$running.Id] = @{ At = $sampledAt; Cpu = $cpuSeconds }
            '{0},{1},{2},{3},{4},{5},{6},{7}' -f $sampledAt.ToString('o'), $running.Id, $working, $free, $tcp, $udp, [math]::Round($cpuSeconds, 2), $cpuCores |
                Add-Content -LiteralPath $monitor
            if ($tcp -or $udp) {
                "Unexpected title socket endpoint at $(Get-Date -Format o): PID=$($running.Id) TCP=$tcp UDP=$udp" |
                    Add-Content -LiteralPath (Join-Path $outPath 'network-alert.log')
            }
        }
        if ($CapturePresentedWorld) {
            $worldLine = Select-String -LiteralPath $stderr -Pattern "the world's first frame" | Select-Object -Last 1
            if ($worldLine -and $worldLine.Line -match 'frame=(\d+)') {
                $presented = Join-Path $frames "frame-$([int64]$Matches[1] + $WorldOffsetStop).png"
                if (Test-Path -LiteralPath $presented -PathType Leaf) {
                    $stopReason = 'presented world frame captured'
                    break
                }
            }
        } elseif ($StopAfterLitCapture) {
            $litInput = Get-ChildItem -LiteralPath $frames -File -Filter '*-348e-577-input-*' | Select-Object -First 1
            $litPost = Get-ChildItem -LiteralPath $frames -File -Filter '*-348e-577-post-*' | Select-Object -First 1
            $ffxInput = Get-ChildItem -LiteralPath $frames -File -Filter '*-348e-ffx-pre-*' | Select-Object -First 1
            $ffxRead = Get-ChildItem -LiteralPath $frames -File -Filter '*-by-ffxpackedread.png' | Select-Object -First 1
            if ($litInput -and $litPost -and $ffxInput -and $ffxRead) {
                $stopReason = 'world lighting and FidelityFX inputs captured'
                break
            }
        } elseif ($DumpWriter) {
            $writerHashes = @($DumpWriter -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_ })
            $writerDump = Get-ChildItem -LiteralPath $frames -File -Filter '*-by-*' | Where-Object {
                $candidate = $_.Name
                @($writerHashes | Where-Object { $candidate.Contains($_) }).Count -gt 0
            } | Select-Object -First 1
            if ($writerDump) {
                $stopReason = 'target dump captured'
                break
            }
        }
        if ($WorldSeconds -gt 0) {
            # The first-world-frame marker already applies WorldMinDraws to a
            # single frame. The fps log averages draws over several frames,
            # so it can stay below that threshold throughout gameplay.
            $worldLine = Select-String -LiteralPath $stderr -Pattern "the world's first frame" -Quiet
            if (-not $worldObservedAt -and $worldLine) {
                $worldObservedAt = Get-Date
                "World timing window started at $($worldObservedAt.ToUniversalTime().ToString('o'))" |
                    Set-Content -LiteralPath (Join-Path $outPath 'world-timing.txt')
            }
            if ($worldObservedAt -and (Get-Date) -ge $worldObservedAt.AddSeconds($WorldSeconds)) {
                if (-not ($RequireNoUnexpectedDrops -or $RequireNoUsedBudgetNulls)) {
                    $stopReason = 'world timing window complete'
                    break
                }
                # A strict run must include the reporting interval that spans
                # the deadline. Otherwise its last few uses are unaccounted.
                $deadline = $worldObservedAt.ToUniversalTime().AddSeconds($WorldSeconds)
                $reported = $true
                if ($RequireNoUnexpectedDrops) {
                    $lastAccounting = Select-String -LiteralPath $stderr -Pattern 'drop accounting:' |
                        Select-Object -Last 1
                    $reported = $reported -and $lastAccounting -and
                        $lastAccounting.Line -match '^(\S+)\s+INFO .*drop accounting:' -and
                        [datetimeoffset]::Parse($Matches[1], [Globalization.CultureInfo]::InvariantCulture).UtcDateTime -ge $deadline
                }
                if ($RequireNoUsedBudgetNulls) {
                    $lastNullUse = Select-String -LiteralPath $stderr -Pattern 'actual shader-use sites with null host buffer descriptors' |
                        Select-Object -Last 1
                    $reported = $reported -and $lastNullUse -and
                        $lastNullUse.Line -match '^(\S+)\s+(?:INFO|WARN) .*actual shader-use sites with null host buffer descriptors' -and
                        [datetimeoffset]::Parse($Matches[1], [Globalization.CultureInfo]::InvariantCulture).UtcDateTime -ge $deadline
                }
                if ($reported) {
                    $stopReason = 'world timing window complete'
                    break
                }
                if ((Get-Date).ToUniversalTime() -ge $deadline.AddSeconds(30)) {
                    $stopReason = 'world timing window missing final strict report'
                    break
                }
            }
        }
        if ($free -lt $MinimumFreeGB -or (Get-Date) -ge $started.AddMinutes($Minutes)) {
            $stopReason = if ($free -lt $MinimumFreeGB) { 'low free RAM' } else { 'time limit' }
            break
        }
    }
} catch {
    $harnessError = $_.ToString()
    $stopReason = 'harness error'
    throw
} finally {
    "Stopped diagnostic run: $stopReason; elapsed_seconds=$([math]::Round(((Get-Date) - $started).TotalSeconds, 1))" |
        Set-Content -LiteralPath (Join-Path $outPath 'stop-reason.txt')
    if ($harnessError) {
        $harnessError | Set-Content -LiteralPath (Join-Path $outPath 'harness-error.log')
    }
    $results = @()
    # Stop the title before its debugger. Closing the debug port first can
    # terminate the title implicitly and obscure which process we stopped.
    foreach ($owned in @($tracked.Values | Sort-Object { $_.Id -eq $process.Id })) {
        try {
            $owned.Refresh()
            $killedByHarness = $false
            if (-not $owned.HasExited) {
                # The Process handle refers to our launch or verified child.
                $owned.Kill()
                $killedByHarness = $true
            }
            [void]$owned.WaitForExit(10000)
            $owned.Refresh()
            $code = if ($owned.HasExited) { "0x{0:X8}" -f ([int]$owned.ExitCode) } else { 'still-running' }
            $results += "PID=$($owned.Id) exited=$($owned.HasExited) exit_code=$code killed_by_harness=$killedByHarness"
        } catch {
            "Could not stop tracked PID $($owned.Id): $_" |
                Add-Content -LiteralPath (Join-Path $outPath 'cleanup-error.log')
        }
    }
    $gpuFailure = (Test-Path -LiteralPath $stderr) -and
        [bool](Select-String -LiteralPath $stderr -Pattern 'ERROR victory_gpu::host: host GPU device removed' -Quiet)
    @("reason=$stopReason", "tracked=$($tracked.Count)", "gpu_failure_reported=$gpuFailure", "gpu_profile_stride=$GpuProfileStride") + $results |
        Set-Content -LiteralPath (Join-Path $outPath 'result.txt')
}
if ($Capture348EOutput) {
    $lightingCaptured = [bool](Get-ChildItem -LiteralPath $frames -File -Filter '*-348e-post-*' | Select-Object -First 1)
    "lighting_snapshot_captured=$lightingCaptured" | Add-Content -LiteralPath (Join-Path $outPath 'result.txt')
    if (-not $lightingCaptured) {
        Write-Warning 'The 348e lighting output capture was requested but no snapshot was saved; inspect stderr.log for the missing-resource warning.'
    }
}
"Logs and captures: $outPath" | Write-Output
$python = Get-Command python.exe -ErrorAction SilentlyContinue
if ($python) {
    $analysisArgs = @((Join-Path $PSScriptRoot 'analyze-diagnostic-run.py'), $outPath)
    if ($RequireNoUnexpectedDrops) { $analysisArgs += '--require-no-unexpected-drops' }
    if ($RequireNoUsedBudgetNulls) { $analysisArgs += '--require-no-used-budget-nulls' }
    & $python.Source @analysisArgs
    if ($LASTEXITCODE -ne 0) {
        if ($RequireNoUnexpectedDrops -or $RequireNoUsedBudgetNulls) {
            throw "Strict world diagnostic gate failed; inspect $outPath\coverage.json"
        }
        Write-Warning 'Diagnostic coverage report failed; raw logs remain available.'
    }
}
