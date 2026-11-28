# Elevated helper for a user-integrity first run. Only network isolation runs
# here; the title remains in the caller's Windows logon token.
param(
    [Parameter(Mandatory = $true)] [string] $Exe,
    [Parameter(Mandatory = $true)] [string] $Log
)

$ErrorActionPreference = 'Stop'

function Assert-FixedLocalPath([string] $Path, [string] $Label) {
    if ([string]::IsNullOrWhiteSpace($Path) -or $Path -notmatch '^[A-Za-z]:\\') {
        throw "$Label must be an absolute path on a fixed local drive."
    }
    try { $full = [IO.Path]::GetFullPath($Path) }
    catch { throw "$Label is not a valid local path." }
    if ($full -notmatch '^[A-Za-z]:\\' -or
        [IO.DriveInfo]::new($full.Substring(0, 1)).DriveType -ne [IO.DriveType]::Fixed) {
        throw "$Label must be on a fixed local drive."
    }
    $partPath = $full.Substring(0, 3)
    foreach ($part in ($full.Substring(3) -split '\\' | Where-Object { $_ })) {
        $partPath = Join-Path $partPath $part
        if (Test-Path -LiteralPath $partPath) {
            $attributes = [IO.File]::GetAttributes($partPath)
            if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "$Label cannot pass through a reparse point: $partPath"
            }
        }
    }
    return $full
}

# Validate both paths before the catch block can write to -Log. Otherwise a
# bad -Log path could still receive diagnostics from a failed preflight.
$scriptFolder = Assert-FixedLocalPath $PSScriptRoot 'Isolation helper folder'
$Exe = Assert-FixedLocalPath $Exe 'Title executable'
$Log = Assert-FixedLocalPath $Log 'Isolation log'
if (-not (Test-Path -LiteralPath $Exe -PathType Leaf) -or [IO.Path]::GetExtension($Exe) -ine '.exe') {
    throw 'Title executable must be an existing EXE file.'
}
if (-not (Test-Path -LiteralPath (Split-Path -Parent $Log) -PathType Container)) {
    throw 'Isolation log folder must exist on a fixed local drive.'
}
try {
    & (Join-Path $scriptFolder 'block-native-title-outbound.ps1') -Exe $Exe *>&1 |
        Out-File -LiteralPath $Log -Encoding UTF8
} catch {
    ($_ | Out-String) | Add-Content -LiteralPath $Log
    exit 1
}
