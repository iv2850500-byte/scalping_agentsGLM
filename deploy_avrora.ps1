# ============================================================
# deploy_avrora.ps1  -  ASCII only, PowerShell 5.1 safe
# Target banner (STRICT):  <U+1F680 rocket> AVRORA v0.3.0 | multi-agent
# Rocket is built at runtime: [char]::ConvertFromUtf32(0x1F680)
#
# This is now THE ONLY deploy script. deploy_aegis.ps1 and
# patch_banner_aegis.ps1 are OBSOLETE - do not run them.
#
# Run from project root:
#   powershell -ExecutionPolicy Bypass -File .\deploy_avrora.ps1
# ============================================================

 $ErrorActionPreference = 'Continue'
if ($PSScriptRoot) { Set-Location $PSScriptRoot } else { Set-Location 'C:\Users\BASE_PC\scalping_agentsGLM' }

# ---- target banner (STRICT, hardcoded version) ----
 $BOTNAME = 'AVRORA'
 $VERSION = '0.3.0'
 $rocket  = [char]::ConvertFromUtf32(0x1F680)
 $banner  = $rocket + ' ' + $BOTNAME + ' v' + $VERSION + ' | multi-agent'
 $ascii   = $BOTNAME + ' v' + $VERSION + ' | multi-agent'   # ASCII probe for exe/log scan

 $exe     = Join-Path (Get-Location) 'target\release\scalping_agents.exe'
 $srcPath = 'src\main.rs'
 $bakDir  = 'backup_p15'
 $bakFile = Join-Path $bakDir 'main.rs.avrora.bak'
 $startup = [Environment]::GetFolderPath('Startup')
 $bat     = Join-Path $startup 'scalping_agents_gate.bat'
 $batDis  = $bat + '.DISABLED'
 $outLog  = Join-Path (Get-Location) 'deploy_bot.log'
 $errLog  = Join-Path (Get-Location) 'deploy_bot.err.log'

function Step([string]$m) { Write-Host ('[' + (Get-Date -Format 'HH:mm:ss') + '] ' + $m) }

Step ('Target banner: <rocket> ' + $ascii + '  (console shows placeholder, file gets real emoji)')

# ---------- STEP 0: backup + version note ----------
if (-not (Test-Path $bakDir)) { New-Item $bakDir -ItemType Directory | Out-Null }
Copy-Item $srcPath $bakFile -Force
if (-not (Test-Path $bakFile)) { Step 'ABORT: backup failed'; exit 1 }
Step ('STEP0 OK: backup -> ' + $bakFile)
 $tv = (Get-Content 'Cargo.toml' -ErrorAction SilentlyContinue | Select-String -Pattern '^version' | Select-Object -First 1)
if ($tv -and ($tv.Line -notmatch '0\.3\.0')) { Step ('WARN: Cargo.toml version is not 0.3.0 (' + $tv.Line.Trim() + ') - banner is hardcoded so display is correct anyway') }

try {
    # ---------- STEP 1: park autostart insurance ----------
    Disable-ScheduledTask -TaskName 'ScalpWatchdog' -ErrorAction SilentlyContinue | Out-Null
    Disable-ScheduledTask -TaskName 'ScalpBackup'   -ErrorAction SilentlyContinue | Out-Null
    $wd = Get-ScheduledTask -TaskName 'ScalpWatchdog' -ErrorAction SilentlyContinue
    if ($wd -and $wd.State -ne 'Disabled') { Step 'WARN: ScalpWatchdog NOT disabled - exe lock risk during build' }
    if ((Test-Path $bat) -and (-not (Test-Path $batDis))) { Rename-Item $bat $batDis -Force -ErrorAction SilentlyContinue }
    Step 'STEP1 OK: ScalpWatchdog + ScalpBackup disabled, startup bat parked'

    # ---------- STEP 2: kill bot ----------
    Get-Process -Name 'scalping_agents' -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Seconds 2
    Step 'STEP2 OK: bot processes killed'

    # ---------- STEP 3: wait until exe unlocked (anti os error 5) ----------
    $unlocked = $false
    for ($i = 1; $i -le 15; $i++) {
        try { $fs = [System.IO.File]::Open($exe, 'Open', 'ReadWrite', 'None'); $fs.Close(); $unlocked = $true; break }
        catch {
            Get-Process -Name 'scalping_agents' -ErrorAction SilentlyContinue | Stop-Process -Force
            Start-Sleep -Seconds 1
        }
    }
    if (-not $unlocked) { Step 'ABORT: exe still locked after 15 s (os error 5 would follow)'; exit 1 }
    Step 'STEP3 OK: exe unlocked'

    # ---------- STEP 4: patch banner line (anchor: multi-agent) ----------
    $src = [System.IO.File]::ReadAllText((Resolve-Path $srcPath).Path)
    $rx  = [regex]'(?m)^[ \t]*tracing::info!\("[^"]*multi-agent[^"]*"[^\r\n]*\);'
    $hits = $rx.Matches($src)
    if ($hits.Count -ne 1) { Step ('ABORT: banner anchor hits = ' + $hits.Count + ' (need exactly 1) - nothing written'); exit 1 }
    $newLine = 'tracing::info!("' + $banner + '");'
    $new = $rx.Replace($src, $newLine, 1)
    [System.IO.File]::WriteAllText((Resolve-Path $srcPath).Path, $new, (New-Object System.Text.UTF8Encoding($false)))
    if (-not (Select-String -Path $srcPath -Pattern $ascii -SimpleMatch -Quiet)) { Step 'ABORT: banner verify after write failed'; exit 1 }
    Step 'STEP4 OK: banner line rewritten, exactly one line changed, P15 logic untouched'

    # ---------- STEP 5: cargo check (gate) ----------
    Step 'STEP5: cargo check ...'
    $ck = cargo check 2>&1
    $ck | Select-Object -Last 4
    if ($LASTEXITCODE -ne 0) { $ck | Select-Object -Last 30; Step 'ABORT: cargo check failed - exe NOT started'; exit 1 }
    Step 'STEP5 OK: cargo check passed'

    # ---------- STEP 6: release build (gate) ----------
    Step 'STEP6: cargo build --release --bin scalping_agents (3-4 min, DO NOT close this window) ...'
    $bd = cargo build --release --bin scalping_agents 2>&1
    $bd | Select-Object -Last 6
    if ($LASTEXITCODE -ne 0) { $bd | Select-Object -Last 30; Step 'ABORT: build failed - exe NOT started'; exit 1 }
    Step 'STEP6 OK: build passed'

    # ---------- STEP 7: preflight ----------
    if (-not (Select-String -Path $exe -Pattern $ascii -SimpleMatch -Quiet)) { Step 'ABORT: new banner NOT found inside exe'; exit 1 }
    $p = Get-NetTCPConnection -LocalPort 9090 -State Listen -ErrorAction SilentlyContinue
    if ($p) { Step ('ABORT: port 9090 still busy, PID ' + $p[0].OwningProcess); exit 1 }
    Step 'STEP7 OK: preflight passed (banner inside exe, port 9090 free)'

    # ---------- STEP 8: start bot ----------
    Start-Process -FilePath $exe -WorkingDirectory (Get-Location) -WindowStyle Hidden -RedirectStandardOutput $outLog -RedirectStandardError $errLog
    Start-Sleep -Seconds 7
    $alive = Get-Process -Name 'scalping_agents' -ErrorAction SilentlyContinue
    if (-not $alive) { Step 'ABORT: bot died right after start - run .\target\release\scalping_agents.exe in a console to see the error'; exit 1 }
    Step ('STEP8 OK: bot started, PID ' + $alive[0].Id)
    if (Test-Path $outLog) {
        if (Select-String -Path $outLog -Pattern $ascii -SimpleMatch -Quiet) { Step 'LIVE LOG: banner confirmed in runtime output' }
    }
    Start-Sleep -Seconds 3
    $w = Get-NetTCPConnection -LocalPort 9090 -State Listen -ErrorAction SilentlyContinue
    if ($w) { Step ('WEBUI OK: 127.0.0.1:9090 LISTENING (PID ' + $w[0].OwningProcess + ')') }
    else { Step 'WARN: 9090 not listening yet - bot still initializing, recheck in 30 s' }

    Step ('DONE. Banner is now: <rocket> ' + $ascii)
}
finally {
    Enable-ScheduledTask -TaskName 'ScalpWatchdog' -ErrorAction SilentlyContinue | Out-Null
    Enable-ScheduledTask -TaskName 'ScalpBackup'   -ErrorAction SilentlyContinue | Out-Null
    if ((-not (Test-Path $bat)) -and (Test-Path $batDis)) { Rename-Item $batDis $bat -Force -ErrorAction SilentlyContinue }
    Step 'INSURANCE RESTORED: ScalpWatchdog + ScalpBackup enabled, startup bat back in place'
}