# Headless validation of the Windows launcher settings and constructed controls.
$ErrorActionPreference = 'Stop'
$launcher = Join-Path $PSScriptRoot 'launch-victory.ps1'
$pwsh = (Get-Command pwsh.exe -ErrorAction Stop).Source
$testRoot = Join-Path ([IO.Path]::GetTempPath()) ('victory-menu-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $testRoot | Out-Null
$settingsPath = Join-Path $testRoot 'settings.json'
$dummyExe = Join-Path $testRoot 'dummy.exe'
[IO.File]::WriteAllBytes($dummyExe, [byte[]]@())

function Invoke-Menu([hashtable] $values, [string] $mode, [string] $profile = '') {
    $values | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $settingsPath -Encoding UTF8
    $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $launcher,
        "-$mode", '-SettingsPath', $settingsPath)
    if ($profile) { $arguments += @('-UiProfile', $profile) }
    $output = & $pwsh @arguments 2>&1 | Out-String
    return [pscustomobject]@{ ExitCode = $LASTEXITCODE; Output = $output }
}

function Assert([bool] $condition, [string] $message) {
    if (-not $condition) { throw $message }
}

try {
    $base = @{
        title = $dummyExe; useFidelityFx = $false; resolutionProfile = 'custom'
        resolution = '1920x1080'; rebuild = $true; streamLocalPercent = 91
        rootSystemMB = 4875; prebuildShaders = $true
    }
    $debug = $base.Clone()
    $debug.diagFrameCapture = $true
    $debug.diagWorldCapture = $true
    $debug.diagCaptureFullRes = $true
    $debug.diagFrameNumbers = '60, 300'
    $debug.diagWorldOffsets = '0, 1, 5'
    $debug.diagTextureTrace = $true
    $debug.diagShaderTrace = $true
    $debug.diagShaderHashes = 'ABCDEF0123456789,0123456789ABCDEF'
    $debug.diagDumpShaderSource = $true
    $debug.diagSpriteTargets = $true
    $debug.diagCoverage = $true
    $debug.diagGpuTiming = $true
    $debug.diagD3dDebug = $true
    $debug.diagDred = $true
    $result = Invoke-Menu $debug 'ValidateOnly'
    Assert ($result.ExitCode -eq 0) "Valid diagnostics plan failed: $($result.Output)"
    $plan = $result.Output | ConvertFrom-Json
    Assert ($plan.diagnostics -and $plan.prebuildShaders) 'Diagnostic build and shader prebuild were not selected.'
    Assert ($plan.environment.VICTORY_CAPTURE -eq '@session/captures') 'Capture path was not scoped to the launch session.'
    Assert ($plan.environment.VICTORY_CAPTURE_FRAMES -eq '60,300') 'Frame list was not normalized.'
    Assert ($plan.environment.VICTORY_CAPTURE_WORLD -eq '0,1,5') 'World offset list was not normalized.'
    Assert ($plan.environment.VICTORY_TRACE_SHADERS -eq 'abcdef0123456789,0123456789abcdef') 'Shader hashes were not normalized.'
    Assert ($plan.environment.VICTORY_DUMP_SHADER_HASH -eq $plan.environment.VICTORY_TRACE_SHADERS) 'Shader source dump did not use the selected hashes.'
    foreach ($name in @('VICTORY_DUMP_TARGETS', 'VICTORY_TRACE_STREAMED_TEXTURES', 'VICTORY_COVERAGE',
        'VICTORY_GPU_TIMING', 'VICTORY_D3D12_DEBUG', 'VICTORY_D3D12_DRED', 'VICTORY_CAPTURE_FULL_RES')) {
        Assert ($plan.environment.$name -eq '1') "$name was not enabled."
    }
    $ui = Invoke-Menu $debug 'ValidateUi'
    Assert ($ui.ExitCode -eq 0) "Valid menu construction failed: $($ui.Output)"
    $view = $ui.Output | ConvertFrom-Json
    Assert ($view.tabs -contains 'Debugging') 'Debugging tab is missing.'
    Assert ($view.autoFindButton -eq 'Find game files in this folder') 'Game file search control is missing.'
    Assert ($view.vramSlider.value -eq 91 -and $view.systemRamSlider.value -eq 4875) 'Saved memory limits did not populate sliders.'
    Assert ($view.cookDialog.rows -eq 9 -and $view.cookDialog.pathFields.Count -eq 6) 'One-time cook dialog is incomplete.'

    # Exercise the same persistence functions used by Save and FormClosing,
    # then construct the menu again from the file on disk.
    $tokens = $null; $parseErrors = $null
    $ast = [Management.Automation.Language.Parser]::ParseFile($launcher, [ref]$tokens, [ref]$parseErrors)
    Assert (-not $parseErrors) 'Launcher script did not parse.'
    foreach ($name in @('Assert-LocalFixedPath', 'Save-LauncherSettings', 'Find-GameFiles')) {
        $definition = $ast.FindAll({ param($node)
            $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name
        }, $true) | Select-Object -First 1
        Assert ($null -ne $definition) "Missing launcher function $name."
        Invoke-Expression $definition.Extent.Text
    }
    $script:SettingsPath = $settingsPath
    $view.settings.quality = 'medium'
    $view.settings.gameRoot = $testRoot
    Save-LauncherSettings $view.settings
    $savedOutput = & $pwsh -NoProfile -ExecutionPolicy Bypass -File $launcher -ValidateUi -SettingsPath $settingsPath 2>&1 | Out-String
    Assert ($LASTEXITCODE -eq 0) "Saved menu failed to reopen: $savedOutput"
    $savedView = $savedOutput | ConvertFrom-Json
    foreach ($property in $view.settings.PSObject.Properties) {
        Assert ("$($savedView.settings.$($property.Name))" -ceq "$($property.Value)") "Setting $($property.Name) did not persist."
    }
    Assert (@($savedView.settings.PSObject.Properties).Count -eq @($view.settings.PSObject.Properties).Count) 'Saved settings lost fields.'
    $draft = $savedView.settings.PSObject.Copy(); $draft.title = ''
    Save-LauncherSettings $draft
    Assert ((Get-Content -LiteralPath $settingsPath -Raw | ConvertFrom-Json).title -ceq '') 'Incomplete game selection could not be saved.'
    Assert (-not @(Get-ChildItem -LiteralPath $testRoot -File | Where-Object { $_.Extension -in @('.tmp', '.bak') }).Count) 'Save left temporary settings files.'

    $scanRoot = Join-Path $testRoot 'game-files'
    $gameDir = Join-Path $scanRoot 'game\bin'
    $ffxDir = Join-Path $scanRoot 'game\plugins\amd'
    New-Item -ItemType Directory -Force -Path $gameDir, $ffxDir | Out-Null
    [IO.File]::WriteAllBytes((Join-Path $gameDir 'game_scarlett_master.exe'), [byte[]]@())
    [IO.File]::WriteAllBytes((Join-Path $ffxDir 'amd_fidelityfx_loader_dx12.dll'), [byte[]]@())
    $partial = Find-GameFiles $scanRoot
    Assert ($partial.gameExecutables.Count -eq 1 -and $partial.fidelityFxFolders.Count -eq 0) 'Incomplete FidelityFX pair was accepted.'
    [IO.File]::WriteAllBytes((Join-Path $ffxDir 'amd_fidelityfx_upscaler_dx12.dll'), [byte[]]@())
    $found = Find-GameFiles $scanRoot
    Assert ($found.gameExecutables.Count -eq 1 -and $found.fidelityFxFolders.Count -eq 1) 'Nested game files were not found.'
    Assert ($found.gameExecutables[0] -eq (Join-Path $gameDir 'game_scarlett_master.exe') -and
        $found.fidelityFxFolders[0] -eq $ffxDir) 'Game file search chose the wrong paths.'
    $otherDir = Join-Path $scanRoot 'game\other'
    New-Item -ItemType Directory -Force -Path $otherDir | Out-Null
    [IO.File]::WriteAllBytes((Join-Path $otherDir 'game_scarlett_master.exe'), [byte[]]@())
    Assert ((Find-GameFiles $scanRoot).gameExecutables.Count -eq 2) 'Ambiguous game executables were not reported.'
    try { Find-GameFiles '\\example.invalid\share' | Out-Null; throw 'UNC game search was accepted.' }
    catch { Assert ($_.Exception.Message -ne 'UNC game search was accepted.') 'UNC game search was accepted.' }

    $profile720 = Invoke-Menu $base 'ValidateUi' '720p'
    Assert ($profile720.ExitCode -eq 0) "720p selection failed: $($profile720.Output)"
    $p720 = $profile720.Output | ConvertFrom-Json
    Assert ($p720.settings.resolution -eq '1280x720' -and $p720.settings.presentMax -eq '1280x720' -and
        $p720.settings.streamMaxEdge -eq 512 -and $p720.settings.prebuildShaders) '720p selection did not update its controls.'
    $profile1080 = Invoke-Menu $base 'ValidateUi' '1080p'
    Assert ($profile1080.ExitCode -eq 0) "1080p selection failed: $($profile1080.Output)"
    $p1080 = $profile1080.Output | ConvertFrom-Json
    Assert ($p1080.settings.resolution -eq '1920x1080' -and $p1080.settings.presentMax -eq '1920x1080' -and
        $p1080.settings.streamMaxEdge -eq 1024) '1080p selection did not update its controls.'
    $profileNative = Invoke-Menu $base 'ValidateUi' 'native'
    Assert ($profileNative.ExitCode -eq 0) "Native selection failed: $($profileNative.Output)"
    $native = $profileNative.Output | ConvertFrom-Json
    Assert ($native.settings.resolutionProfile -eq 'native' -and $native.settings.resolution -match '^\d+x\d+$' -and
        $native.settings.presentMax -eq 'off' -and $native.settings.streamMaxEdge -eq 0) 'Native selection did not update its controls.'

    $cookedRoot = Join-Path $testRoot 'cooked'
    New-Item -ItemType Directory -Path $cookedRoot | Out-Null
    $manifest720 = Join-Path $cookedRoot 'runtime_720p.toml'
    "version = 1`nprofile = `"720p`"" | Set-Content -LiteralPath $manifest720 -Encoding UTF8
    $cooked = $base.Clone()
    $cooked.resolutionProfile = '720p'
    $cooked.resolution = '1280x720'
    $cooked.useCookedTextures = $true
    $cooked.cookedRoot = $cookedRoot
    $result = Invoke-Menu $cooked 'ValidateOnly'
    Assert ($result.ExitCode -eq 0) "Valid cooked profile plan failed: $($result.Output)"
    $cookedPlan = $result.Output | ConvertFrom-Json
    Assert ($cookedPlan.environment.VICTORY_COOKED_PROFILE_MANIFEST -eq $manifest720) 'Cooked manifest path was not selected.'
    $result = Invoke-Menu $cooked 'ValidateUi'
    Assert ($result.ExitCode -eq 0) "Valid cooked profile UI failed: $($result.Output)"
    $cookedUi = $result.Output | ConvertFrom-Json
    Assert ($cookedUi.settings.useCookedTextures -and $cookedUi.settings.cookedRoot -eq $cookedRoot) 'Cooked root and opt-in were not persisted into controls.'
    Assert ($cookedUi.selectedCookedManifest -eq $manifest720) 'Selected cooked manifest was not shown in the menu.'
    "version = 1`nprofile = `"1080p`"" | Set-Content -LiteralPath $manifest720 -Encoding UTF8
    $result = Invoke-Menu $cooked 'ValidateOnly'
    Assert ($result.ExitCode -ne 0 -and $result.Output -match 'manifest header') 'Mismatched cooked profile was accepted.'
    $cooked1080 = $cooked.Clone(); $cooked1080.resolutionProfile = '1080p'
    $result = Invoke-Menu $cooked1080 'ValidateOnly'
    Assert ($result.ExitCode -ne 0 -and $result.Output -match 'manifest is missing') 'Missing cooked profile was accepted.'
    $cookedCustom = $cooked.Clone(); $cookedCustom.resolutionProfile = 'custom'
    $result = Invoke-Menu $cookedCustom 'ValidateOnly'
    Assert ($result.ExitCode -ne 0 -and $result.Output -match 'require a 720p or 1080p') 'Custom resolution accepted a cooked profile.'

    $python = Get-Command python.exe -ErrorAction SilentlyContinue
    $fixtureScript = Join-Path $PSScriptRoot 'test-runtime-texture-profile.py'
    if ($python -and (Test-Path -LiteralPath $fixtureScript -PathType Leaf)) {
        $fixtureRoot = Join-Path $testRoot 'fixture'
        & $python.Source $fixtureScript --output-root $fixtureRoot | Out-Null
        Assert ($LASTEXITCODE -eq 0) 'Synthetic cooked profile fixture generation failed.'
        $verifiedCooked = $cooked.Clone(); $verifiedCooked.cookedRoot = $fixtureRoot
        $result = Invoke-Menu $verifiedCooked 'ValidateCookedOnly'
        Assert ($result.ExitCode -eq 0 -and $result.Output -match '2 verified mips') "Launcher cooked validator rejected a valid DDS: $($result.Output)"
        $ddsPath = Join-Path $fixtureRoot 'assets_720p\wall.dds'
        $dds = [IO.File]::ReadAllBytes($ddsPath)
        $dds[$dds.Length - 1] = $dds[$dds.Length - 1] -bxor 1
        [IO.File]::WriteAllBytes($ddsPath, $dds)
        $result = Invoke-Menu $verifiedCooked 'ValidateCookedOnly'
        Assert ($result.ExitCode -ne 0 -and $result.Output -match 'manifest validation failed') 'Launcher cooked validator accepted a corrupted DDS.'
    }

    $badHash = $debug.Clone(); $badHash.diagShaderHashes = 'wrong'
    $result = Invoke-Menu $badHash 'ValidateOnly'
    Assert ($result.ExitCode -ne 0 -and $result.Output -match 'Shader hashes') 'Invalid shader hash was accepted.'
    $noBuild = $debug.Clone(); $noBuild.rebuild = $false
    $result = Invoke-Menu $noBuild 'ValidateOnly'
    Assert ($result.ExitCode -ne 0 -and $result.Output -match 'diagnostics feature') 'Diagnostics without a diagnostics build were accepted.'
    $badFrame = $debug.Clone(); $badFrame.diagFrameNumbers = '60,100001'
    $result = Invoke-Menu $badFrame 'ValidateOnly'
    Assert ($result.ExitCode -ne 0 -and $result.Output -match 'Frame numbers') 'Out-of-range frame was accepted.'
    $badVram = $base.Clone(); $badVram.streamLocalPercent = 96
    $result = Invoke-Menu $badVram 'ValidateOnly'
    Assert ($result.ExitCode -ne 0 -and $result.Output -match 'Local VRAM threshold') 'Out-of-range VRAM threshold was accepted.'
    $badFullRes = $base.Clone(); $badFullRes.diagCaptureFullRes = $true
    $result = Invoke-Menu $badFullRes 'ValidateOnly'
    Assert ($result.ExitCode -ne 0 -and $result.Output -match 'Full-resolution capture requires') 'Full-resolution capture without frame capture was accepted.'
    $noTitle = $base.Clone(); $noTitle.title = ''
    $result = Invoke-Menu $noTitle 'ValidateOnly'
    Assert ($result.ExitCode -ne 0 -and $result.Output -match 'Choose the game executable') 'Missing game path was accepted.'

    $classifiedPath = Join-Path $testRoot 'classified.json'
    $identitiesPath = Join-Path $testRoot 'verified-identities.json'
    '{"assets":[{"output_name":"wall.png"}]}' | Set-Content -LiteralPath $classifiedPath -Encoding UTF8
    '{"assets":[{"profile":"720p","output_name":"wall.png"}]}' | Set-Content -LiteralPath $identitiesPath -Encoding UTF8
    $extractedRoot = Join-Path $testRoot 'extracted'
    $outputRoot = Join-Path $testRoot 'output'
    New-Item -ItemType Directory -Path $extractedRoot, $outputRoot | Out-Null
    $texconvPath = Join-Path $testRoot 'texconv.exe'
    $texassemblePath = Join-Path $testRoot 'texassemble.exe'
    [IO.File]::WriteAllBytes($texconvPath, [byte[]]@())
    [IO.File]::WriteAllBytes($texassemblePath, [byte[]]@())
    $cookInputs = $base.Clone()
    $cookInputs.resolutionProfile = '720p'
    $cookInputs.cookedRoot = $outputRoot
    $cookInputs.cookManifest = $classifiedPath
    $cookInputs.cookInputRoot = $extractedRoot
    $cookInputs.cookIdentities = $identitiesPath
    $cookInputs.cookTexconv = $texconvPath
    $cookInputs.cookTexassemble = $texassemblePath
    $result = Invoke-Menu $cookInputs 'ValidateCookInputs'
    Assert ($result.ExitCode -eq 0) "Valid cook inputs were rejected: $($result.Output)"
    $cookPlan = $result.Output | ConvertFrom-Json
    Assert ($cookPlan.eligibleAssets -eq 1 -and $cookPlan.profile -eq '720p') 'Eligible cook asset count is incorrect.'
    $missingManifest = $cookInputs.Clone(); $missingManifest.cookManifest = ''
    $result = Invoke-Menu $missingManifest 'ValidateCookInputs'
    Assert ($result.ExitCode -ne 0 -and $result.Output -match 'classified asset manifest') 'Cook accepted no classified manifest.'
    $missingTool = $cookInputs.Clone(); $missingTool.cookTexassemble = ''
    $result = Invoke-Menu $missingTool 'ValidateCookInputs'
    Assert ($result.ExitCode -ne 0 -and $result.Output -match 'texassemble.exe') 'Cook accepted no Texassemble executable.'
    '{"assets":[{"profile":"1080p","output_name":"wall.png"}]}' | Set-Content -LiteralPath $identitiesPath -Encoding UTF8
    $result = Invoke-Menu $cookInputs 'ValidateCookInputs'
    Assert ($result.ExitCode -ne 0 -and $result.Output -match 'no 720p assets') 'Cook accepted no identities for the selected profile.'
    '{"assets":[{"profile":"720p","output_name":"wall.png"}]}' | Set-Content -LiteralPath $identitiesPath -Encoding UTF8
    $sameOutput = $cookInputs.Clone(); $sameOutput.cookedRoot = $extractedRoot
    $result = Invoke-Menu $sameOutput 'ValidateCookInputs'
    Assert ($result.ExitCode -ne 0 -and $result.Output -match 'separate from') 'Cook output inside original images was accepted.'
    $uncInput = $cookInputs.Clone(); $uncInput.cookInputRoot = '\\example.invalid\share\images'
    $result = Invoke-Menu $uncInput 'ValidateCookInputs'
    Assert ($result.ExitCode -ne 0 -and $result.Output -match 'UNC paths are not allowed') 'UNC cook input was accepted.'
    $uncSettings = & $pwsh -NoProfile -ExecutionPolicy Bypass -File $launcher -ValidateOnly -SettingsPath '\\example.invalid\share\launcher.json' 2>&1 | Out-String
    Assert ($LASTEXITCODE -ne 0 -and $uncSettings -match 'UNC paths are not allowed') 'UNC launcher settings path was accepted.'
    $previousLocalAppData = $env:LOCALAPPDATA
    try {
        $env:LOCALAPPDATA = '\\example.invalid\share\AppData'
        $result = Invoke-Menu $base 'ValidateOnly'
        Assert ($result.ExitCode -ne 0 -and $result.Output -match 'UNC paths are not allowed') 'UNC launcher state folder was accepted.'
    } finally { $env:LOCALAPPDATA = $previousLocalAppData }
    $inheritedNames = @('VICTORY_CONFIG', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR')
    $previousInherited = @{}
    try {
        foreach ($name in $inheritedNames) {
            $previousInherited[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
            [Environment]::SetEnvironmentVariable($name, '\\example.invalid\share\redirected', 'Process')
        }
        $result = Invoke-Menu $base 'ValidateOnly'
        Assert ($result.ExitCode -eq 0) "Inherited config path changed the local plan: $($result.Output)"
        $localPlan = $result.Output | ConvertFrom-Json
        foreach ($name in $inheritedNames) {
            Assert ($localPlan.environment.$name -match '^[a-zA-Z]:\\' -and
                $localPlan.environment.$name -notmatch 'example.invalid') "$name inherited a network path."
        }
    } finally {
        foreach ($name in $inheritedNames) { [Environment]::SetEnvironmentVariable($name, $previousInherited[$name], 'Process') }
    }
    $safeAppData = Join-Path $testRoot 'safe-appdata'
    New-Item -ItemType Directory -Path $safeAppData | Out-Null
    $taintedNames = @('VICTORY_CONFIG', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR', 'VICTORY_EMIT_DUMP',
        'VICTORY_AUDIO_CAPTURE', 'VICTORY_XMA_DUMP', 'VICTORY_PIPELINE_LOG',
        'VICTORY_DUMP_LIGHTING_BC_DIR', 'VICTORY_TRAP_LOG', 'VICTORY_NATIVE_LOG_PATH')
    $savedTainted = @{}
    $previousLocalAppData = $env:LOCALAPPDATA
    try {
        $env:LOCALAPPDATA = $safeAppData
        foreach ($name in $taintedNames) {
            $savedTainted[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
            [Environment]::SetEnvironmentVariable($name, '\\example.invalid\share\redirected', 'Process')
        }
        $result = Invoke-Menu $base 'ValidateSafeEnvironment'
        Assert ($result.ExitCode -eq 0) "Safe environment setup failed: $($result.Output)"
        $safe = $result.Output | ConvertFrom-Json
        Assert ($safe.configText.Trim() -eq '[paths]' -and $safe.config -match '^[a-zA-Z]:\\') 'Explicit local loader config was not created.'
        Assert ($safe.cache.StartsWith($safeAppData, [StringComparison]::OrdinalIgnoreCase) -and
            $safe.data.StartsWith($safeAppData, [StringComparison]::OrdinalIgnoreCase)) 'Loader cache/data escaped local app data.'
        Assert (@($safe.remaining | Sort-Object) -join ',' -eq 'VICTORY_CACHE_DIR,VICTORY_CONFIG,VICTORY_DATA_DIR,VICTORY_GAME_DIR') 'Inherited VICTORY output/config variables survived sanitation.'
    } finally {
        $env:LOCALAPPDATA = $previousLocalAppData
        foreach ($name in $taintedNames) { [Environment]::SetEnvironmentVariable($name, $savedTainted[$name], 'Process') }
    }
    $junction = Join-Path $testRoot 'linked'
    New-Item -ItemType Junction -Path $junction -Target $testRoot | Out-Null
    try {
        $linkedSettings = Join-Path $junction 'settings.json'
        $linked = & $pwsh -NoProfile -ExecutionPolicy Bypass -File $launcher -ValidateOnly -SettingsPath $linkedSettings 2>&1 | Out-String
        Assert ($LASTEXITCODE -ne 0 -and $linked -match 'reparse point') 'Reparse launcher settings path was accepted.'
        $linkedInput = $cookInputs.Clone(); $linkedInput.cookInputRoot = $junction
        $result = Invoke-Menu $linkedInput 'ValidateCookInputs'
        Assert ($result.ExitCode -ne 0 -and $result.Output -match 'reparse point') 'Reparse cook input was accepted.'
        $previousLocalAppData = $env:LOCALAPPDATA
        try {
            $env:LOCALAPPDATA = $junction
            $result = Invoke-Menu $base 'ValidateOnly'
            Assert ($result.ExitCode -ne 0 -and $result.Output -match 'reparse point') 'Reparse launcher state folder was accepted.'
        } finally { $env:LOCALAPPDATA = $previousLocalAppData }
    } finally { Remove-Item -LiteralPath $junction -Force }
    $fakeAppData = Join-Path $testRoot 'appdata'
    $fakeState = Join-Path $fakeAppData 'VIctory'
    New-Item -ItemType Directory -Path $fakeState | Out-Null
    $linkedLogs = Join-Path $fakeState 'logs'
    New-Item -ItemType Junction -Path $linkedLogs -Target $testRoot | Out-Null
    $previousLocalAppData = $env:LOCALAPPDATA
    try {
        $env:LOCALAPPDATA = $fakeAppData
        $result = Invoke-Menu $base 'ValidateOnly'
        Assert ($result.ExitCode -ne 0 -and $result.Output -match 'reparse point') 'Reparse launcher log folder was accepted.'
    } finally {
        $env:LOCALAPPDATA = $previousLocalAppData
        Remove-Item -LiteralPath $linkedLogs -Force
    }
    $runtime = Join-Path $PSScriptRoot '..\target\release\victory.exe'
    if (Test-Path -LiteralPath $runtime -PathType Leaf) {
        $hostileConfig = Join-Path $testRoot 'victory.toml'
        $explicitConfig = Join-Path $testRoot 'safe-config.toml'
        "[paths]`ncache_dir = `"\\\\example.invalid\\share\\cache`"`ndata_dir = `"\\\\example.invalid\\share\\data`"" |
            Set-Content -LiteralPath $hostileConfig -Encoding UTF8
        '[paths]' | Set-Content -LiteralPath $explicitConfig -Encoding UTF8
        $configNames = @('VICTORY_CONFIG', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR', 'VICTORY_GAME_DIR', 'VICTORY_TEST_TITLES')
        $savedConfig = @{}
        foreach ($name in $configNames) { $savedConfig[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
        try {
            $env:VICTORY_CONFIG = $explicitConfig
            $env:VICTORY_CACHE_DIR = Join-Path $testRoot 'cache'
            $env:VICTORY_DATA_DIR = Join-Path $testRoot 'data'
            $env:VICTORY_GAME_DIR = $testRoot
            $env:VICTORY_TEST_TITLES = ''
            Push-Location $testRoot
            try { $configOutput = & $runtime config 2>&1 | Out-String; $configExit = $LASTEXITCODE }
            finally { Pop-Location }
            Assert ($configExit -eq 0 -and $configOutput -match [regex]::Escape($explicitConfig) -and
                $configOutput -notmatch 'example.invalid') 'Loader read the hostile game-directory victory.toml despite explicit local config.'
        } finally {
            foreach ($name in $configNames) { [Environment]::SetEnvironmentVariable($name, $savedConfig[$name], 'Process') }
        }
    }
    Write-Host 'Launcher menu validation passed: diagnostics, persistence, profiles, cooked validation, and unsafe-path rejection.'
} finally {
    $resolved = [IO.Path]::GetFullPath($testRoot)
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    if ($resolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and
        [IO.Path]::GetFileName($resolved) -like 'victory-menu-*') {
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}
