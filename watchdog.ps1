# watchdog.ps1 - P26 ops pack (roadmap #13). Liveness: process + log freshness
# + webui port. Actions: Telegram push (TG_BOT_TOKEN / TG_CHAT_ID from .env) and
# fast restart via deploy.ps1 -SkipBuild. Cleanup: removes logs older than 30 days.
# Usage:
#   .\watchdog.ps1            full run: check + act
#   .\watchdog.ps1 -TestOnly  checks only, prints verdict, never acts
# Scheduled task (every 5 min): see README_FIXPACK_P26_OPS.md
param(
    [switch]$TestOnly
)
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $root
$logDir = Join-Path $root 'logs'
$STALE_SEC = 300
$RECHECK_SEC = 20

function Send-TG([string]$text) {
    # Reads .env locally, never prints token values.
    $envFile = Join-Path $root '.env'
    if (-not (Test-Path $envFile)) { Write-Host '[tg] no .env - skip'; return }
    $tok = ''
    $chat = ''
    Get-Content $envFile | ForEach-Object {
        if ($_ -match '^\s*TG_BOT_TOKEN\s*=\s*(.+?)\s*$') { $tok = $Matches[1].Trim('"') }
        if ($_ -match '^\s*TG_CHAT_ID\s*=\s*(.+?)\s*$')   { $chat = $Matches[1].Trim('"') }
    }
    if ($tok -eq '' -or $chat -eq '') { Write-Host '[tg] TG_BOT_TOKEN / TG_CHAT_ID not found - skip'; return }
    $uri = 'https://api.telegram.org/bot' + $tok + '/sendMessage'
    $body = @{ chat_id = $chat; text = $text }
    try {
        Invoke-RestMethod -Uri $uri -Method Post -Body $body -TimeoutSec 6 | Out-Null
        Write-Host '[tg] sent'
    } catch {
        Write-Host ('[tg] failed: ' + $_.Exception.Message)
    }
}

function Get-LogAgeSec {
    $latest = Get-ChildItem $logDir -Filter 'bot.log*' -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $latest) { return -1 }
    return [int]((Get-Date) - $latest.LastWriteTime).TotalSeconds
}

function Test-Port9090 {
    $tcp = New-Object Net.Sockets.TcpClient
    try {
        $ar = $tcp.BeginConnect('127.0.0.1', 9090, $null, $null)
        $ok = $ar.AsyncWaitHandle.WaitOne(2000)
        if ($ok -and $tcp.Connected) { return $true }
        return $false
    } catch {
        return $false
    } finally {
        $tcp.Close()
    }
}

# --- checks ---
$dead = $true
$proc = Get-Process -Name 'scalping_agents' -ErrorAction SilentlyContinue | Select-Object -First 1
if ($proc) { $dead = $false }

$age = Get-LogAgeSec
$stale = $true
if ($age -lt 0) {
    Write-Host '[watchdog] no log file in logs\'
} else {
    Write-Host ('[watchdog] log age: ' + $age + 's (limit ' + $STALE_SEC + 's)')
    $stale = ($age -gt $STALE_SEC)
}
$port = Test-Port9090
Write-Host ('[watchdog] webui 9090 answering: ' + $port)

# zombie guard: process alive but log stale -> re-check once before restart
$staleConfirmed = $stale
if ($stale -and (-not $dead) -and (-not $TestOnly)) {
    Write-Host ('[watchdog] log stale but process alive - re-check in ' + $RECHECK_SEC + 's')
    Start-Sleep -Seconds $RECHECK_SEC
    $age2 = Get-LogAgeSec
    $staleConfirmed = ($age2 -lt 0) -or ($age2 -gt ($STALE_SEC + $RECHECK_SEC))
    Write-Host ('[watchdog] re-check: log age now ' + $age2 + 's, staleConfirmed=' + $staleConfirmed)
}

# --- verdict / actions ---
if ($TestOnly) {
    if ($dead) { Write-Host '[watchdog] TEST verdict: FAIL (process dead)' }
    elseif ($stale) { Write-Host '[watchdog] TEST verdict: FAIL (log stale)' }
    elseif (-not $port) { Write-Host '[watchdog] TEST verdict: WARN (port down, process alive)' }
    else { Write-Host '[watchdog] TEST verdict: OK' }
    exit 0
}

if ($dead -or $staleConfirmed) {
    $reason = 'process dead'
    if (-not $dead) { $reason = 'log stale > ' + $STALE_SEC + 's (zombie)' }
    Send-TG ('[WATCHDOG] bot DOWN: ' + $reason + '. Restarting.')
    Write-Host ('[watchdog] ACTION: restart (' + $reason + ')')
    & (Join-Path $root 'deploy.ps1') -SkipBuild
    Send-TG ('[WATCHDOG] restart issued ' + (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'))
} elseif (-not $port) {
    Send-TG '[WATCHDOG] webui 9090 not answering, process alive and log fresh - manual check advised'
    Write-Host '[watchdog] port down, no restart (process alive, log fresh)'
} else {
    Write-Host '[watchdog] OK - all green'
}

# --- cleanup: logs older than 30 days ---
$cutoff = (Get-Date).AddDays(-30)
$old = Get-ChildItem $logDir -Filter 'bot.log*' -ErrorAction SilentlyContinue | Where-Object { $_.LastWriteTime -lt $cutoff }
$n = 0
foreach ($f in $old) { Remove-Item $f.FullName -Force; $n++ }
if ($n -gt 0) { Write-Host ('[watchdog] cleaned ' + $n + ' old log files') }
