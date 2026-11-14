# ============================================================
# P64: supervisor.ps1 - process-level supervisor for scalping_agents.exe
# ============================================================
# WHY: the in-process supervisor (main.rs, AUDIT-P6) restarts bot TASKS,
# but if the whole PROCESS dies (panic in runtime, OOM, power, aborted
# update) or WEDGES (event loop stalls while process is alive), nothing
# brings it back. This script supervises at the process level:
#
#   - starts target\release\scalping_agents.exe (config.toml from CWD);
#   - restarts on crash with backoff 5->10->20->...->300s (reset after
#     10 min of stable uptime);
#   - restart budget: max -MaxRestartsPerHour per rolling hour, then
#     gives up with Telegram alert (bot DOWN, check positions manually);
#   - STALL WATCHDOG: the bot writes a heartbeat line to logs\bot.log.*
#     every 60s (websocket hub). If the newest log file is older than
#     max(180, 3*HEARTBEAT_SEC+60) seconds while the process is alive,
#     the process is considered wedged -> killed -> restarted;
#   - exit-code classification (P64 AppError / E5b):
#       0   clean stop (deploy.ps1/build.ps1) -> supervisor EXITS;
#       1   FATAL (config/keys, AppError)     -> NO restart, alert, exit;
#       2   transient (AppError)              -> restart with backoff;
#       3   watchdog stall                    -> restart;
#       101  Rust panic (default runtime)     -> restart;
#       other -> restart.
#
# COORDINATION:
#   - KILL file (kill-switch, P11)   -> bot is NOT started while present;
#   - PAUSE file (maintenance gate)  -> supervisor idles; create it BEFORE
#     build.ps1 / deploy.ps1, delete after. Otherwise the supervisor will
#     restart the bot mid-build and the linker will hit a locked exe.
#       Before build:  New-Item -ItemType File PAUSE
#       After build:   Remove-Item PAUSE
#   - single instance: supervisor.pid lock file.
#
# TELEGRAM ALERTS (best-effort, never fails the loop):
#   env TG_BOT_TOKEN + TG_CHAT_ID (+ optional TG_PROXY=http://127.0.0.1:10809
#   for RF - Invoke-RestMethod supports http proxies only).
#
# USAGE:
#   powershell -ExecutionPolicy Bypass -File .\supervisor.ps1
#   powershell -ExecutionPolicy Bypass -File .\supervisor.ps1 -HeartbeatSec 60 -MaxRestartsPerHour 10
#   powershell -ExecutionPolicy Bypass -File .\supervisor.ps1 -StopSupervisor
# Do NOT run deploy.ps1 and supervisor.ps1 at the same time: exactly one
# of them must own the bot process.
# ============================================================
param(
    [string]$Exe = "target\release\scalping_agents.exe",
    [int]$HeartbeatSec = 60,
    [int]$MaxRestartsPerHour = 10,
    [int]$PollSec = 30,
    [switch]$StopSupervisor
)

$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
Set-Location $root
$logDir = Join-Path $root "logs"
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
$supLog = Join-Path $logDir ("supervisor.log." + (Get-Date -Format "yyyy-MM-dd"))

function Write-Sup([string]$level, [string]$msg) {
    $line = "{0} [SUP/{1}] {2}" -f (Get-Date -Format "yyyy-MM-dd HH:mm:ss"), $level, $msg
    Write-Host $line
    Add-Content -Path $supLog -Value $line -Encoding UTF8
}

function Send-Tg([string]$text) {
    # best-effort Telegram alert; any error is swallowed on purpose
    try {
        $token = $env:TG_BOT_TOKEN
        $chat = $env:TG_CHAT_ID
        if (-not $token -or -not $chat) { return }
        $uri = "https://api.telegram.org/bot$token/sendMessage"
        $body = @{ chat_id = $chat; text = $text }
        if ($env:TG_PROXY) {
            Invoke-RestMethod -Method Post -Uri $uri -Body $body -TimeoutSec 8 -Proxy $env:TG_PROXY | Out-Null
        } else {
            Invoke-RestMethod -Method Post -Uri $uri -Body $body -TimeoutSec 8 | Out-Null
        }
    } catch { }
}

$pidFile = Join-Path $root "supervisor.pid"

if ($StopSupervisor) {
    if (Test-Path $pidFile) {
        $oldPid = Get-Content $pidFile -ErrorAction SilentlyContinue
        if ($oldPid) {
            $oldProc = Get-Process -Id ([int]$oldPid) -ErrorAction SilentlyContinue
            if ($oldProc) {
                Write-Sup "WARN" "stopping supervisor pid $oldPid"
                Stop-Process -Id ([int]$oldPid) -Force -ErrorAction SilentlyContinue
            } else {
                Write-Sup "INFO" "supervisor pid $oldPid not alive"
            }
        }
        Remove-Item $pidFile -ErrorAction SilentlyContinue
    } else {
        Write-Sup "INFO" "no supervisor.pid - nothing to stop"
    }
    exit 0
}

# ---- single instance lock ----
if (Test-Path $pidFile) {
    $oldPid = Get-Content $pidFile -ErrorAction SilentlyContinue
    if ($oldPid) {
        $oldProc = Get-Process -Id ([int]$oldPid) -ErrorAction SilentlyContinue
        if ($oldProc) {
            Write-Sup "ERR" "supervisor already running (pid $oldPid) - exit (use -StopSupervisor first)"
            exit 1
        }
    }
}
$supPid = $PID
$supPid | Set-Content -Path $pidFile -Encoding ASCII

$botPath = Join-Path $root $Exe
if (-not (Test-Path $botPath)) {
    Write-Sup "ERR" "bot exe not found: $botPath (run build.ps1 first)"
    Remove-Item $pidFile -ErrorAction SilentlyContinue
    exit 1
}

function Test-KillFile { return (Test-Path (Join-Path $root "KILL")) }
function Test-PauseFile { return (Test-Path (Join-Path $root "PAUSE")) }

function Get-BotLogAge {
    # age in seconds of the newest bot.log.* file; $null if no logs at all
    $latest = Get-ChildItem -Path $logDir -Filter "bot.log.*" -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $latest) { return $null }
    return ((Get-Date) - $latest.LastWriteTime).TotalSeconds
}

$restartTimes = New-Object System.Collections.Generic.List[DateTime]
function Get-FreshRestartCount {
    $cut = (Get-Date).AddHours(-1)
    while ($restartTimes.Count -gt 0 -and $restartTimes[0] -lt $cut) { $restartTimes.RemoveAt(0) }
    return $restartTimes.Count
}

$staleAfter = [Math]::Max(180, 3 * $HeartbeatSec + 60)
$backoffSec = 5

Write-Sup "INFO" ("supervisor started (pid {0}) | exe={1} | heartbeat={2}s | stale after {3}s | max restarts/hour={4}" -f $supPid, $Exe, $HeartbeatSec, $staleAfter, $MaxRestartsPerHour)
Send-Tg "AVRORA supervisor: started"

$stopSup = $false
while (-not $stopSup) {
    if (Test-PauseFile) {
        Write-Sup "INFO" "PAUSE present - supervisor idle (delete PAUSE to resume)"
        Start-Sleep -Seconds 5
        continue
    }
    if (Test-KillFile) {
        Write-Sup "INFO" "KILL present (kill-switch) - bot NOT started"
        Start-Sleep -Seconds 10
        continue
    }

    Write-Sup "INFO" ("starting bot: {0}" -f $botPath)
    $proc = Start-Process -FilePath $botPath -WorkingDirectory $root -PassThru -WindowStyle Hidden
    $startedAt = Get-Date
    Write-Sup "INFO" ("bot started pid={0}" -f $proc.Id)

    $killed = $false
    while ($true) {
        Start-Sleep -Seconds $PollSec
        if ($proc.HasExited) { break }
        $age = Get-BotLogAge
        if ($null -ne $age -and $age -gt $staleAfter) {
            Write-Sup "WARN" ("log stale {0:N0}s (threshold {1}s) while process alive - bot wedged, killing pid {2}" -f $age, $staleAfter, $proc.Id)
            Send-Tg ("AVRORA supervisor: bot WEDGED (log stale {0}s) - killing and restarting" -f [int]$age)
            try { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue } catch { }
            try { $proc.WaitForExit(10000) | Out-Null } catch { }
            $killed = $true
            break
        }
    }

    $uptimeMin = ((Get-Date) - $startedAt).TotalMinutes
    $exitCode = -1
    if ($proc.HasExited) { $exitCode = $proc.ExitCode }

    if ($killed) {
        $reason = "stall-watchdog kill"
        $doRestart = $true
    } elseif ($exitCode -eq 0) {
        $reason = "clean exit (code 0)"
        $doRestart = $false
    } elseif ($exitCode -eq 1) {
        $reason = "FATAL exit (code 1: config/keys)"
        $doRestart = $false
    } else {
        $reason = "exit code $exitCode"
        $doRestart = $true
    }

    Write-Sup "WARN" ("bot exited: {0} after {1:N1} min | restart={2}" -f $reason, $uptimeMin, $doRestart)

    if (-not $doRestart) {
        if ($exitCode -eq 1) {
            Send-Tg "AVRORA supervisor: bot FATAL exit ($reason) - NOT restarting. Fix config/keys, then start supervisor again."
        } else {
            Send-Tg "AVRORA supervisor: bot stopped cleanly ($reason) - supervisor exits"
        }
        $stopSup = $true
        break
    }

    if ((Get-FreshRestartCount) -ge $MaxRestartsPerHour) {
        Send-Tg ("AVRORA supervisor: restart budget exceeded ({0}/hour) - GIVING UP. Bot is DOWN, check positions on the exchange!" -f $MaxRestartsPerHour)
        Write-Sup "ERR" "restart budget exceeded - supervisor stops"
        $stopSup = $true
        break
    }
    $restartTimes.Add((Get-Date))

    Send-Tg ("AVRORA supervisor: bot {0} - restarting ({1}/{2} in the last hour)" -f $reason, (Get-FreshRestartCount), $MaxRestartsPerHour)

    if ($uptimeMin -ge 10) { $backoffSec = 5 }  # stable run -> reset backoff
    Write-Sup "INFO" ("restart in {0}s" -f $backoffSec)
    Start-Sleep -Seconds $backoffSec
    $backoffSec = [Math]::Min(300, $backoffSec * 2)
}

Remove-Item $pidFile -ErrorAction SilentlyContinue
Write-Sup "INFO" "supervisor stopped"
