# Called by a network-disabled Windows Sandbox configuration generated on host.
param([ValidateRange(1, 300)] [int] $TimeoutSeconds = 30)
$ErrorActionPreference = 'Stop'
$exe = 'C:\VictoryStage\game_scarlett_master.exe'
$logs = 'C:\VictoryLogs'
$wfp = 'C:\VictoryTools\victory-offline-wfp.exe'
$launcher = 'C:\VictoryTools\victory-first-run.exe'
$guestLog = Join-Path $logs 'sandbox-guest.log'
try {
    "started_utc=$((Get-Date).ToUniversalTime().ToString('o'))" | Set-Content -LiteralPath $guestLog
    "network_adapters=$((@(Get-NetAdapter -ErrorAction SilentlyContinue | Where-Object Status -eq 'Up')).Count)" |
        Add-Content -LiteralPath $guestLog
    & $wfp add $exe *>&1 | Out-File -LiteralPath (Join-Path $logs 'guest-wfp.log')
    if ($LASTEXITCODE -ne 0) { throw 'Guest WFP block could not be installed' }
    & $wfp check $exe *>> (Join-Path $logs 'guest-wfp.log')
    if ($LASTEXITCODE -ne 0) { throw 'Guest WFP block could not be verified' }
    $started = Get-Date
    & $launcher $exe $logs $TimeoutSeconds
    $code = $LASTEXITCODE
    "launcher_exit_code=$code" | Add-Content -LiteralPath $guestLog
    $events = @(Get-WinEvent -FilterHashtable @{ LogName = 'Application'; Id = 1000; StartTime = $started.AddSeconds(-2) } -ErrorAction SilentlyContinue |
        Where-Object { $_.ProviderName -eq 'Application Error' -and $_.Message.Contains($exe) } |
        Select-Object -First 5)
    if ($events.Count) {
        $events | ForEach-Object { "[$($_.TimeCreated)] $($_.Message)" } |
            Set-Content -LiteralPath (Join-Path $logs 'application-errors.log')
    }
    "completed_utc=$((Get-Date).ToUniversalTime().ToString('o'))" | Add-Content -LiteralPath $guestLog
} catch {
    ($_ | Out-String) | Add-Content -LiteralPath $guestLog
    'title_was_not_started_or_run_failed=true' | Add-Content -LiteralPath $guestLog
}
