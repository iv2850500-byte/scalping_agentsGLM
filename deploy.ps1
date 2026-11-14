# deploy.ps1 - P26 ops pack (roadmap #8). Stop bot -> build -> start -> verify.
# Usage:
#   .\deploy.ps1              full deploy: stop + build + start + verify
#   .\deploy.ps1 -SkipBuild   fast restart without build (watchdog uses this)
#   .\deploy.ps1 -BuildOnly   build only, running bot is NOT touched
# Note: starting from P26 the recommended way to run the bot is this script
# (direct exe, not `cargo run --release`), so the process tree is simple.
param(
    [switch]$SkipBuild,
    [switch]$BuildOnly
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $root
$exe = Join-Path $root 'target\release\scalping_agents.exe'
# ===================== P61/1: auto-stop bot before build/copy (fix os error 5) =====================
# Kills any running bot instance (by process name or by exe path) and waits
# until target\release\scalping_agents.exe is writable. Windows locks the image
# of a running .exe, so building/copying while the bot is alive fails with
# "os error 5" (Access denied). Fail-closed: if the exe is still locked after
# 15 s, deploy aborts BEFORE overwriting anything.
$p61Procs = @(Get-Process -Name 'scalping_agents' -ErrorAction SilentlyContinue)
$p61Procs += @(Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Path -like '*scalping_agents*' -and $_.Id -ne $PID })
$p61Procs = @($p61Procs | Where-Object { $null -ne $_ } | Sort-Object Id -Unique)
foreach ($p61p in $p61Procs) {
    Write-Host ("[deploy] P61 auto-stop: stopping PID {0} ({1})" -f $p61p.Id, $p61p.ProcessName)
    try { Stop-Process -Id $p61p.Id -Force -ErrorAction Stop } catch { Write-Host ("[deploy] P61 warn: {0}" -f $_.Exception.Message) }
}
if ($p61Procs.Count -gt 0) { Start-Sleep -Seconds 2 }
$p61Exe = Join-Path $PSScriptRoot 'target\release\scalping_agents.exe'
$p61Deadline = (Get-Date).AddSeconds(15)
while (Test-Path $p61Exe) {
    try { $p61fs = [System.IO.File]::Open($p61Exe, 'Open', 'ReadWrite', 'None'); $p61fs.Close(); break }   # P61/1w: exe image lock is gone -> go on
    catch {
        if ((Get-Date) -gt $p61Deadline) { Write-Host '[deploy] P61 ERROR: exe still locked after 15 s - deploy aborted (nothing overwritten)'; exit 1 }
        Start-Sleep -Milliseconds 500
    }
}
if ($p61Procs.Count -gt 0) { Write-Host '[deploy] P61: bot stopped, exe unlocked' }
# ===================== end P61/1 =====================

Write-Host "[deploy] root = $root"

if ($BuildOnly) {
    Write-Host "[deploy] build-only: running bot is NOT touched"
    & cargo build --release
    if ($LASTEXITCODE -ne 0) { Write-Host "[deploy] BUILD FAILED"; exit 1 }
    Write-Host "[deploy] build OK"
    exit 0
}

# --- 1) stop running bot ---
# Hard stop is acceptable by design: exchange-side STOP_MARKET orders stay live,
# open positions are adopted on next start (adoption reads the live STOP_MARKET,
# verified in P22-B).
$procs = Get-Process -Name 'scalping_agents' -ErrorAction SilentlyContinue
if ($procs) {
    foreach ($p in $procs) {
        Write-Host "[deploy] stopping PID $($p.Id)"
        Stop-Process -Id $p.Id -Force
    }
    Start-Sleep -Seconds 3
} else {
    Write-Host "[deploy] bot not running"
}

# --- 2) build ---
if (-not $SkipBuild) {
    Write-Host "[deploy] cargo build --release (5-7 min, do not close window)"
    & cargo build --release
    if ($LASTEXITCODE -ne 0) {
        Write-Host "[deploy] BUILD FAILED - bot NOT started. Old exe is still in place."
        exit 1
    }
}

# --- 3) start (direct exe; the bot loads .env itself from project root) ---
if (-not (Test-Path $exe)) {
    Write-Host "[deploy] exe not found: $exe - run without -SkipBuild first"
    exit 1
}
$p = Start-Process -FilePath $exe -WorkingDirectory $root -WindowStyle Minimized -PassThru
Write-Host "[deploy] started PID $($p.Id) at $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')"

# --- 4) verify + first log lines ---
Start-Sleep -Seconds 15
if (Get-Process -Id $p.Id -ErrorAction SilentlyContinue) {
    Write-Host "[deploy] OK: process alive after 15s"
} else {
    Write-Host "[deploy] FAIL: process died within 15s - see log"
    exit 1
}
$latest = Get-ChildItem (Join-Path $root 'logs') -Filter 'bot.log*' -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1
if ($latest) {
    Write-Host "[deploy] --- last 25 lines of $($latest.Name) ---"
    Get-Content -Path $latest.FullName -Tail 25 -Encoding UTF8
} else {
    Write-Host "[deploy] WARN: no log file yet"
}
