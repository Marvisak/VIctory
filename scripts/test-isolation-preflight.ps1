# Negative path tests run without elevation and never install firewall rules.
$ErrorActionPreference = 'Stop'
$preflight = Join-Path $PSScriptRoot 'isolation-preflight.ps1'
$exe = (Get-Process -Id $PID).Path
$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$testRoot = [IO.Path]::GetFullPath((Join-Path $repo 'target\test-isolation-preflight'))
if (-not $testRoot.StartsWith($repo.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Test output must stay inside the repository target directory.'
}
New-Item -ItemType Directory -Force -Path $testRoot | Out-Null
$localLog = Join-Path $testRoot 'preflight.log'

function Assert-Rejected([string] $Name, [string] $CandidateExe, [string] $CandidateLog, [string] $Expected) {
    $errorText = $null
    try {
        & $preflight -Exe $CandidateExe -Log $CandidateLog | Out-Null
    } catch {
        $errorText = $_.Exception.Message
    }
    if (-not $errorText -or $errorText -notlike "*$Expected*") {
        throw "${Name}: expected '$Expected', got '$errorText'."
    }
    Write-Output "PASS $Name"
}

Assert-Rejected 'UNC log' $exe '\\127.0.0.1\NoShare\preflight.log' 'fixed local drive'
Assert-Rejected 'UNC executable' '\\127.0.0.1\NoShare\title.exe' $localLog 'fixed local drive'
Assert-Rejected 'relative log' $exe 'preflight.log' 'fixed local drive'

$junctionTarget = Join-Path $testRoot 'junction-target'
$junction = Join-Path $testRoot 'junction'
New-Item -ItemType Directory -Force -Path $junctionTarget | Out-Null
try {
    if (-not (Test-Path -LiteralPath $junction)) {
        New-Item -ItemType Junction -Path $junction -Target $junctionTarget | Out-Null
    }
    Assert-Rejected 'reparse log' $exe (Join-Path $junction 'preflight.log') 'reparse point'
} finally {
    if (Test-Path -LiteralPath $junction) {
        # Directory.Delete removes the junction itself without walking its target.
        [IO.Directory]::Delete($junction)
    }
}

if (Test-Path -LiteralPath $localLog) { throw 'A rejected path wrote a log.' }
Write-Output 'Isolation preflight path tests passed.'
