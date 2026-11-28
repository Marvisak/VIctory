# Build the shared native runtime and the console DLLs that forward to it.
# All output is our own code under target/release/native-shims.
param(
    [string] $OutputDirectory = '',
    [string] $GameDirectory = '',
    [switch] $Diagnostics
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
$scriptDirectory = Assert-FixedLocalPath -Path $PSScriptRoot -Label 'Script directory'
$repo = Assert-FixedLocalPath -Path (Join-Path $scriptDirectory '..') -Label 'Repository'
if ($GameDirectory -and -not $OutputDirectory) {
    throw 'Specify an output directory outside the repository for title files'
}
if (-not $OutputDirectory) {
    $OutputDirectory = Join-Path $repo 'target\release\native-shims'
}
$out = Assert-FixedLocalPath -Path $OutputDirectory -Label 'Output directory'
if ($GameDirectory) {
    $GameDirectory = Assert-FixedLocalPath -Path $GameDirectory -Label 'Game directory'
}
New-Item -ItemType Directory -Force -Path $out | Out-Null

$cargo = (Get-Command cargo.exe -ErrorAction SilentlyContinue).Source
if (-not $cargo) {
    $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
}
if (-not (Test-Path -LiteralPath $cargo)) {
    throw 'cargo.exe was not found'
}
$cargoDirectory = Assert-FixedLocalPath -Path (Split-Path -Parent $cargo) -Label 'Cargo executable directory'
$cargoItem = Get-Item -LiteralPath $cargo -Force
if ($cargoItem.Attributes -band [IO.FileAttributes]::ReparsePoint) {
    # Rustup commonly installs cargo.exe as a symlink to rustup.exe. Permit
    # that one checked local tool link without permitting linked data/output
    # directories or an executable on a network share.
    if ($cargoItem.LinkType -ne 'SymbolicLink' -or @($cargoItem.Target).Count -ne 1) {
        throw 'Cargo executable link is unsupported'
    }
    $cargoTarget = [string]@($cargoItem.Target)[0]
    if (-not [IO.Path]::IsPathRooted($cargoTarget)) {
        $cargoTarget = Join-Path $cargoDirectory $cargoTarget
    }
    $cargoTarget = Assert-FixedLocalPath -Path $cargoTarget -Label 'Cargo executable target'
    if (-not (Test-Path -LiteralPath $cargoTarget -PathType Leaf)) {
        throw 'Cargo executable target is missing'
    }
} else {
    $cargo = Assert-FixedLocalPath -Path $cargo -Label 'Cargo executable'
}

Push-Location $repo
try {
    $cargoArguments = @('build', '--offline', '--release', '-p', 'victory-native-runtime')
    if ($Diagnostics) { $cargoArguments += @('--features', 'victory-hle/diagnostics') }
    & $cargo @cargoArguments
    if ($LASTEXITCODE -ne 0) { throw 'native runtime build failed' }
} finally {
    Pop-Location
}

$runtime = Join-Path $repo 'target\release\victory_native_runtime.dll'
if (-not (Test-Path -LiteralPath $runtime)) {
    throw 'native runtime DLL was not built'
}

$link = (Get-Command link.exe -ErrorAction SilentlyContinue).Source
if (-not $link) {
    $installer = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path -LiteralPath $installer) {
        $installation = & $installer -latest -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if ($installation) {
            $tools = Join-Path $installation 'VC\Tools\MSVC'
            $link = Get-ChildItem -LiteralPath $tools -Directory |
                Sort-Object Name -Descending |
                ForEach-Object { Join-Path $_.FullName 'bin\Hostx64\x64\link.exe' } |
                Where-Object { Test-Path -LiteralPath $_ } |
                Select-Object -First 1
        }
    }
}
if (-not $link) { throw 'Visual Studio x64 link.exe was not found' }
$link = Assert-FixedLocalPath -Path $link -Label 'Linker executable'
$dumpbin = Join-Path (Split-Path -Parent $link) 'dumpbin.exe'
if (-not (Test-Path -LiteralPath $dumpbin)) { throw 'dumpbin.exe was not found beside link.exe' }
$dumpbin = Assert-FixedLocalPath -Path $dumpbin -Label 'Dumpbin executable'

$exports = @{
    'xmem.dll' = @(
        'XMemAlloc', 'XMemAllocDefault', 'XMemAllocatePhysicalPages',
        'XMemFree', 'XMemFreeDefault', 'XMemFreePhysicalPages',
        'XMemGetWorkingSetStatistics', 'XMemIsToolsMemAvailable',
        'XMemMapPhysicalPages', 'XMemMapPhysicalPagesScatter',
        'XMemTransferMemory', 'XMemVirtualAlloc', 'XMemVirtualQuery'
    )
    'xg_xs.dll' = @('XGCreateTextureComputer', 'XGSuggestSwizzleMode')
    'chad.dll' = @(
        'XDspConnectWithMaximumStreamLimit', 'XDspDisconnect',
        'XDspActivate', 'XDspDeactivate', 'XDspSubmitCommand'
    )
    'xapu.dll' = @(
        'XApuConnect', 'XApuDequeueResult', 'XApuDisconnect',
        'XApuEnqueueCommand', 'XApuGetResultCount'
    )
    'AcpHal.dll' = @(
        'AcpHalAllocateShapeContexts', 'AcpHalCreate',
        'AcpHalReleaseShapeContexts', 'ApuAlloc', 'ApuCreateHeap',
        'ApuFree', 'ApuIsVirtualAddressValid', 'ApuMapApuAddress',
        'ApuMapVirtualAddress'
    )
    'XCRDAPI.dll' = @('DStorageGetFactory')
    'GameInput.dll' = @('GameInputCreate')
    'xgameruntime.dll' = @(
        'DllCanUnloadNow', 'InitializeApiImpl', 'InitializeApiImplEx',
        'InitializeApiImplEx2', 'QueryApiImpl', 'UninitializeApiImpl',
        'XErrorReport'
    )
    'd3d12_xs.dll' = @(
        'D3D12ConfigureVirtualMemoryX', 'D3D12XboxCreateDevice',
        'D3D12SerializeRootSignature', 'D3D12SerializeVersionedRootSignature',
        'D3D12CreateRootSignatureDeserializer', 'IID_ID3D12Device'
    )
}

function ForwardTarget([string] $dll, [string] $name) {
    if ($dll -ieq 'd3d12_xs.dll') { return "Victory$name" }
    return $name
}

$runtimeExports = (& $dumpbin /exports $runtime) -join "`n"
if ($LASTEXITCODE -ne 0) { throw 'could not inspect native runtime exports' }
foreach ($dll in $exports.Keys) {
    foreach ($name in $exports[$dll]) {
        $target = ForwardTarget $dll $name
        if ($runtimeExports -notmatch ("\b" + [regex]::Escape($target) + "\b")) {
            throw "native runtime does not export $target"
        }
    }
}

Copy-Item -LiteralPath $runtime -Destination (Join-Path $out 'victory_native_runtime.dll') -Force
foreach ($dll in ($exports.Keys | Sort-Object)) {
    $stem = [IO.Path]::GetFileNameWithoutExtension($dll)
    $definition = Join-Path $out ($stem + '.def')
    $lines = @("LIBRARY $dll", 'EXPORTS') + @(
        $exports[$dll] | ForEach-Object {
            $target = ForwardTarget $dll $_
            $line = "$_=victory_native_runtime.$target"
            if ($_ -eq 'IID_ID3D12Device') { $line += ' DATA' }
            if ($dll -ieq 'GameInput.dll') { $line += ' @1' }
            $line
        }
    )
    Set-Content -LiteralPath $definition -Value $lines -Encoding ascii
    & $link /DLL /NOENTRY /MACHINE:X64 ("/DEF:$definition") ("/OUT:" + (Join-Path $out $dll))
    if ($LASTEXITCODE -ne 0) { throw "failed to build $dll" }
}

if ($GameDirectory) {
    $python = (Get-Command python.exe -ErrorAction SilentlyContinue).Source
    if (-not $python) { throw 'python.exe with the pefile package was not found' }
    $python = Assert-FixedLocalPath -Path $python -Label 'Python executable'
    & $python (Join-Path $PSScriptRoot 'prepare-native-title.py') $GameDirectory $out
    if ($LASTEXITCODE -ne 0) { throw 'title import rewrite failed' }
    foreach ($dll in @(
        'VS2_32.dll', 'VINHTTP.dll',
        'vpi-ms-win-core-windowserrorreporting-l1-1-0.dll'
    )) {
        $definition = Join-Path $out ([IO.Path]::GetFileNameWithoutExtension($dll) + '.def')
        & $link /DLL /NOENTRY /MACHINE:X64 ("/DEF:$definition") ("/OUT:" + (Join-Path $out $dll))
        if ($LASTEXITCODE -ne 0) { throw "failed to build $dll" }
    }
}

Write-Output "Native shims built in $out"
