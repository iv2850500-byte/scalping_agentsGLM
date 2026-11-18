# =====================================================================
# P61/2: build.ps1 - one-command bot update (replaces the manual chain)
#   stop bot -> cargo test --lib -> cargo build --release -> deploy -SkipBuild
# Why: Windows locks the image of a running .exe; building while the bot is
# alive fails with "os error 5" (Access denied). This script enforces the
# safe order Stop -> Build -> Deploy automatically.
# Usage:
#   powershell -ExecutionPolicy Bypass -File .\build.ps1
#   powershell -ExecutionPolicy Bypass -File .\build.ps1 -SkipTests
# Fail-fast: if tests or build fail, the bot stays STOPPED; fix and re-run.
# =====================================================================
param([switch]$SkipTests)

$root = $PSScriptRoot
Set-Location $root

# ---------- P61/2s: stop bot + wait for exe unlock (mirror of deploy P61 block) ----------
$p61Procs = @(Get-Process -Name 'scalping_agents' -ErrorAction SilentlyContinue)
$p61Procs += @(Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Path -like '*scalping_agents*' -and $_.Id -ne $PID })
$p61Procs = @($p61Procs | Where-Object { $null -ne $_ } | Sort-Object Id -Unique)
foreach ($p61p in $p61Procs) {
    Write-Host ("[build] P61 auto-stop: stopping PID {0} ({1})" -f $p61p.Id, $p61p.ProcessName)
    try { Stop-Process -Id $p61p.Id -Force -ErrorAction Stop } catch { Write-Host ("[build] P61 warn: {0}" -f $_.Exception.Message) }
}
if ($p61Procs.Count -gt 0) { Start-Sleep -Seconds 2 }
$p61Exe = Join-Path $root 'target\release\scalping_agents.exe'
$p61Deadline = (Get-Date).AddSeconds(15)
while (Test-Path $p61Exe) {
    try { $p61fs = [System.IO.File]::Open($p61Exe, 'Open', 'ReadWrite', 'None'); $p61fs.Close(); break }
    catch {
        if ((Get-Date) -gt $p61Deadline) { Write-Host '[build] P61 ERROR: exe still locked after 15 s - aborting'; exit 1 }
        Start-Sleep -Milliseconds 500
    }
}
if ($p61Procs.Count -gt 0) { Write-Host '[build] bot stopped, exe unlocked' }

# ---------- step 1/3: unit tests ----------
if (-not $SkipTests) {
    Write-Host '[build] step 1/3: cargo test --lib'
    & cargo test --lib
    if ($LASTEXITCODE -ne 0) { Write-Host '[build] FAIL: tests failed - bot stays stopped. Fix, then re-run build.ps1'; exit 1 }
} else {
    Write-Host '[build] step 1/3: tests skipped (-SkipTests)'
}

# ---------- step 2/3: release build ----------
Write-Host '[build] step 2/3: cargo build --release'
& cargo build --release
if ($LASTEXITCODE -ne 0) { Write-Host '[build] FAIL: build failed - bot stays stopped. Fix, then re-run build.ps1'; exit 1 }

# ---------- step 3/3: deploy (its P61 block is a no-op now, then it starts the bot) ----------
Write-Host '[build] step 3/3: deploy.ps1 -SkipBuild (starts the bot)'
& powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $root 'deploy.ps1') -SkipBuild
if ($LASTEXITCODE -ne 0) { Write-Host '[build] WARN: deploy.ps1 exited nonzero - check output above'; exit 1 }
Write-Host '[build] OK: bot updated and running'
