# Deny all inbound and outbound protocols for the named executables.
# WFP filters cover local loopback, which Windows Firewall's program rules permit.
# Run elevated. A manifest is a JSON array of absolute executable paths.
param(
    [string[]] $Exe = @(),
    [string] $ManifestPath = '',
    [switch] $SkipWer
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
$scriptDirectory = Assert-FixedLocalPath $PSScriptRoot 'Isolation script directory'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script from an elevated PowerShell session'
}

if ($ManifestPath) {
    $ManifestPath = Assert-FixedLocalPath $ManifestPath 'Isolation manifest'
    $fromManifest = ConvertFrom-Json -InputObject (Get-Content -LiteralPath $ManifestPath -Raw)
    foreach ($item in $fromManifest) {
        if ($item -isnot [string]) { throw 'Manifest entries must be paths' }
        $Exe += $item
    }
}
if (-not $Exe.Count) { throw 'Supply at least one executable path' }

if (-not ('VictoryNativeWer' -as [type])) {
    Add-Type -TypeDefinition @'
using System.Runtime.InteropServices;
public static class VictoryNativeWer {
    [DllImport("wer.dll", CharSet = CharSet.Unicode)]
    public static extern int WerAddExcludedApplication(string exeName, bool allUsers);
}
'@
}

$profiles = @(Get-NetFirewallProfile -PolicyStore ActiveStore)
if ($profiles.Count -ne 3 -or @($profiles | Where-Object { $_.Enabled -ne $true }).Count) {
    throw 'All three Windows Firewall profiles must be enabled before testing'
}

$paths = @($Exe | ForEach-Object {
    if (-not [IO.Path]::IsPathRooted($_)) { throw "Not an absolute path: $_" }
    $path = Assert-FixedLocalPath $_ 'Title executable'
    $path = (Resolve-Path -LiteralPath $path).Path
    $file = Get-Item -LiteralPath $path
    if ($file.PSIsContainer -or $file.Extension -ine '.exe') {
        throw "Not an executable file: $path"
    }
    $path
} | Sort-Object -Unique)

$wfpSource = Assert-FixedLocalPath (Join-Path $scriptDirectory 'victory-offline-wfp.c') 'WFP helper source'
$repo = Assert-FixedLocalPath (Join-Path $scriptDirectory '..') 'Repository'
$wfpDirectory = Join-Path $repo 'target\offline-wfp'
$wfpHelper = Join-Path $wfpDirectory 'victory-offline-wfp.exe'
$wfpHelper = Assert-FixedLocalPath $wfpHelper 'WFP helper'
if (-not (Test-Path -LiteralPath $wfpHelper) -or
    (Get-Item -LiteralPath $wfpSource).LastWriteTimeUtc -gt (Get-Item -LiteralPath $wfpHelper).LastWriteTimeUtc) {
    $gcc = (Get-Command gcc.exe -ErrorAction SilentlyContinue).Source
    if (-not $gcc) { throw 'gcc.exe is required to build the WFP isolation helper' }
    $gcc = Assert-FixedLocalPath $gcc 'GCC executable'
    New-Item -ItemType Directory -Force -Path $wfpDirectory | Out-Null
    & $gcc -O2 -Wall -Wextra -municode -o $wfpHelper $wfpSource -lfwpuclnt -ladvapi32 -luuid
    if ($LASTEXITCODE -ne 0) { throw 'WFP isolation helper build failed' }
}

foreach ($path in $paths) {
    $file = Get-Item -LiteralPath $path
    & $wfpHelper add $path
    if ($LASTEXITCODE -ne 0) { throw "Could not install WFP isolation for $path" }
    & $wfpHelper check $path
    if ($LASTEXITCODE -ne 0) { throw "Could not verify WFP isolation for $path" }
    if (-not $SkipWer) {
        $wer = [VictoryNativeWer]::WerAddExcludedApplication($file.Name, $false)
        if ($wer -ne 0) {
            throw ('Could not exclude {0} from WER: HRESULT 0x{1:X8}' -f $file.Name, $wer)
        }
    }

    $digest = [Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [Text.Encoding]::UTF8.GetBytes($path.ToLowerInvariant())
        $hex = [BitConverter]::ToString($digest.ComputeHash($bytes)).Replace('-', '')
    } finally {
        $digest.Dispose()
    }
    $prefix = 'VictoryNativeOffline-' + $hex.Substring(0, 16)

    foreach ($direction in @('Inbound', 'Outbound')) {
        $name = "$prefix-$direction"
        $existing = Get-NetFirewallRule -Name $name -ErrorAction SilentlyContinue
        if (-not $existing) {
            New-NetFirewallRule -Name $name -DisplayName "VIctory offline $direction - $($file.Name)" -Direction $direction -Action Block -Enabled True -Profile Any -Protocol Any -LocalAddress Any -RemoteAddress Any -Program $path | Out-Null
        }
        $active = Get-NetFirewallRule -PolicyStore ActiveStore -Name $name -ErrorAction Stop
        $application = $active | Get-NetFirewallApplicationFilter
        $port = $active | Get-NetFirewallPortFilter
        $address = $active | Get-NetFirewallAddressFilter
        if ([string]$active.Direction -ne $direction -or [string]$active.Action -ne 'Block' -or
            [string]$active.Enabled -ne 'True' -or [string]$active.Profile -ne 'Any' -or
            $application.Program -ine $path -or [string]$port.Protocol -ne 'Any' -or
            [string]$address.LocalAddress -ne 'Any' -or [string]$address.RemoteAddress -ne 'Any') {
            throw "Active firewall rule $name does not match the requested block"
        }
        Write-Output "Verified $direction block: $path ($name)"
    }
    if (-not $SkipWer) { Write-Output "WER excluded for current user: $($file.Name)" }
}
