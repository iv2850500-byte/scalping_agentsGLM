# ============================================================
# check_startup.ps1 (P47) - startup probe: surfaces config/serde
# errors that kill the process BEFORE logging init (bot.log never
# sees them). Runs the exe in foreground for 12s, captures
# STDOUT/STDERR, kills the process, prints everything.
# Guard: refuses within the first 20s of any minute (candle close
# at :00-:05 for 1m kline intervals - never touches a live signal).
# Usage: powershell -ExecutionPolicy Bypass -File .\check_startup.ps1
# ============================================================
 $ErrorActionPreference = 'Continue'
Set-Location 'C:\Users\BASE_PC\scalping_agentsGLM'
if ((Get-Date).Second -lt 20) { Write-Host '[probe] SKIP: inside the 20s pre-close window'; exit 1 }
 $exe = '.\target\release\scalping_agents.exe'
if (-not (Test-Path $exe)) { Write-Host '[probe] ABORT: exe not found - run cargo build first'; exit 1 }
Write-Host '[probe] foreground run for 12s, then auto-kill...'
 $psi = New-Object System.Diagnostics.ProcessStartInfo
 $psi.FileName = (Resolve-Path $exe).Path
 $psi.WorkingDirectory = (Get-Location).Path
 $psi.RedirectStandardOutput = $true
 $psi.RedirectStandardError = $true
 $psi.UseShellExecute = $false
 $p = [System.Diagnostics.Process]::Start($psi)
Start-Sleep -Seconds 12
 $alive = -not $p.HasExited
if ($alive) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue; Start-Sleep -Milliseconds 300 }
 $out = $p.StandardOutput.ReadToEnd()
 $err = $p.StandardError.ReadToEnd()
Write-Host '=== STDOUT (first 30 lines) ==='
 $out -split "`r?`n" | Select-Object -First 30
Write-Host '=== STDERR (first 30 lines) ==='
 $err -split "`r?`n" | Select-Object -First 30
if ($alive) { Write-Host '[probe] RESULT: ALIVE after 12s - startup OK (killed by probe)' }
else { Write-Host '[probe] RESULT: EXITED early - the reason is printed above' }