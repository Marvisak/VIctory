# Generate a Windows Sandbox config with no virtual NIC and read-only title files.
param(
    [Parameter(Mandatory = $true)] [string] $StageDirectory,
    [string] $OutputDirectory = '',
    [ValidateRange(1, 300)] [int] $TimeoutSeconds = 30,
    [switch] $EnableVGpu
)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$stage = (Resolve-Path -LiteralPath $StageDirectory).Path
if (-not (Test-Path -LiteralPath (Join-Path $stage 'game_scarlett_master.exe'))) {
    throw 'Staged title executable is missing'
}
if (-not $OutputDirectory) {
    $stamp = (Get-Date).ToUniversalTime().ToString('yyyyMMdd-HHmmssZ')
    $OutputDirectory = Join-Path $repo "target\first-run\sandbox-$stamp"
}
$output = [IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Force -Path $output | Out-Null
$tools = Join-Path $repo 'target\offline-wfp'
$gcc = (Get-Command gcc.exe -ErrorAction SilentlyContinue).Source
if (-not $gcc) { throw 'gcc.exe is required to build the isolation helpers' }
New-Item -ItemType Directory -Force -Path $tools | Out-Null
$wfpSource = Join-Path $PSScriptRoot 'victory-offline-wfp.c'
$wfpExe = Join-Path $tools 'victory-offline-wfp.exe'
if (-not (Test-Path -LiteralPath $wfpExe) -or
    (Get-Item -LiteralPath $wfpSource).LastWriteTimeUtc -gt (Get-Item -LiteralPath $wfpExe).LastWriteTimeUtc) {
    & $gcc -O2 -Wall -Wextra -municode -o $wfpExe $wfpSource -lfwpuclnt -ladvapi32 -luuid
    if ($LASTEXITCODE -ne 0) { throw 'WFP helper build failed' }
}
$launcherSource = Join-Path $PSScriptRoot 'victory-first-run.c'
$launcherExe = Join-Path $tools 'victory-first-run.exe'
if (-not (Test-Path -LiteralPath $launcherExe) -or
    (Get-Item -LiteralPath $launcherSource).LastWriteTimeUtc -gt (Get-Item -LiteralPath $launcherExe).LastWriteTimeUtc) {
    & $gcc -O2 -Wall -Wextra -municode -o $launcherExe $launcherSource
    if ($LASTEXITCODE -ne 0) { throw 'First-run helper build failed' }
}

function XmlText([string] $value) { [Security.SecurityElement]::Escape($value) }
$gpu = if ($EnableVGpu) { 'Enable' } else { 'Disable' }
$command = "powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\VictoryScripts\first-run-sandbox-guest.ps1 -TimeoutSeconds $TimeoutSeconds"
$config = @"
<Configuration>
  <vGPU>$gpu</vGPU>
  <Networking>Disable</Networking>
  <AudioInput>Disable</AudioInput>
  <VideoInput>Disable</VideoInput>
  <PrinterRedirection>Disable</PrinterRedirection>
  <ClipboardRedirection>Disable</ClipboardRedirection>
  <ProtectedClient>Enable</ProtectedClient>
  <MappedFolders>
    <MappedFolder><HostFolder>$(XmlText $stage)</HostFolder><SandboxFolder>C:\VictoryStage</SandboxFolder><ReadOnly>true</ReadOnly></MappedFolder>
    <MappedFolder><HostFolder>$(XmlText $tools)</HostFolder><SandboxFolder>C:\VictoryTools</SandboxFolder><ReadOnly>true</ReadOnly></MappedFolder>
    <MappedFolder><HostFolder>$(XmlText $PSScriptRoot)</HostFolder><SandboxFolder>C:\VictoryScripts</SandboxFolder><ReadOnly>true</ReadOnly></MappedFolder>
    <MappedFolder><HostFolder>$(XmlText $output)</HostFolder><SandboxFolder>C:\VictoryLogs</SandboxFolder><ReadOnly>false</ReadOnly></MappedFolder>
  </MappedFolders>
  <LogonCommand><Command>$(XmlText $command)</Command></LogonCommand>
</Configuration>
"@
$configPath = Join-Path $output 'offline-first-run.wsb'
$config | Set-Content -LiteralPath $configPath -Encoding UTF8
[xml]$null = Get-Content -LiteralPath $configPath -Raw
Write-Output "Offline Sandbox configuration: $configPath"
Write-Output "Run by opening the .wsb file; diagnostic files appear in $output"
