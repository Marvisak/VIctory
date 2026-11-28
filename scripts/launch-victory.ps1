# Windows launch menu for the standalone loader. The title is launched only
# after the exact loader executable has passed the offline WFP/firewall check.
param(
    [switch] $ValidateOnly,
    [switch] $ValidateUi,
    [switch] $ValidateCookedOnly,
    [switch] $CookOnly,
    [switch] $ValidateCookInputs,
    [switch] $ValidateSafeEnvironment,
    [string] $UiProfile = '',
    [string] $SettingsPath = ''
)

$ErrorActionPreference = 'Stop'
function Assert-LocalFixedPath([string] $value, [string] $label) {
    if ([string]::IsNullOrWhiteSpace($value)) { throw "$label must be an absolute local path." }
    try { $full = [IO.Path]::GetFullPath($value) }
    catch { throw "$label is not a valid local path." }
    if ($full -notmatch '^[a-zA-Z]:\\') { throw "$label must be on a local fixed drive (UNC paths are not allowed)." }
    if ([IO.DriveInfo]::new($full.Substring(0, 1)).DriveType -ne [IO.DriveType]::Fixed) {
        throw "$label must be on a local fixed drive."
    }
    $current = $full.Substring(0, 3)
    foreach ($part in ($full.Substring(3) -split '\\' | Where-Object { $_ })) {
        $current = Join-Path $current $part
        if (Test-Path -LiteralPath $current) {
            $item = Get-Item -LiteralPath $current -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "$label must not pass through a reparse point: $current"
            }
        }
    }
    return $full
}
$repo = Assert-LocalFixedPath (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path 'Repository folder'
$loader = Join-Path $repo 'target\release\victory.exe'
$stateDir = Assert-LocalFixedPath (Join-Path $env:LOCALAPPDATA 'VIctory') 'Launcher state folder'
if (-not $SettingsPath) { $SettingsPath = Join-Path $stateDir 'launcher.json' }
$SettingsPath = Assert-LocalFixedPath $SettingsPath 'Launcher settings file'
[void](Assert-LocalFixedPath (Split-Path -Parent $SettingsPath) 'Launcher settings folder')
$logRoot = Assert-LocalFixedPath (Join-Path $stateDir 'logs') 'Launcher log folder'

$defaults = [ordered]@{
    gameRoot = ''
    title = ''
    fidelityfx = ''
    useFidelityFx = $false
    console = 'series-s'
    resolutionProfile = 'custom'
    useCookedTextures = $false
    cookedRoot = ''
    cookManifest = ''
    cookInputRoot = ''
    cookIdentities = ''
    cookTexconv = ''
    cookTexassemble = ''
    resolution = '1280x720'
    presentMax = '1280x720'
    quality = 'low'
    aa = 'off'
    anisotropic = 0
    disableShadows = $true
    rayTracing = $false
    motionBlur = $false
    reflections = $false
    vrs = $false
    vsync = $false
    hdr = $false
    sharpening = $false
    particleShadows = $false
    longShadows = $false
    treeTessellation = $false
    frameLimit = 30
    disable731Composite = $false
    ambient = $true
    streamBudgetMB = 9000
    streamLocalPercent = 85
    streamMaxEdge = 512
    rootSystemMB = 4750
    rootPressureTrimMB = 768
    rootReuseOnly = $true
    boundPriorityMips = $true
    ffxExposure = 16.0
    autoEnter = $false
    logLevel = 'info'
    rebuild = $true
    prebuildShaders = $false
    diagFrameCapture = $false
    diagWorldCapture = $false
    diagCaptureFullRes = $false
    diagFrameNumbers = '60,300'
    diagWorldOffsets = '0,1,5'
    diagTextureTrace = $false
    diagShaderTrace = $false
    diagShaderHashes = ''
    diagDumpShaderSource = $false
    diagSpriteTargets = $false
    diagCoverage = $false
    diagGpuTiming = $false
    diagD3dDebug = $false
    diagDred = $false
}

function Read-LauncherSettings {
    $value = [pscustomobject]$defaults
    if (Test-Path -LiteralPath $script:SettingsPath -PathType Leaf) {
        $saved = Get-Content -LiteralPath $script:SettingsPath -Raw | ConvertFrom-Json
        foreach ($name in $defaults.Keys) {
            if ($saved.PSObject.Properties.Name -contains $name) {
                $value.$name = $saved.$name
            }
        }
    }
    return $value
}

function New-LaunchPlan([pscustomobject] $settings) {
    if ($settings.gameRoot) { [void](Assert-LocalFixedPath ([string]$settings.gameRoot) 'Game folder') }
    if ($settings.console -notin @('series-s', 'series-x')) { throw 'Choose Series S or Series X.' }
    if ($settings.resolutionProfile -notin @('720p', '1080p', 'native', 'custom')) { throw 'Choose a resolution profile.' }
    $cookedManifest = $null
    if ($settings.useCookedTextures) {
        if ($settings.resolutionProfile -notin @('720p', '1080p')) {
            throw 'Cooked texture profiles require a 720p or 1080p resolution profile.'
        }
        if (-not $settings.cookedRoot) { throw 'Choose a cooked texture output folder.' }
        $cookedRoot = Assert-LocalFixedPath ([string]$settings.cookedRoot) 'Cooked texture folder'
        if (-not (Test-Path -LiteralPath $cookedRoot -PathType Container)) { throw 'Cooked texture folder does not exist.' }
        $cookedManifest = Assert-LocalFixedPath (Join-Path $cookedRoot ("runtime_$($settings.resolutionProfile).toml")) 'Cooked texture manifest'
        if (-not (Test-Path -LiteralPath $cookedManifest -PathType Leaf)) {
            throw "The selected cooked profile manifest is missing: $cookedManifest"
        }
        $manifestItem = Get-Item -LiteralPath $cookedManifest
        if ($manifestItem.Length -lt 1 -or $manifestItem.Length -gt 4194304) { throw 'Cooked profile manifest must be 1 byte to 4 MB.' }
        $manifestHeader = Get-Content -LiteralPath $cookedManifest -TotalCount 60
        if (-not ($manifestHeader -match '^\s*version\s*=\s*1\s*(#.*)?$') -or
            -not ($manifestHeader -match ('^\s*profile\s*=\s*"' + [regex]::Escape([string]$settings.resolutionProfile) + '"\s*(#.*)?$'))) {
            throw 'Cooked manifest header must match version 1 and the selected resolution profile.'
        }
    }
    if ($settings.resolution -notmatch '^[0-9]{3,4}x[0-9]{3,4}$') { throw 'Choose a valid resolution.' }
    $size = $settings.resolution -split 'x'
    if ([int]$size[0] -lt 640 -or [int]$size[0] -gt 7680 -or [int]$size[1] -lt 360 -or [int]$size[1] -gt 4320) {
        throw 'Resolution must be between 640x360 and 7680x4320.'
    }
    if ($settings.presentMax -notin @('off', '1280x720', '1920x1080')) { throw 'Choose a final composite size.' }
    if ($settings.quality -notin @('low', 'medium', 'high', 'extreme')) { throw 'Choose a graphics quality.' }
    if ($settings.aa -notin @('off', 'msaa')) { throw 'Choose an anti-aliasing option.' }
    if ([int]$settings.anisotropic -notin @(0, 2, 4, 8, 16)) { throw 'Anisotropic filtering must be 0, 2, 4, 8, or 16.' }
    if ([int]$settings.frameLimit -notin @(0, 30, 60)) { throw 'Frame limit must be 0, 30, or 60.' }
    if ([int]$settings.streamBudgetMB -lt 1000 -or [int]$settings.streamBudgetMB -gt 20000) { throw 'Streaming budget must be 1000-20000 MB.' }
    if ([int]$settings.streamLocalPercent -lt 10 -or [int]$settings.streamLocalPercent -gt 95) { throw 'Local VRAM threshold must be 10-95%.' }
    if ([int]$settings.streamMaxEdge -notin @(0, 256, 512, 1024, 2048)) { throw 'Choose a texture edge limit.' }
    if ([int]$settings.rootSystemMB -lt 3750 -or [int]$settings.rootSystemMB -gt 5000) {
        throw 'Root buffer system memory limit must be 3750-5000 MB.'
    }
    if ([int]$settings.rootPressureTrimMB -ne 0 -and
        ([int]$settings.rootPressureTrimMB -lt 256 -or [int]$settings.rootPressureTrimMB -gt 1536)) {
        throw 'Root pressure trim must be 0 or 256-1536 MB.'
    }
    if ([double]$settings.ffxExposure -lt 0.25 -or [double]$settings.ffxExposure -gt 64) { throw 'Exposure must be 0.25-64.' }
    if ($settings.logLevel -notin @('info', 'debug')) { throw 'Choose info or debug logging.' }
    $shaderHashes = @([string]$settings.diagShaderHashes -split ',' | ForEach-Object { $_.Trim().ToLowerInvariant() } | Where-Object { $_ })
    if (@($shaderHashes | Where-Object { $_ -cnotmatch '^[0-9a-f]{16}$' }).Count) {
        throw 'Shader hashes must be comma-separated 16-digit hexadecimal values.'
    }
    if (($settings.diagShaderTrace -or $settings.diagDumpShaderSource) -and -not $shaderHashes.Count) {
        throw 'Enter at least one shader hash to trace or dump shader sources.'
    }
    if ($settings.diagCaptureFullRes -and -not ($settings.diagFrameCapture -or $settings.diagWorldCapture)) {
        throw 'Full-resolution capture requires selected screen or world frame capture.'
    }
    $frameLists = @()
    if ($settings.diagFrameCapture -or $settings.diagSpriteTargets) { $frameLists += ,@('Frame numbers', [string]$settings.diagFrameNumbers) }
    if ($settings.diagWorldCapture) { $frameLists += ,@('World offsets', [string]$settings.diagWorldOffsets) }
    foreach ($item in $frameLists) {
        if ($item[1] -notmatch '^\d{1,6}(\s*,\s*\d{1,6})*$') { throw "$($item[0]) must be a comma-separated list of nonnegative integers." }
        foreach ($number in ($item[1] -split ',')) {
            if ([int]$number.Trim() -gt 100000) { throw "$($item[0]) must be at most 100000." }
        }
    }
    $needsDiagnostics = [bool]($settings.diagFrameCapture -or $settings.diagWorldCapture -or $settings.diagCaptureFullRes -or
        $settings.diagTextureTrace -or $settings.diagShaderTrace -or $settings.diagDumpShaderSource -or
        $settings.diagSpriteTargets -or $settings.diagCoverage -or $settings.diagGpuTiming -or
        $settings.diagD3dDebug -or $settings.diagDred)
    if ($needsDiagnostics -and -not $settings.rebuild) {
        throw 'Debugging switches require Build latest loader before launch, so the diagnostics feature is included.'
    }
    if ([string]::IsNullOrWhiteSpace([string]$settings.title)) { throw 'Choose the game executable.' }
    $title = Assert-LocalFixedPath ([string]$settings.title) 'Game executable'
    if (-not (Test-Path -LiteralPath $title -PathType Leaf) -or [IO.Path]::GetExtension($title) -ine '.exe') {
        throw 'Choose the game executable.'
    }
    $ffx = $null
    if ($settings.useFidelityFx) {
        if ([string]::IsNullOrWhiteSpace([string]$settings.fidelityfx)) { throw 'Choose a PC FidelityFX DLL folder.' }
        $ffx = Assert-LocalFixedPath ([string]$settings.fidelityfx) 'FidelityFX folder'
        foreach ($dll in @('amd_fidelityfx_loader_dx12.dll', 'amd_fidelityfx_upscaler_dx12.dll')) {
            if (-not (Test-Path -LiteralPath (Join-Path $ffx $dll) -PathType Leaf)) {
                throw "FidelityFX folder is missing $dll."
            }
        }
    }
    $exposure = ([double]$settings.ffxExposure).ToString([Globalization.CultureInfo]::InvariantCulture)
    $cacheDir = Assert-LocalFixedPath (Join-Path $stateDir 'cache') 'Loader cache folder'
    $dataDir = Assert-LocalFixedPath (Join-Path $stateDir 'data') 'Loader data folder'
    $configPath = Assert-LocalFixedPath (Join-Path $stateDir 'launcher-config.toml') 'Loader config file'
    $envValues = [ordered]@{
        VICTORY_CONFIG = $configPath
        VICTORY_CACHE_DIR = $cacheDir
        VICTORY_DATA_DIR = $dataDir
        VICTORY_GAME_DIR = Split-Path -Parent $title
        VICTORY_WINDOW_MAX = [string]$settings.resolution
        VICTORY_PRESENT_MAX = if ($settings.presentMax -eq 'off') { $null } else { [string]$settings.presentMax }
        VICTORY_LOG = [string]$settings.logLevel
        VICTORY_FIDELITYFX_DIR = $ffx
        VICTORY_STREAM_MB = [string]$settings.streamBudgetMB
        VICTORY_STREAM_LOCAL_PERCENT = [string]$settings.streamLocalPercent
        VICTORY_STREAM_MAX_EDGE = if ([int]$settings.streamMaxEdge -eq 0) { $null } else { [string]$settings.streamMaxEdge }
        VICTORY_ROOT_SYSTEM_MB = [string]$settings.rootSystemMB
        VICTORY_ROOT_PRESSURE_TRIM_MB = if ([int]$settings.rootPressureTrimMB -eq 0) { $null } else { [string]$settings.rootPressureTrimMB }
        VICTORY_ROOT_REUSE_ONLY = if ($settings.rootReuseOnly) { '1' } else { $null }
        VICTORY_STREAM_PRIORITY_BOUND_MIPS = if ($settings.boundPriorityMips) { '1' } else { $null }
        VICTORY_FFX_EXPOSURE = $exposure
        VICTORY_AMBIENT = if ($settings.ambient) { '1' } else { 'off' }
        VICTORY_AMBIENT_SKY = 'off'
        VICTORY_AMBIENT_SHADE = if ($settings.disableShadows) { '0,0.6,0.1,0.45' } else { $null }
        VICTORY_DISABLE_731_COMPOSITE = if ($settings.disable731Composite) { '1' } else { '0' }
        VICTORY_DISABLE_SHADOWS = if ($settings.disableShadows) { '1' } else { '0' }
        VICTORY_RAY_TRACING = if ($settings.rayTracing -and -not $settings.disableShadows) { '1' } else { '0' }
        VICTORY_MOTION_BLUR = if ($settings.motionBlur) { '1' } else { '0' }
        VICTORY_REFLECTIONS = if ($settings.reflections) { '1' } else { '0' }
        VICTORY_VRS = if ($settings.vrs) { '1' } else { '0' }
        VICTORY_VSYNC = if ($settings.vsync) { '1' } else { '0' }
        VICTORY_HDR = if ($settings.hdr) { '1' } else { '0' }
        VICTORY_SHARPENING = if ($settings.sharpening) { '1' } else { '0' }
        VICTORY_PARTICLE_SHADOWS = if ($settings.particleShadows -and -not $settings.disableShadows) { '1' } else { '0' }
        VICTORY_LONG_SHADOWS = if ($settings.longShadows -and -not $settings.disableShadows) { '1' } else { '0' }
        VICTORY_TREE_TESSELLATION = if ($settings.treeTessellation) { '1' } else { '0' }
        VICTORY_FRAME_LIMIT = [string]$settings.frameLimit
        VICTORY_KEYS = if ($settings.autoEnter) { '45:Enter,75:Enter,105:Enter,135:Enter' } else { $null }
        VICTORY_CAPTURE = if ($settings.diagFrameCapture -or $settings.diagWorldCapture -or $settings.diagSpriteTargets -or $settings.diagDumpShaderSource) { '@session/captures' } else { $null }
        VICTORY_CAPTURE_FRAMES = if ($settings.diagFrameCapture -or $settings.diagSpriteTargets) { (([string]$settings.diagFrameNumbers -split ',' | ForEach-Object { $_.Trim() }) -join ',') } else { $null }
        VICTORY_CAPTURE_WORLD = if ($settings.diagWorldCapture) { (([string]$settings.diagWorldOffsets -split ',' | ForEach-Object { $_.Trim() }) -join ',') } else { $null }
        VICTORY_CAPTURE_FULL_RES = if ($settings.diagCaptureFullRes) { '1' } else { $null }
        VICTORY_TRACE_STREAMED_TEXTURES = if ($settings.diagTextureTrace) { '1' } else { $null }
        VICTORY_TRACE_SHADERS = if ($settings.diagShaderTrace) { $shaderHashes -join ',' } else { $null }
        VICTORY_DUMP_SHADER_HASH = if ($settings.diagDumpShaderSource) { $shaderHashes -join ',' } else { $null }
        VICTORY_DUMP_TARGETS = if ($settings.diagSpriteTargets) { '1' } else { $null }
        VICTORY_COVERAGE = if ($settings.diagCoverage) { '1' } else { $null }
        VICTORY_GPU_TIMING = if ($settings.diagGpuTiming) { '1' } else { $null }
        VICTORY_D3D12_DEBUG = if ($settings.diagD3dDebug) { '1' } else { $null }
        VICTORY_D3D12_DRED = if ($settings.diagDred) { '1' } else { $null }
        VICTORY_COOKED_PROFILE_MANIFEST = $cookedManifest
    }
    $arguments = @('run', $title, '--execute', '--console', [string]$settings.console,
        '--resolution', [string]$settings.resolution, '--quality', [string]$settings.quality,
        '--aa', [string]$settings.aa, '--anisotropic', [string]$settings.anisotropic)
    return [pscustomobject]@{ title = $title; fidelityfx = $ffx; resolutionProfile = [string]$settings.resolutionProfile; cookedManifest = $cookedManifest; arguments = $arguments; environment = $envValues; rebuild = [bool]$settings.rebuild; diagnostics = $needsDiagnostics; prebuildShaders = [bool]$settings.prebuildShaders }
}

function Save-LauncherSettings([pscustomobject] $settings) {
    $directory = Assert-LocalFixedPath (Split-Path -Parent $script:SettingsPath) 'Launcher settings folder'
    [void](Assert-LocalFixedPath $script:SettingsPath 'Launcher settings file')
    New-Item -ItemType Directory -Force -Path $directory | Out-Null
    $temporary = Assert-LocalFixedPath (Join-Path $directory ([IO.Path]::GetFileName($script:SettingsPath) + '.' + [guid]::NewGuid().ToString('N') + '.tmp')) 'Temporary settings file'
    $backup = Assert-LocalFixedPath (Join-Path $directory ([IO.Path]::GetFileName($script:SettingsPath) + '.' + [guid]::NewGuid().ToString('N') + '.bak')) 'Settings backup file'
    try {
        $json = $settings | ConvertTo-Json -Depth 4
        [IO.File]::WriteAllText($temporary, $json, [Text.UTF8Encoding]::new($false))
        if (Test-Path -LiteralPath $script:SettingsPath -PathType Leaf) {
            [IO.File]::Replace($temporary, $script:SettingsPath, $backup)
            Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue
        } else {
            [IO.File]::Move($temporary, $script:SettingsPath)
        }
    } finally {
        if (Test-Path -LiteralPath $temporary -PathType Leaf) { Remove-Item -LiteralPath $temporary -Force }
    }
}

function Find-GameFiles([string] $root) {
    $root = Assert-LocalFixedPath $root 'Game folder'
    if (-not (Test-Path -LiteralPath $root -PathType Container)) { throw "Game folder does not exist: $root" }
    $pending = [Collections.Generic.Queue[string]]::new()
    $pending.Enqueue($root)
    $games = [Collections.Generic.List[string]]::new()
    $ffxFolders = [Collections.Generic.List[string]]::new()
    $scanned = 0
    while ($pending.Count) {
        if (++$scanned -gt 20000) { throw 'Game folder search exceeded 20,000 subfolders. Choose a narrower folder.' }
        $folder = $pending.Dequeue()
        $dlls = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        try { $entries = @(Get-ChildItem -LiteralPath $folder -Force -ErrorAction Stop) }
        catch { continue }
        foreach ($entry in $entries) {
            if (($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { continue }
            if ($entry.PSIsContainer) {
                $pending.Enqueue($entry.FullName)
            } elseif ($entry.Name -ieq 'game_scarlett_master.exe') {
                $games.Add($entry.FullName)
            } elseif ($entry.Name -ieq 'amd_fidelityfx_loader_dx12.dll' -or
                      $entry.Name -ieq 'amd_fidelityfx_upscaler_dx12.dll') {
                [void]$dlls.Add($entry.Name)
            }
        }
        if ($dlls.Contains('amd_fidelityfx_loader_dx12.dll') -and
            $dlls.Contains('amd_fidelityfx_upscaler_dx12.dll')) {
            $ffxFolders.Add($folder)
        }
    }
    return [pscustomobject]@{
        gameExecutables = @($games | Sort-Object)
        fidelityFxFolders = @($ffxFolders | Sort-Object)
        scannedFolders = $scanned
    }
}

function Test-PcFidelityFxFolder([string] $folder) {
    if (-not $folder) { return $false }
    try { $folder = Assert-LocalFixedPath $folder 'FidelityFX folder' }
    catch { return $false }
    return (Test-Path -LiteralPath (Join-Path $folder 'amd_fidelityfx_loader_dx12.dll') -PathType Leaf) -and
        (Test-Path -LiteralPath (Join-Path $folder 'amd_fidelityfx_upscaler_dx12.dll') -PathType Leaf)
}

function Invoke-CookedProfileValidation([pscustomobject] $plan, [string] $outputDirectory) {
    if (-not $plan.cookedManifest) { throw 'Enable a 720p or 1080p verified cooked texture profile first.' }
    $validator = Join-Path $PSScriptRoot 'build-runtime-texture-profile.py'
    if (-not (Test-Path -LiteralPath $validator -PathType Leaf)) {
        throw 'Cooked profile validator is missing. Disable verified cooked textures or install the profile builder.'
    }
    $pythonCommand = Get-Command python.exe -ErrorAction SilentlyContinue
    if (-not $pythonCommand) { throw 'Python is required to validate a cooked texture profile before launch.' }
    $outputDirectory = Assert-LocalFixedPath $outputDirectory 'Cooked validation log folder'
    New-Item -ItemType Directory -Force -Path $outputDirectory | Out-Null
    $outLog = Join-Path $outputDirectory 'cooked-validate-out.log'
    $errLog = Join-Path $outputDirectory 'cooked-validate-err.log'
    $validated = Start-Process -FilePath $pythonCommand.Source `
        -ArgumentList @(('"' + $validator + '"'), '--validate-manifest', ('"' + $plan.cookedManifest + '"'), '--profile', $plan.resolutionProfile) `
        -WorkingDirectory $repo -Wait -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput $outLog -RedirectStandardError $errLog
    if ($validated.ExitCode -ne 0) { throw "Cooked texture manifest validation failed. See $errLog" }
    return (Get-Content -LiteralPath $outLog -Raw).Trim()
}

function Resolve-CookInput([string] $value, [string] $label, [string] $kind) {
    if ([string]::IsNullOrWhiteSpace($value)) { throw "Choose $label." }
    $path = Assert-LocalFixedPath $value $label
    if (-not (Test-Path -LiteralPath $path)) { throw "$label does not exist: $value" }
    if ($kind -eq 'folder' -and -not (Test-Path -LiteralPath $path -PathType Container)) { throw "$label must be a folder." }
    if ($kind -eq 'file' -and -not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "$label must be a file." }
    return $path
}

function New-CookInputPlan([pscustomobject] $settings) {
    if ($settings.resolutionProfile -notin @('720p', '1080p')) {
        throw 'Select a 720p or 1080p resolution profile before cooking verified textures.'
    }
    $manifest = Resolve-CookInput ([string]$settings.cookManifest) 'the classified asset manifest' 'file'
    $inputRoot = Resolve-CookInput ([string]$settings.cookInputRoot) 'the extracted image folder' 'folder'
    $identities = Resolve-CookInput ([string]$settings.cookIdentities) 'the verified title identities JSON' 'file'
    $texconv = Resolve-CookInput ([string]$settings.cookTexconv) 'texconv.exe' 'file'
    $texassemble = Resolve-CookInput ([string]$settings.cookTexassemble) 'texassemble.exe' 'file'
    $outputRoot = Resolve-CookInput ([string]$settings.cookedRoot) 'the cooked output folder' 'folder'
    if ([IO.Path]::GetExtension($manifest) -ine '.json' -or [IO.Path]::GetExtension($identities) -ine '.json' -or
        [IO.Path]::GetExtension($texconv) -ine '.exe' -or [IO.Path]::GetExtension($texassemble) -ine '.exe') {
        throw 'Use JSON files for manifests and identities, and EXE files for Texconv and Texassemble.'
    }
    $comparison = [StringComparison]::OrdinalIgnoreCase
    $separator = [IO.Path]::DirectorySeparatorChar
    if ($outputRoot.Equals($inputRoot, $comparison) -or
        $outputRoot.StartsWith($inputRoot.TrimEnd($separator) + $separator, $comparison) -or
        $inputRoot.StartsWith($outputRoot.TrimEnd($separator) + $separator, $comparison)) {
        throw 'The cooked output folder must be separate from the extracted image folder to preserve originals.'
    }
    foreach ($jsonPath in @($manifest, $identities)) {
        if ((Get-Item -LiteralPath $jsonPath).Length -gt 33554432) { throw "JSON input exceeds 32 MB: $jsonPath" }
    }
    try {
        $classified = Get-Content -LiteralPath $manifest -Raw | ConvertFrom-Json -ErrorAction Stop
        $verified = Get-Content -LiteralPath $identities -Raw | ConvertFrom-Json -ErrorAction Stop
    } catch { throw "Cook inputs must contain valid JSON: $($_.Exception.Message)" }
    if (-not @($classified.assets | Where-Object { $null -ne $_ }).Count) { throw 'Classified asset manifest has no assets.' }
    $eligible = @($verified.assets | Where-Object { $null -ne $_ -and $_.profile -eq $settings.resolutionProfile }).Count
    if ($eligible -lt 1) { throw "Verified identities contain no $($settings.resolutionProfile) assets." }
    return [pscustomobject]@{
        profile = [string]$settings.resolutionProfile; manifest = $manifest; inputRoot = $inputRoot
        identities = $identities; texconv = $texconv; texassemble = $texassemble
        outputRoot = $outputRoot; eligibleAssets = $eligible
    }
}

function Quote-ProcessArgument([string] $value) { return '"' + $value + '"' }

function Invoke-CookStep([string] $python, [string] $script, [string[]] $arguments,
    [string] $name, [string] $logDirectory, $statusLabel) {
    $outLog = Join-Path $logDirectory "$name-out.log"
    $errLog = Join-Path $logDirectory "$name-err.log"
    if ($statusLabel) { $statusLabel.Text = "Running $name...  Logs: $logDirectory" }
    $allArguments = @((Quote-ProcessArgument $script)) + $arguments
    $process = Start-Process -FilePath $python -ArgumentList $allArguments `
        -WorkingDirectory $repo -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput $outLog -RedirectStandardError $errLog
    while (-not $process.HasExited) {
        if ($statusLabel) { [System.Windows.Forms.Application]::DoEvents() }
        Start-Sleep -Milliseconds 200
    }
    $process.WaitForExit()
    if ($process.ExitCode -ne 0) { throw "$name failed. See $errLog" }
    return (Get-Content -LiteralPath $outLog -Raw).Trim()
}

function Invoke-CookPipeline([pscustomobject] $plan, [string] $logDirectory, $statusLabel = $null) {
    $python = Get-Command python.exe -ErrorAction SilentlyContinue
    if (-not $python) { throw 'Python is required for the verified texture cooker.' }
    $cookScript = Join-Path $PSScriptRoot 'cook-textures.py'
    $profileScript = Join-Path $PSScriptRoot 'build-runtime-texture-profile.py'
    if (-not (Test-Path -LiteralPath $cookScript -PathType Leaf) -or -not (Test-Path -LiteralPath $profileScript -PathType Leaf)) {
        throw 'The texture cooker or profile builder script is missing.'
    }
    $logDirectory = Assert-LocalFixedPath $logDirectory 'Cook log folder'
    New-Item -ItemType Directory -Force -Path $logDirectory | Out-Null
    $cookArguments = @('--manifest', (Quote-ProcessArgument $plan.manifest),
        '--input', (Quote-ProcessArgument $plan.inputRoot), '--output', (Quote-ProcessArgument $plan.outputRoot),
        '--texconv', (Quote-ProcessArgument $plan.texconv), '--texassemble', (Quote-ProcessArgument $plan.texassemble))
    [void](Invoke-CookStep -python $python.Source -script $cookScript -arguments $cookArguments -name 'cook' -logDirectory $logDirectory -statusLabel $statusLabel)
    $profileArguments = @('--cooked-root', (Quote-ProcessArgument $plan.outputRoot),
        '--identities', (Quote-ProcessArgument $plan.identities))
    [void](Invoke-CookStep -python $python.Source -script $profileScript -arguments $profileArguments -name 'profile' -logDirectory $logDirectory -statusLabel $statusLabel)
    $runtimeManifest = Join-Path $plan.outputRoot ("runtime_$($plan.profile).toml")
    if (-not (Test-Path -LiteralPath $runtimeManifest -PathType Leaf)) {
        throw "No verified $($plan.profile) runtime manifest was emitted. See $logDirectory"
    }
    $validateArguments = @('--validate-manifest', (Quote-ProcessArgument $runtimeManifest), '--profile', $plan.profile)
    $validated = Invoke-CookStep -python $python.Source -script $profileScript -arguments $validateArguments -name 'validate' -logDirectory $logDirectory -statusLabel $statusLabel
    if ($statusLabel) { $statusLabel.Text = "Validated $($plan.eligibleAssets) eligible assets. Logs: $logDirectory" }
    return [pscustomobject]@{ manifest = $runtimeManifest; eligibleAssets = $plan.eligibleAssets; validation = $validated; logs = $logDirectory }
}

function Initialize-SafeLoaderEnvironment([pscustomobject] $plan, [string] $session) {
    $session = Assert-LocalFixedPath $session 'Launch log folder'
    New-Item -ItemType Directory -Force -Path $session | Out-Null
    $cacheDir = Assert-LocalFixedPath $plan.environment.VICTORY_CACHE_DIR 'Loader cache folder'
    $dataDir = Assert-LocalFixedPath $plan.environment.VICTORY_DATA_DIR 'Loader data folder'
    $configPath = Assert-LocalFixedPath $plan.environment.VICTORY_CONFIG 'Loader config file'
    New-Item -ItemType Directory -Force -Path $stateDir, $cacheDir, $dataDir | Out-Null
    [IO.File]::WriteAllText($configPath, "[paths]`n", [Text.UTF8Encoding]::new($false))
    foreach ($entry in @(Get-ChildItem Env:VICTORY_* -ErrorAction SilentlyContinue)) {
        Remove-Item -LiteralPath "Env:$($entry.Name)" -ErrorAction SilentlyContinue
    }
    foreach ($name in @('VICTORY_CONFIG', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR', 'VICTORY_GAME_DIR')) {
        Set-Item "Env:$name" ([string]$plan.environment[$name])
    }
    $plan.environment.VICTORY_TRAP_LOG = Assert-LocalFixedPath (Join-Path $session 'trap.log') 'Trap log file'
    $plan.environment.VICTORY_NATIVE_LOG_PATH = Assert-LocalFixedPath (Join-Path $session 'native.log') 'Native shim log file'
}

$settings = Read-LauncherSettings
if ($ValidateOnly) {
    New-LaunchPlan $settings | ConvertTo-Json -Depth 5
    exit 0
}
if ($ValidateCookedOnly) {
    $plan = New-LaunchPlan $settings
    $validationDir = Join-Path (Split-Path -Parent $SettingsPath) 'cooked-validation'
    Invoke-CookedProfileValidation $plan $validationDir
    exit 0
}
if ($ValidateCookInputs) {
    New-CookInputPlan $settings | ConvertTo-Json -Depth 4
    exit 0
}
if ($ValidateSafeEnvironment) {
    $safePlan = New-LaunchPlan $settings
    $safeSession = Assert-LocalFixedPath (Join-Path $logRoot ('env-check-' + (Get-Date).ToUniversalTime().ToString('yyyyMMdd-HHmmssZ'))) 'Environment check log folder'
    Initialize-SafeLoaderEnvironment $safePlan $safeSession
    [pscustomobject]@{
        config = $env:VICTORY_CONFIG; cache = $env:VICTORY_CACHE_DIR; data = $env:VICTORY_DATA_DIR
        game = $env:VICTORY_GAME_DIR; configText = Get-Content -LiteralPath $env:VICTORY_CONFIG -Raw
        remaining = @(Get-ChildItem Env:VICTORY_* | Select-Object -ExpandProperty Name)
        session = $safeSession
    } | ConvertTo-Json -Depth 3
    exit 0
}
if ($CookOnly) {
    $cookPlan = New-CookInputPlan $settings
    $cookLog = Join-Path $logRoot ('cook-' + (Get-Date).ToUniversalTime().ToString('yyyyMMdd-HHmmssZ'))
    Invoke-CookPipeline $cookPlan $cookLog | ConvertTo-Json -Depth 4
    exit 0
}

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
[System.Windows.Forms.Application]::EnableVisualStyles()
$script:cookInputs = [pscustomobject]@{
    cookManifest = [string]$settings.cookManifest
    cookInputRoot = [string]$settings.cookInputRoot
    cookIdentities = [string]$settings.cookIdentities
    cookTexconv = [string]$settings.cookTexconv
    cookTexassemble = [string]$settings.cookTexassemble
}

function New-Panel {
    $panel = New-Object System.Windows.Forms.TableLayoutPanel
    $panel.Dock = 'Fill'
    $panel.AutoScroll = $true
    $panel.ColumnCount = 2
    $panel.ColumnStyles.Add([System.Windows.Forms.ColumnStyle]::new([System.Windows.Forms.SizeType]::Absolute, 265)) | Out-Null
    $panel.ColumnStyles.Add([System.Windows.Forms.ColumnStyle]::new([System.Windows.Forms.SizeType]::Percent, 100)) | Out-Null
    $panel.Padding = New-Object System.Windows.Forms.Padding(16)
    return $panel
}

function Add-Row($panel, [string] $caption, $control, [int] $height = 43) {
    $row = $panel.RowCount
    $panel.RowCount = $row + 1
    $panel.RowStyles.Add([System.Windows.Forms.RowStyle]::new([System.Windows.Forms.SizeType]::Absolute, $height)) | Out-Null
    $label = New-Object System.Windows.Forms.Label
    $label.Text = $caption
    $label.AutoSize = $true
    $label.Anchor = 'Left'
    $panel.Controls.Add($label, 0, $row)
    $control.Anchor = 'Left'
    $panel.Controls.Add($control, 1, $row)
}

function New-Combo([string[]] $items, [string] $selected) {
    $combo = New-Object System.Windows.Forms.ComboBox
    $combo.DropDownStyle = 'DropDownList'
    $combo.Width = 240
    [void]$combo.Items.AddRange($items)
    $combo.SelectedItem = $selected
    if ($combo.SelectedIndex -lt 0) { $combo.SelectedIndex = 0 }
    return $combo
}

function New-Check([bool] $checked) {
    $check = New-Object System.Windows.Forms.CheckBox
    $check.Checked = $checked
    $check.AutoSize = $true
    return $check
}

function New-Number([decimal] $min, [decimal] $max, [decimal] $value, [decimal] $increment = 1) {
    $number = New-Object System.Windows.Forms.NumericUpDown
    $number.Minimum = $min
    $number.Maximum = $max
    $number.Increment = $increment
    $number.Width = 130
    $number.Value = [Math]::Min($max, [Math]::Max($min, $value))
    return $number
}

function Add-PathPickerRow($panel, [string] $caption, [string] $value, [bool] $folder, [string] $filter = '') {
    $box = New-Object System.Windows.Forms.TextBox
    $box.Text = $value
    $box.Width = 390
    $browse = New-Object System.Windows.Forms.Button
    $browse.Text = 'Browse...'
    $browse.Width = 82
    $flow = New-Object System.Windows.Forms.FlowLayoutPanel
    $flow.AutoSize = $true
    $flow.Controls.AddRange(@($box, $browse))
    Add-Row $panel $caption $flow
    $browse.Add_Click({
        if ($folder) {
            $picker = New-Object System.Windows.Forms.FolderBrowserDialog
            if ($picker.ShowDialog() -eq 'OK') { $box.Text = $picker.SelectedPath }
        } else {
            $picker = New-Object System.Windows.Forms.OpenFileDialog
            $picker.Filter = $filter
            if ($picker.ShowDialog() -eq 'OK') { $box.Text = $picker.FileName }
        }
    }.GetNewClosure())
    return $box
}

function New-IntegerSlider([int] $min, [int] $max, [int] $value, [int] $tick) {
    $number = New-Number $min $max $value
    $track = New-Object System.Windows.Forms.TrackBar
    $track.AutoSize = $false
    $track.Width = 260
    $track.Height = 39
    $track.Minimum = $min
    $track.Maximum = $max
    $track.TickFrequency = $tick
    $track.Value = [int]$number.Value
    $flow = New-Object System.Windows.Forms.FlowLayoutPanel
    $flow.AutoSize = $true
    $flow.WrapContents = $false
    $flow.Controls.AddRange(@($track, $number))
    $track.Add_ValueChanged({ $number.Value = $track.Value }.GetNewClosure())
    $number.Add_ValueChanged({ $track.Value = [int]$number.Value }.GetNewClosure())
    return [pscustomobject]@{ Panel = $flow; Track = $track; Number = $number }
}

$form = New-Object System.Windows.Forms.Form
$form.Text = 'VIctory - Offline Game Launcher'
$form.StartPosition = 'CenterScreen'
$form.Size = New-Object System.Drawing.Size(850, 720)
$form.MinimumSize = New-Object System.Drawing.Size(740, 620)
$form.Font = New-Object System.Drawing.Font('Segoe UI', 10)
$tabs = New-Object System.Windows.Forms.TabControl
$tabs.Dock = 'Fill'
$gameTab = New-Object System.Windows.Forms.TabPage('Game')
$graphicsTab = New-Object System.Windows.Forms.TabPage('Graphics')
$performanceTab = New-Object System.Windows.Forms.TabPage('Performance')
$debugTab = New-Object System.Windows.Forms.TabPage('Debugging')
$tabs.TabPages.AddRange(@($gameTab, $graphicsTab, $performanceTab, $debugTab))
$gamePanel = New-Panel
$graphicsPanel = New-Panel
$performancePanel = New-Panel
$debugPanel = New-Panel
$gameTab.Controls.Add($gamePanel)
$graphicsTab.Controls.Add($graphicsPanel)
$performanceTab.Controls.Add($performancePanel)
$debugTab.Controls.Add($debugPanel)

$initialGameRoot = [string]$settings.gameRoot
if (-not $initialGameRoot -and $settings.title) {
    $initialGameRoot = Split-Path -Parent ([string]$settings.title)
}
if (-not $initialGameRoot) { $initialGameRoot = $repo }
$gameRootBox = Add-PathPickerRow $gamePanel 'Game folder (search here)' $initialGameRoot $true
$findGameFilesButton = New-Object System.Windows.Forms.Button
$findGameFilesButton.Text = 'Find game files in this folder'
$findGameFilesButton.Width = 250
$findGameFilesButton.Height = 32
Add-Row $gamePanel 'Auto find' $findGameFilesButton

$titleBox = New-Object System.Windows.Forms.TextBox
$titleBox.Text = [string]$settings.title
$titleBox.Width = 390
$titleBrowse = New-Object System.Windows.Forms.Button
$titleBrowse.Text = 'Browse...'
$titleBrowse.Width = 82
$titleFlow = New-Object System.Windows.Forms.FlowLayoutPanel
$titleFlow.AutoSize = $true
$titleFlow.Controls.AddRange(@($titleBox, $titleBrowse))
Add-Row $gamePanel 'Game executable' $titleFlow
$titleBrowse.Add_Click({
    $picker = New-Object System.Windows.Forms.OpenFileDialog
    $picker.Filter = 'Executable (*.exe)|*.exe'
    if ($picker.ShowDialog() -eq 'OK') { $titleBox.Text = $picker.FileName }
})

$ffxBox = New-Object System.Windows.Forms.TextBox
$ffxBox.Text = [string]$settings.fidelityfx
$ffxBox.Width = 390
$ffxBrowse = New-Object System.Windows.Forms.Button
$ffxBrowse.Text = 'Browse...'
$ffxBrowse.Width = 82
$ffxFlow = New-Object System.Windows.Forms.FlowLayoutPanel
$ffxFlow.AutoSize = $true
$ffxFlow.Controls.AddRange(@($ffxBox, $ffxBrowse))
Add-Row $gamePanel 'PC FidelityFX DLL folder' $ffxFlow
$ffxBrowse.Add_Click({
    $picker = New-Object System.Windows.Forms.FolderBrowserDialog
    if ($picker.ShowDialog() -eq 'OK') { $ffxBox.Text = $picker.SelectedPath }
})
$useFfx = New-Check ([bool]$settings.useFidelityFx)
Add-Row $gamePanel 'Use PC FidelityFX upscaler' $useFfx
$console = New-Combo @('series-s', 'series-x') ([string]$settings.console)
Add-Row $gamePanel 'Console profile' $console
$gpuLabel = New-Object System.Windows.Forms.Label
$gpuLabel.AutoSize = $true
$gpuLabel.MaximumSize = New-Object System.Drawing.Size(490, 0)
$gpuLabel.Text = 'Adapter unavailable'
try {
    $adapters = @(Get-CimInstance Win32_VideoController -ErrorAction Stop |
        Where-Object { $_.Name -and $_.Name -notmatch 'Microsoft Basic|Remote Display|Virtual Display' })
    $adapter = @($adapters | Where-Object { $_.PNPDeviceID -like 'PCI*' } | Select-Object -First 1)
    if (-not $adapter.Count) { $adapter = @($adapters | Select-Object -First 1) }
    if ($adapter.Count) { $gpuLabel.Text = [string]$adapter[0].Name }
} catch { }
Add-Row $gamePanel 'Detected graphics adapter' $gpuLabel
$resolutionProfile = New-Combo @('720p', '1080p', 'native', 'custom') ([string]$settings.resolutionProfile)
Add-Row $gamePanel 'Resolution profile' $resolutionProfile
$useCooked = New-Check ([bool]$settings.useCookedTextures)
$useCooked.Enabled = [string]$resolutionProfile.SelectedItem -in @('720p', '1080p')
if (-not $useCooked.Enabled) { $useCooked.Checked = $false }
Add-Row $gamePanel 'Use verified cooked textures' $useCooked
$cookedRootBox = New-Object System.Windows.Forms.TextBox
$cookedRootBox.Text = [string]$settings.cookedRoot
$cookedRootBox.Width = 390
$cookedBrowse = New-Object System.Windows.Forms.Button
$cookedBrowse.Text = 'Browse...'
$cookedBrowse.Width = 82
$cookedFlow = New-Object System.Windows.Forms.FlowLayoutPanel
$cookedFlow.AutoSize = $true
$cookedFlow.Controls.AddRange(@($cookedRootBox, $cookedBrowse))
Add-Row $gamePanel 'Cooked texture output folder' $cookedFlow
$cookedBrowse.Add_Click({
    $picker = New-Object System.Windows.Forms.FolderBrowserDialog
    if ($picker.ShowDialog() -eq 'OK') { $cookedRootBox.Text = $picker.SelectedPath }
})
$cookedManifestLabel = New-Object System.Windows.Forms.Label
$cookedManifestLabel.AutoSize = $true
$cookedManifestLabel.MaximumSize = New-Object System.Drawing.Size(490, 0)
Add-Row $gamePanel 'Selected cooked manifest' $cookedManifestLabel 62
$updateCookedManifest = {
    $profile = [string]$resolutionProfile.SelectedItem
    if (-not $useCooked.Checked -or $profile -notin @('720p', '1080p')) {
        $cookedManifestLabel.Text = 'Off: original title textures and runtime streaming'
    } elseif ($cookedRootBox.Text.Trim()) {
        $cookedManifestLabel.Text = Join-Path $cookedRootBox.Text.Trim() ("runtime_$profile.toml")
    } else {
        $cookedManifestLabel.Text = 'Choose a cooked output folder'
    }
}
$useCooked.Add_CheckedChanged($updateCookedManifest)
$cookedRootBox.Add_TextChanged($updateCookedManifest)
$resolutionProfile.Add_SelectedIndexChanged($updateCookedManifest)
& $updateCookedManifest
$cookButton = New-Object System.Windows.Forms.Button
$cookButton.Text = 'Cook verified textures...'
$cookButton.Width = 210
$cookButton.Height = 32
Add-Row $gamePanel 'One-time asset build' $cookButton
$rebuild = New-Check ([bool]$settings.rebuild)
Add-Row $gamePanel 'Build latest loader before launch' $rebuild
$prebuildShaders = New-Check ([bool]$settings.prebuildShaders)
Add-Row $gamePanel 'Prebuild shaders for this GPU once' $prebuildShaders
$autoEnter = New-Check ([bool]$settings.autoEnter)
Add-Row $gamePanel 'Auto Enter through opening menus' $autoEnter
$offline = New-Object System.Windows.Forms.Label
$offline.Text = 'Required: inbound, outbound, and loopback blocked before launch.'
$offline.AutoSize = $true
Add-Row $gamePanel 'Network isolation' $offline

$resolution = New-Combo @('1280x720', '1600x900', '1920x1080', '2560x1440') ([string]$settings.resolution)
$resolution.DropDownStyle = 'DropDown'
$resolution.Text = [string]$settings.resolution
Add-Row $graphicsPanel 'Resolution' $resolution
$quality = New-Combo @('low', 'medium', 'high', 'extreme') ([string]$settings.quality)
Add-Row $graphicsPanel 'Graphics / shader preset' $quality
$aa = New-Combo @('off', 'msaa') ([string]$settings.aa)
Add-Row $graphicsPanel 'Anti-aliasing' $aa
$anisotropic = New-Combo @('0', '2', '4', '8', '16') ([string]$settings.anisotropic)
Add-Row $graphicsPanel 'Anisotropic filtering' $anisotropic
$disableShadows = New-Check ([bool]$settings.disableShadows)
Add-Row $graphicsPanel 'Disable title shadows' $disableShadows
$rayTracing = New-Check ([bool]$settings.rayTracing)
Add-Row $graphicsPanel 'Ray tracing (experimental)' $rayTracing
$motionBlur = New-Check ([bool]$settings.motionBlur)
Add-Row $graphicsPanel 'Motion blur' $motionBlur
$reflections = New-Check ([bool]$settings.reflections)
Add-Row $graphicsPanel 'Screen-space / water reflections' $reflections
$vrs = New-Check ([bool]$settings.vrs)
Add-Row $graphicsPanel 'Variable rate shading' $vrs
$vsync = New-Check ([bool]$settings.vsync)
Add-Row $graphicsPanel 'VSync' $vsync
$hdr = New-Check ([bool]$settings.hdr)
Add-Row $graphicsPanel 'HDR' $hdr
$sharpening = New-Check ([bool]$settings.sharpening)
Add-Row $graphicsPanel 'Sharpening' $sharpening
$particleShadows = New-Check ([bool]$settings.particleShadows)
Add-Row $graphicsPanel 'Particle shadows' $particleShadows
$longShadows = New-Check ([bool]$settings.longShadows)
Add-Row $graphicsPanel 'Long shadows' $longShadows
$shadowControls = @($rayTracing, $particleShadows, $longShadows)
foreach ($control in $shadowControls) { $control.Enabled = -not $disableShadows.Checked }
$disableShadows.Add_CheckedChanged({
    foreach ($control in $shadowControls) { $control.Enabled = -not $disableShadows.Checked }
})
$treeTessellation = New-Check ([bool]$settings.treeTessellation)
Add-Row $graphicsPanel 'Tree tessellation' $treeTessellation
$frameLimit = New-Combo @('30', '60', '0') ([string]$settings.frameLimit)
Add-Row $graphicsPanel 'Frame limit (0 = uncapped)' $frameLimit
$ambient = New-Check ([bool]$settings.ambient)
Add-Row $graphicsPanel 'Experimental ambient light' $ambient

$streamBudget = New-Number 1000 20000 ([decimal]$settings.streamBudgetMB) 256
Add-Row $performancePanel 'Combined stream cap (MB)' $streamBudget
$presentMax = New-Combo @('off', '1280x720', '1920x1080') ([string]$settings.presentMax)
Add-Row $performancePanel 'Final composite size' $presentMax
$localPercentSlider = New-IntegerSlider 10 95 ([int]$settings.streamLocalPercent) 5
$localPercent = $localPercentSlider.Number
Add-Row $performancePanel 'Local VRAM threshold (%)' $localPercentSlider.Panel
$maxEdge = New-Combo @('256', '512', '1024', '2048', '0') ([string]$settings.streamMaxEdge)
Add-Row $performancePanel 'Texture edge limit (0 = none)' $maxEdge
$script:applyingProfile = $false
$resolutionProfile.Add_SelectedIndexChanged({
    if ($script:applyingProfile) { return }
    $script:applyingProfile = $true
    try {
        $useCooked.Enabled = [string]$resolutionProfile.SelectedItem -in @('720p', '1080p')
        if (-not $useCooked.Enabled) { $useCooked.Checked = $false }
        switch ([string]$resolutionProfile.SelectedItem) {
            '720p' {
                $resolution.Text = '1280x720'; $presentMax.SelectedItem = '1280x720'; $maxEdge.SelectedItem = '512'
                $prebuildShaders.Checked = $true
            }
            '1080p' {
                $resolution.Text = '1920x1080'; $presentMax.SelectedItem = '1920x1080'; $maxEdge.SelectedItem = '1024'
                $prebuildShaders.Checked = $true
            }
            'native' {
                $screen = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
                $resolution.Text = "$($screen.Width)x$($screen.Height)"
                $presentMax.SelectedItem = 'off'; $maxEdge.SelectedItem = '0'
                $prebuildShaders.Checked = $true
            }
        }
    } finally { $script:applyingProfile = $false }
})
$markCustom = {
    if (-not $script:applyingProfile -and [string]$resolutionProfile.SelectedItem -ne 'custom') {
        $resolutionProfile.SelectedItem = 'custom'
    }
}
$resolution.Add_TextChanged($markCustom)
$presentMax.Add_SelectedIndexChanged($markCustom)
$maxEdge.Add_SelectedIndexChanged($markCustom)
if ([string]$settings.resolutionProfile -eq 'native') {
    $screen = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
    $nativeSize = "$($screen.Width)x$($screen.Height)"
    $script:applyingProfile = $true
    try {
        if ($resolution.Text -ne $nativeSize) { $prebuildShaders.Checked = $true }
        $resolution.Text = $nativeSize
        $presentMax.SelectedItem = 'off'
        $maxEdge.SelectedItem = '0'
    } finally { $script:applyingProfile = $false }
}
$rootSystemSlider = New-IntegerSlider 3750 5000 ([int]$settings.rootSystemMB) 250
$rootSystem = $rootSystemSlider.Number
Add-Row $performancePanel 'Root buffer RAM limit (MB)' $rootSystemSlider.Panel
$rootTrim = New-Number 0 1536 ([decimal]$settings.rootPressureTrimMB) 256
Add-Row $performancePanel 'Root pressure trim (MB)' $rootTrim
$rootReuse = New-Check ([bool]$settings.rootReuseOnly)
Add-Row $performancePanel 'Reuse resident root buffer space' $rootReuse
$boundMips = New-Check ([bool]$settings.boundPriorityMips)
Add-Row $performancePanel 'Prioritize bound texture mips' $boundMips
$exposure = New-Number 0.25 64 ([decimal]$settings.ffxExposure) 0.25
$exposure.DecimalPlaces = 2
Add-Row $performancePanel 'FidelityFX exposure' $exposure
$logLevel = New-Combo @('info', 'debug') ([string]$settings.logLevel)
Add-Row $debugPanel 'Local log detail' $logLevel
$diagFrameCapture = New-Check ([bool]$settings.diagFrameCapture)
Add-Row $debugPanel 'Capture selected screen frames' $diagFrameCapture
$diagWorldCapture = New-Check ([bool]$settings.diagWorldCapture)
Add-Row $debugPanel 'Capture world-relative frames' $diagWorldCapture
$diagCaptureFullRes = New-Check ([bool]$settings.diagCaptureFullRes)
Add-Row $debugPanel 'Full-resolution screen capture' $diagCaptureFullRes
$updateCaptureSize = {
    $diagCaptureFullRes.Enabled = $diagFrameCapture.Checked -or $diagWorldCapture.Checked
    if (-not $diagCaptureFullRes.Enabled) { $diagCaptureFullRes.Checked = $false }
}
$diagFrameCapture.Add_CheckedChanged($updateCaptureSize)
$diagWorldCapture.Add_CheckedChanged($updateCaptureSize)
& $updateCaptureSize
$diagFrameNumbers = New-Object System.Windows.Forms.TextBox
$diagFrameNumbers.Text = [string]$settings.diagFrameNumbers
$diagFrameNumbers.Width = 240
Add-Row $debugPanel 'Screen frame numbers' $diagFrameNumbers
$diagWorldOffsets = New-Object System.Windows.Forms.TextBox
$diagWorldOffsets.Text = [string]$settings.diagWorldOffsets
$diagWorldOffsets.Width = 240
Add-Row $debugPanel 'World frame offsets' $diagWorldOffsets
$diagTextureTrace = New-Check ([bool]$settings.diagTextureTrace)
Add-Row $debugPanel 'Trace streamed textures' $diagTextureTrace
$diagShaderTrace = New-Check ([bool]$settings.diagShaderTrace)
Add-Row $debugPanel 'Trace selected shaders' $diagShaderTrace
$diagShaderHashes = New-Object System.Windows.Forms.TextBox
$diagShaderHashes.Text = [string]$settings.diagShaderHashes
$diagShaderHashes.Width = 335
Add-Row $debugPanel 'Shader hashes (16 hex each)' $diagShaderHashes
$diagDumpShaderSource = New-Check ([bool]$settings.diagDumpShaderSource)
Add-Row $debugPanel 'Dump selected shader sources' $diagDumpShaderSource
$diagSpriteTargets = New-Check ([bool]$settings.diagSpriteTargets)
Add-Row $debugPanel 'Sprite / UI render-target capture' $diagSpriteTargets
$diagCoverage = New-Check ([bool]$settings.diagCoverage)
Add-Row $debugPanel 'Shader / texture coverage log' $diagCoverage
$diagGpuTiming = New-Check ([bool]$settings.diagGpuTiming)
Add-Row $debugPanel 'GPU command timing' $diagGpuTiming
$diagD3dDebug = New-Check ([bool]$settings.diagD3dDebug)
Add-Row $debugPanel 'D3D12 debug layer' $diagD3dDebug
$diagDred = New-Check ([bool]$settings.diagDred)
Add-Row $debugPanel 'Device removal diagnostics' $diagDred
$disable731 = New-Check ([bool]$settings.disable731Composite)
Add-Row $debugPanel 'Skip world composite (diagnostic)' $disable731
$cookStatus = New-Object System.Windows.Forms.Label
$cookStatus.AutoSize = $true
$cookStatus.MaximumSize = New-Object System.Drawing.Size(490, 0)
$cookStatus.Text = 'Optional 720p/1080p manifests replace only verified listed textures. Original title assets remain untouched; all other textures use runtime streaming.'
Add-Row $debugPanel 'Offline texture build status' $cookStatus 72

$toolTip = New-Object System.Windows.Forms.ToolTip
$toolTip.SetToolTip($quality, 'This is the title graphics preset. The port has no separate shader compiler quality switch.')
$toolTip.SetToolTip($disableShadows, 'Overrides every exposed title shadow setting, disables ray traversal, and removes depth shading from the optional ambient fallback.')
$toolTip.SetToolTip($rayTracing, 'Off clears both title RT shadow settings and suppresses BVH builder dispatches and native ray-traversal shaders. On is experimental.')
$toolTip.SetToolTip($disable731, 'Skips world lighting. Use only to compare images while diagnosing the renderer.')
$toolTip.SetToolTip($streamBudget, 'Combined local and nonlocal stream cap. The local threshold below protects RX 5700 XT VRAM.')
$toolTip.SetToolTip($presentMax, 'Scales the 1080p scene and 4K overlay into a smaller host swap chain. Does not resize the game render targets.')
$toolTip.SetToolTip($localPercentSlider.Panel, 'Percent of the current DXGI local memory budget the streamer may use. This is the VRAM ceiling; leave some headroom for the driver.')
$toolTip.SetToolTip($rootSystemSlider.Panel, 'Budget for root buffers in system memory, not a limit on all game RAM or a way to use the entire 32 GB.')
$toolTip.SetToolTip($prebuildShaders, 'Translate and cache missing title shaders and host pipelines on this GPU after the offline check. Clears itself after a successful prebuild; the shader cache remains.')
$toolTip.SetToolTip($resolutionProfile, '720p, 1080p, and native fill in render size, final composite size, and streamed texture edge limit. Editing a value switches to custom.')
$toolTip.SetToolTip($gpuLabel, 'Windows reports the first PCI graphics adapter. The D3D12 adapter chosen by the runtime may differ; pipeline cache keys use the adapter actually used.')
$toolTip.SetToolTip($useCooked, 'Opt in only after the offline cooker created runtime_720p.toml or runtime_1080p.toml in the folder below. The exact original source and same-format DDS are validated before launch and again at runtime.')
$toolTip.SetToolTip($cookedRootBox, 'Local output folder from the offline texture cooker. The selected manifest file is runtime_720p.toml or runtime_1080p.toml. Originals stay in title archives.')
$toolTip.SetToolTip($cookButton, 'Cook only explicitly classified extracted images with verified title identities and Texconv/Texassemble. Runs once when clicked, keeps originals, and logs each stage locally.')
$toolTip.SetToolTip($diagTextureTrace, 'Log a bounded sample of streamed large textures and source audits. Requires a diagnostics build.')
$toolTip.SetToolTip($diagShaderTrace, 'Log commands using the shader hashes below. Requires a diagnostics build.')
$toolTip.SetToolTip($diagDumpShaderSource, 'Save the matching title shader containers in this launch session capture folder.')
$toolTip.SetToolTip($diagSpriteTargets, 'Capture all render targets at selected screen frames to inspect UI and sprites. This is not a sprite-only trace and can use substantial disk space.')
$toolTip.SetToolTip($diagFrameNumbers, 'Comma-separated absolute presented frame numbers. Used by screen-frame and render-target capture.')
$toolTip.SetToolTip($diagWorldOffsets, 'Comma-separated offsets from the first world frame. Useful when menu duration varies.')
$toolTip.SetToolTip($diagCaptureFullRes, 'Save selected presented frames at their full resolution. Requires screen or world frame capture and uses more disk space.')
$toolTip.SetToolTip($cookStatus, 'A cooked profile applies only to verified entries in its manifest. It does not rebuild or replace the full game texture set.')

$buttons = New-Object System.Windows.Forms.FlowLayoutPanel
$buttons.Dock = 'Bottom'
$buttons.Height = 54
$buttons.FlowDirection = 'RightToLeft'
$launchButton = New-Object System.Windows.Forms.Button
$launchButton.Text = 'Launch offline'
$launchButton.Width = 140
$launchButton.Height = 34
$saveButton = New-Object System.Windows.Forms.Button
$saveButton.Text = 'Save settings'
$saveButton.Width = 120
$saveButton.Height = 34
$cancelButton = New-Object System.Windows.Forms.Button
$cancelButton.Text = 'Close'
$cancelButton.Width = 90
$cancelButton.Height = 34
$buttons.Controls.AddRange(@($launchButton, $saveButton, $cancelButton))
$form.Controls.Add($tabs)
$form.Controls.Add($buttons)
$form.AcceptButton = $launchButton
$form.CancelButton = $cancelButton

function Read-FormSettings {
    return [pscustomobject]@{
        gameRoot = $gameRootBox.Text.Trim()
        title = $titleBox.Text.Trim(); fidelityfx = $ffxBox.Text.Trim(); useFidelityFx = $useFfx.Checked
        console = [string]$console.SelectedItem; resolutionProfile = [string]$resolutionProfile.SelectedItem
        useCookedTextures = $useCooked.Checked; cookedRoot = $cookedRootBox.Text.Trim()
        cookManifest = $script:cookInputs.cookManifest; cookInputRoot = $script:cookInputs.cookInputRoot
        cookIdentities = $script:cookInputs.cookIdentities; cookTexconv = $script:cookInputs.cookTexconv
        cookTexassemble = $script:cookInputs.cookTexassemble
        resolution = $resolution.Text.Trim()
        presentMax = [string]$presentMax.SelectedItem
        quality = [string]$quality.SelectedItem; aa = [string]$aa.SelectedItem
        anisotropic = [int][string]$anisotropic.SelectedItem; disableShadows = $disableShadows.Checked; rayTracing = $rayTracing.Checked
        motionBlur = $motionBlur.Checked; reflections = $reflections.Checked; vrs = $vrs.Checked
        vsync = $vsync.Checked; hdr = $hdr.Checked; sharpening = $sharpening.Checked
        particleShadows = $particleShadows.Checked; longShadows = $longShadows.Checked
        treeTessellation = $treeTessellation.Checked; frameLimit = [int][string]$frameLimit.SelectedItem
        disable731Composite = $disable731.Checked; ambient = $ambient.Checked
        streamBudgetMB = [int]$streamBudget.Value; streamLocalPercent = [int]$localPercent.Value
        streamMaxEdge = [int][string]$maxEdge.SelectedItem; rootSystemMB = [int]$rootSystem.Value
        rootPressureTrimMB = [int]$rootTrim.Value
        rootReuseOnly = $rootReuse.Checked
        boundPriorityMips = $boundMips.Checked; ffxExposure = [double]$exposure.Value
        autoEnter = $autoEnter.Checked; logLevel = [string]$logLevel.SelectedItem; rebuild = $rebuild.Checked
        prebuildShaders = $prebuildShaders.Checked
        diagFrameCapture = $diagFrameCapture.Checked; diagWorldCapture = $diagWorldCapture.Checked
        diagCaptureFullRes = $diagCaptureFullRes.Checked
        diagFrameNumbers = $diagFrameNumbers.Text.Trim(); diagWorldOffsets = $diagWorldOffsets.Text.Trim()
        diagTextureTrace = $diagTextureTrace.Checked; diagShaderTrace = $diagShaderTrace.Checked
        diagShaderHashes = $diagShaderHashes.Text.Trim(); diagDumpShaderSource = $diagDumpShaderSource.Checked
        diagSpriteTargets = $diagSpriteTargets.Checked; diagCoverage = $diagCoverage.Checked
        diagGpuTiming = $diagGpuTiming.Checked; diagD3dDebug = $diagD3dDebug.Checked; diagDred = $diagDred.Checked
    }
}

$findGameFilesButton.Add_Click({
    try {
        $root = $gameRootBox.Text.Trim()
        $rootAvailable = $false
        if ($root) {
            try {
                $root = Assert-LocalFixedPath $root 'Game folder'
                $rootAvailable = Test-Path -LiteralPath $root -PathType Container
            } catch { $rootAvailable = $false }
        }
        if (-not $rootAvailable) {
            $folderPicker = New-Object System.Windows.Forms.FolderBrowserDialog
            $folderPicker.Description = 'Choose the folder containing the game files and their subfolders.'
            if ($folderPicker.ShowDialog($form) -ne 'OK') { return }
            $root = $folderPicker.SelectedPath
        }
        $found = Find-GameFiles $root
        $gameRootBox.Text = $root
        if ($found.gameExecutables.Count -eq 1) {
            $titleBox.Text = $found.gameExecutables[0]
        } else {
            $picker = New-Object System.Windows.Forms.OpenFileDialog
            $picker.Title = if ($found.gameExecutables.Count) { 'Several game executables found; choose one' } else { 'Game executable not found; choose it manually' }
            $picker.Filter = 'Executable (*.exe)|*.exe'
            $picker.InitialDirectory = $root
            if ($picker.ShowDialog($form) -eq 'OK') {
                $titleBox.Text = Assert-LocalFixedPath $picker.FileName 'Game executable'
            }
        }
        $existingFfxValid = Test-PcFidelityFxFolder $ffxBox.Text.Trim()
        if ($found.fidelityFxFolders.Count -eq 1) {
            $ffxBox.Text = $found.fidelityFxFolders[0]
        } elseif ($useFfx.Checked -and -not $existingFfxValid) {
            $folderPicker = New-Object System.Windows.Forms.FolderBrowserDialog
            $folderPicker.Description = 'Choose the folder containing both PC FidelityFX DX12 DLLs.'
            $folderPicker.SelectedPath = $root
            if ($folderPicker.ShowDialog($form) -eq 'OK') {
                $chosen = Find-GameFiles $folderPicker.SelectedPath
                if ($chosen.fidelityFxFolders.Count -eq 1) {
                    $ffxBox.Text = $chosen.fidelityFxFolders[0]
                } else {
                    [System.Windows.Forms.MessageBox]::Show(
                        "Found $($chosen.fidelityFxFolders.Count) folders with both PC FidelityFX DX12 DLLs. Choose a narrower folder and search again.",
                        'FidelityFX folder needed') | Out-Null
                }
            }
        }
        Save-LauncherSettings (Read-FormSettings)
        [System.Windows.Forms.MessageBox]::Show(
            "Searched $($found.scannedFolders) local folder(s). Found $($found.gameExecutables.Count) game EXE(s) and $($found.fidelityFxFolders.Count) PC FidelityFX folder(s).`nSelected paths were saved to $script:SettingsPath",
            'VIctory game file search') | Out-Null
    } catch {
        [System.Windows.Forms.MessageBox]::Show($_.Exception.Message, 'Game file search failed') | Out-Null
    }
})

function Show-CookDialog([bool] $constructOnly = $false) {
    $dialog = New-Object System.Windows.Forms.Form
    $dialog.Text = 'VIctory - Cook Verified Textures'
    $dialog.StartPosition = 'CenterParent'
    $dialog.Size = New-Object System.Drawing.Size(870, 620)
    $dialog.MinimumSize = New-Object System.Drawing.Size(780, 580)
    $dialog.Font = $form.Font
    $panel = New-Panel
    $profilePicker = New-Combo @('720p', '1080p') ([string]$resolutionProfile.SelectedItem)
    Add-Row $panel 'Profile to build and validate' $profilePicker
    $classifiedBox = Add-PathPickerRow $panel 'Classified asset manifest' $script:cookInputs.cookManifest $false 'JSON files (*.json)|*.json'
    $inputBox = Add-PathPickerRow $panel 'Extracted image folder' $script:cookInputs.cookInputRoot $true
    $identityBox = Add-PathPickerRow $panel 'Verified title identities JSON' $script:cookInputs.cookIdentities $false 'JSON files (*.json)|*.json'
    $texconvPath = [string]$script:cookInputs.cookTexconv
    $texassemblePath = [string]$script:cookInputs.cookTexassemble
    if (-not $texconvPath) {
        $found = Get-Command texconv.exe -ErrorAction SilentlyContinue
        if ($found) {
            $item = Get-Item -LiteralPath $found.Source
            $texconvPath = if ($item.Target) { [string]$item.Target } else { $found.Source }
        }
    }
    if (-not $texassemblePath) {
        $found = Get-Command texassemble.exe -ErrorAction SilentlyContinue
        if ($found) {
            $item = Get-Item -LiteralPath $found.Source
            $texassemblePath = if ($item.Target) { [string]$item.Target } else { $found.Source }
        }
    }
    $texconvBox = Add-PathPickerRow $panel 'Texconv executable' $texconvPath $false 'Executable files (*.exe)|*.exe'
    $texassembleBox = Add-PathPickerRow $panel 'Texassemble executable' $texassemblePath $false 'Executable files (*.exe)|*.exe'
    $outputBox = Add-PathPickerRow $panel 'Separate cooked output folder' $cookedRootBox.Text $true
    $note = New-Object System.Windows.Forms.Label
    $note.AutoSize = $true
    $note.MaximumSize = New-Object System.Drawing.Size(520, 0)
    $note.Text = 'Only explicitly classified extracted images with verified title mip identities are eligible. The original images and game archives stay untouched.'
    Add-Row $panel 'Required source evidence' $note 65
    $status = New-Object System.Windows.Forms.Label
    $status.AutoSize = $true
    $status.MaximumSize = New-Object System.Drawing.Size(520, 0)
    $status.Text = 'Choose all inputs, then cook and validate. Each stage writes local logs.'
    Add-Row $panel 'Status' $status 65
    $buttons = New-Object System.Windows.Forms.FlowLayoutPanel
    $buttons.Dock = 'Bottom'
    $buttons.Height = 52
    $buttons.FlowDirection = 'RightToLeft'
    $runCook = New-Object System.Windows.Forms.Button
    $runCook.Text = 'Cook and validate'
    $runCook.Width = 160
    $runCook.Height = 34
    $cancelCook = New-Object System.Windows.Forms.Button
    $cancelCook.Text = 'Cancel'
    $cancelCook.Width = 90
    $cancelCook.Height = 34
    $buttons.Controls.AddRange(@($runCook, $cancelCook))
    $dialog.Controls.Add($panel)
    $dialog.Controls.Add($buttons)
    $dialog.AcceptButton = $runCook
    $dialog.CancelButton = $cancelCook
    $cancelCook.Add_Click({ $dialog.DialogResult = 'Cancel'; $dialog.Close() }.GetNewClosure())
    $runCook.Add_Click({
        try {
            $candidate = [pscustomobject]@{
                resolutionProfile = [string]$profilePicker.SelectedItem
                cookedRoot = $outputBox.Text.Trim()
                cookManifest = $classifiedBox.Text.Trim()
                cookInputRoot = $inputBox.Text.Trim()
                cookIdentities = $identityBox.Text.Trim()
                cookTexconv = $texconvBox.Text.Trim()
                cookTexassemble = $texassembleBox.Text.Trim()
            }
            $cookPlan = New-CookInputPlan $candidate
            $status.Text = "$($cookPlan.eligibleAssets) verified asset(s) eligible for $($cookPlan.profile). Cooking..."
            $runCook.Enabled = $false
            $cancelCook.Enabled = $false
            $dialog.ControlBox = $false
            $cookLog = Assert-LocalFixedPath (Join-Path $logRoot ('cook-' + (Get-Date).ToUniversalTime().ToString('yyyyMMdd-HHmmssZ'))) 'Cook log folder'
            $result = Invoke-CookPipeline $cookPlan $cookLog $status
            $script:cookInputs = [pscustomobject]@{
                cookManifest = $cookPlan.manifest; cookInputRoot = $cookPlan.inputRoot
                cookIdentities = $cookPlan.identities; cookTexconv = $cookPlan.texconv
                cookTexassemble = $cookPlan.texassemble
            }
            $cookedRootBox.Text = $cookPlan.outputRoot
            $resolutionProfile.SelectedItem = $cookPlan.profile
            $useCooked.Checked = $true
            Save-LauncherSettings (Read-FormSettings)
            $dialog.Tag = $result
            $dialog.DialogResult = 'OK'
            $dialog.Close()
        } catch {
            $status.Text = $_.Exception.Message
            [System.Windows.Forms.MessageBox]::Show($_.Exception.Message, 'Texture cook failed') | Out-Null
        } finally {
            $runCook.Enabled = $true
            $cancelCook.Enabled = $true
            $dialog.ControlBox = $true
        }
    }.GetNewClosure())
    if ($constructOnly) {
        $constructed = [pscustomobject]@{
            rows = $panel.RowCount
            profile = [string]$profilePicker.SelectedItem
            pathFields = @('classified manifest', 'extracted images', 'verified identities', 'texconv', 'texassemble', 'separate output')
        }
        $dialog.Dispose()
        return $constructed
    }
    $outcome = $dialog.ShowDialog($form)
    if ($outcome -eq 'OK' -and $dialog.Tag) {
        $result = $dialog.Tag
        [System.Windows.Forms.MessageBox]::Show(
            "Cooked $($result.eligibleAssets) eligible asset(s).`n$($result.validation)`nManifest: $($result.manifest)`nLogs: $($result.logs)",
            'Texture profile ready') | Out-Null
    }
    $dialog.Dispose()
}

$cookButton.Add_Click({ Show-CookDialog })

if ($ValidateUi) {
    if ($UiProfile) {
        if ($UiProfile -notin @('720p', '1080p', 'native', 'custom')) { throw 'Choose a valid UI test profile.' }
        $resolutionProfile.SelectedItem = $UiProfile
    }
    $formSettings = Read-FormSettings
    $formPlan = New-LaunchPlan $formSettings
    [pscustomobject]@{
        tabs = @($tabs.TabPages | ForEach-Object { $_.Text })
        gameRows = $gamePanel.RowCount
        graphicsRows = $graphicsPanel.RowCount
        performanceRows = $performancePanel.RowCount
        debuggingRows = $debugPanel.RowCount
        detectedGpu = $gpuLabel.Text
        selectedCookedManifest = $cookedManifestLabel.Text
        autoFindButton = $findGameFilesButton.Text
        cookDialog = Show-CookDialog $true
        vramSlider = [pscustomobject]@{ min = $localPercentSlider.Track.Minimum; max = $localPercentSlider.Track.Maximum; value = $localPercentSlider.Track.Value }
        systemRamSlider = [pscustomobject]@{ min = $rootSystemSlider.Track.Minimum; max = $rootSystemSlider.Track.Maximum; value = $rootSystemSlider.Track.Value }
        settings = $formSettings
        environment = $formPlan.environment
    } | ConvertTo-Json -Depth 5
    exit 0
}

$saveButton.Add_Click({
    try {
        $candidate = Read-FormSettings
        Save-LauncherSettings $candidate
        [System.Windows.Forms.MessageBox]::Show("Settings saved to $script:SettingsPath", 'VIctory') | Out-Null
    } catch {
        [System.Windows.Forms.MessageBox]::Show($_.Exception.Message, 'Settings error') | Out-Null
    }
})
$cancelButton.Add_Click({ $form.DialogResult = 'Cancel'; $form.Close() })
$form.Add_FormClosing({
    param($sender, $eventArgs)
    try {
        Save-LauncherSettings (Read-FormSettings)
    } catch {
        [System.Windows.Forms.MessageBox]::Show($_.Exception.Message, 'Settings were not saved') | Out-Null
    }
})
$form.Add_Shown({
    $missingTitle = $true
    if ($titleBox.Text.Trim()) {
        try {
            $selectedTitle = Assert-LocalFixedPath $titleBox.Text.Trim() 'Game executable'
            $missingTitle = -not (Test-Path -LiteralPath $selectedTitle -PathType Leaf)
        } catch { $missingTitle = $true }
    }
    if ($missingTitle -or ($useFfx.Checked -and -not (Test-PcFidelityFxFolder $ffxBox.Text.Trim()))) {
        $findGameFilesButton.PerformClick()
    }
})
$launchButton.Add_Click({
    try {
        $candidate = Read-FormSettings
        $script:plan = New-LaunchPlan $candidate
        $script:launchSettings = $candidate
        Save-LauncherSettings $candidate
        $form.DialogResult = 'OK'
        $form.Close()
    } catch {
        [System.Windows.Forms.MessageBox]::Show($_.Exception.Message, 'Launch settings error') | Out-Null
    }
})

if ($form.ShowDialog() -ne 'OK') { exit 0 }

try {
    $stamp = (Get-Date).ToUniversalTime().ToString('yyyyMMdd-HHmmssZ')
    $session = Assert-LocalFixedPath (Join-Path $logRoot $stamp) 'Launch log folder'
    Initialize-SafeLoaderEnvironment $plan $session
    if ($plan.environment.VICTORY_CAPTURE -eq '@session/captures') {
        $plan.environment.VICTORY_CAPTURE = Join-Path $session 'captures'
        New-Item -ItemType Directory -Force -Path $plan.environment.VICTORY_CAPTURE | Out-Null
    }
    $plan | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $session 'launch.json') -Encoding UTF8
    if ($plan.cookedManifest) { [void](Invoke-CookedProfileValidation $plan $session) }
    if ($plan.rebuild) {
        $cargoCommand = Get-Command cargo.exe -ErrorAction SilentlyContinue
        $cargo = if ($cargoCommand) { $cargoCommand.Source } else { Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe' }
        if (-not (Test-Path -LiteralPath $cargo -PathType Leaf)) { throw 'cargo.exe was not found.' }
        $buildArgs = @('build', '--offline', '--release', '-p', 'victory')
        if ($plan.diagnostics) { $buildArgs += @('--features', 'diagnostics') }
        $build = Start-Process -FilePath $cargo -ArgumentList $buildArgs `
            -WorkingDirectory $repo -Wait -PassThru -WindowStyle Hidden `
            -RedirectStandardOutput (Join-Path $session 'build-out.log') -RedirectStandardError (Join-Path $session 'build-err.log')
        if ($build.ExitCode -ne 0) { throw "Offline build failed. See $session\build-err.log" }
    }
    if (-not (Test-Path -LiteralPath $loader -PathType Leaf)) { throw "Loader not built: $loader" }
    $loader = (Resolve-Path -LiteralPath $loader).Path
    $preflight = Join-Path $PSScriptRoot 'isolation-preflight.ps1'
    $isolationLog = Join-Path $session 'isolation.log'
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        & $preflight -Exe $loader -Log $isolationLog
        if ($LASTEXITCODE -ne 0) { throw "Offline isolation failed. See $isolationLog" }
    } else {
        $preflightArgs = '-NoProfile -ExecutionPolicy Bypass -File "{0}" -Exe "{1}" -Log "{2}"' -f $preflight, $loader, $isolationLog
        $isolation = Start-Process powershell.exe -ArgumentList $preflightArgs -Verb RunAs -Wait -PassThru -WindowStyle Hidden
        if ($isolation.ExitCode -ne 0) { throw "Offline isolation failed. See $isolationLog" }
    }
    $profiles = @(Get-NetFirewallProfile -PolicyStore ActiveStore)
    if ($profiles.Count -ne 3 -or @($profiles | Where-Object { -not $_.Enabled }).Count) {
        throw 'All Windows Firewall profiles must be enabled.'
    }
    $rules = @(Get-NetFirewallRule -PolicyStore ActiveStore -DisplayName 'VIctory offline*victory.exe' -ErrorAction SilentlyContinue |
        Where-Object { (Get-NetFirewallApplicationFilter -AssociatedNetFirewallRule $_).Program -ieq $loader })
    foreach ($direction in @('Inbound', 'Outbound')) {
        $matches = @($rules | Where-Object {
            [string]$_.Direction -eq $direction -and [string]$_.Action -eq 'Block' -and
            [string]$_.Enabled -eq 'True' -and [string]$_.Profile -eq 'Any' -and
            [string]($_ | Get-NetFirewallPortFilter).Protocol -eq 'Any' -and
            [string]($_ | Get-NetFirewallAddressFilter).LocalAddress -eq 'Any' -and
            [string]($_ | Get-NetFirewallAddressFilter).RemoteAddress -eq 'Any'
        })
        if (-not $matches.Count) { throw "Verified $direction firewall block is missing." }
    }
    if ($plan.prebuildShaders) {
        $shaderRoot = Split-Path -Parent $plan.title
        Write-Host "Prebuilding shader and pipeline caches for this GPU. Logs: $session"
        $warm = Start-Process -FilePath $loader -ArgumentList @('translate', ('"' + $shaderRoot + '"'), '--cache', '--pipelines') `
            -WorkingDirectory $repo -Wait -PassThru -WindowStyle Hidden `
            -RedirectStandardOutput (Join-Path $session 'prebuild-out.log') -RedirectStandardError (Join-Path $session 'prebuild-err.log')
        if ($warm.ExitCode -ne 0) { throw "Shader prebuild failed. See $session\prebuild-err.log" }
        $script:launchSettings.prebuildShaders = $false
        Save-LauncherSettings $script:launchSettings
    }
    foreach ($entry in $plan.environment.GetEnumerator()) {
        if ($null -eq $entry.Value) { Remove-Item "Env:$($entry.Key)" -ErrorAction SilentlyContinue }
        else { Set-Item "Env:$($entry.Key)" ([string]$entry.Value) }
    }
    $argsLine = @('run', ('"' + $plan.title + '"'), '--execute', '--console', $plan.arguments[4],
        '--resolution', $plan.arguments[6], '--quality', $plan.arguments[8],
        '--aa', $plan.arguments[10], '--anisotropic', $plan.arguments[12])
    $process = Start-Process -FilePath $loader -ArgumentList $argsLine -WorkingDirectory (Split-Path -Parent $plan.title) `
        -RedirectStandardOutput (Join-Path $session 'stdout.log') -RedirectStandardError (Join-Path $session 'stderr.log') `
        -WindowStyle Hidden -PassThru
    "pid=$($process.Id)`nstarted_utc=$((Get-Date).ToUniversalTime().ToString('o'))" |
        Set-Content -LiteralPath (Join-Path $session 'process.txt')
    [System.Windows.Forms.MessageBox]::Show("Game launched (PID $($process.Id)).`nOffline blocks verified.`nLogs: $session", 'VIctory') | Out-Null
} catch {
    [System.Windows.Forms.MessageBox]::Show($_.Exception.Message, 'VIctory launch failed') | Out-Null
    throw
}
