# ============================================================
#  deploy_aegis.ps1   [P15 finisher]
#  Fix banner -> cargo check -> release build -> launch
#  GATES: on ANY failure the exe is NOT started.
#  Run: powershell -ExecutionPolicy Bypass -File .\deploy_aegis.ps1
# ============================================================

 $BOTNAME = 'AVRORA'   # <- esli khochesh imya AVRORA: zameni zdes i vsyo

 $ErrorActionPreference = 'Stop'
 $root = 'C:\Users\BASE_PC\scalping_agentsGLM'
 $exe  = Join-Path $root 'target\release\scalping_agents.exe'
 $main = Join-Path $root 'src\main.rs'
Set-Location $root

Write-Host '============ step 1/4: banner line 107 ============'
 $src    = [IO.File]::ReadAllText($main)
 $rocket = [char]::ConvertFromUtf32(0x1F680)
 $newCall = 'tracing::info!("' + $rocket + ' ' + $BOTNAME + ' v{} | multi-agent futures scalper | isolated bots | risk-gates", env!("CARGO_PKG_VERSION"));'

 $rx = [regex]'(?m)^[ \t]*tracing::info!\("[^"]*multi-agent futures scalper[^"]*"[^\r\n]*\);'
 $ms = $rx.Matches($src)
if ($ms.Count -eq 0) { Write-Host '  [X] banner line not found - abort'; exit 1 }
if ($ms.Count -gt 1) { Write-Host ('  [X] banner not unique: ' + $ms.Count + ' - abort'); exit 1 }
 $old = $ms[0].Value
if ($old -eq $newCall) {
  Write-Host '  [skip] banner already correct'
} else {
  Write-Host '  OLD:' $old
  $bakDir = Join-Path $root 'backup_p15'
  New-Item -ItemType Directory -Force -Path $bakDir | Out-Null
  [IO.File]::WriteAllText((Join-Path $bakDir 'main.rs.deploy.bak'), $src, (New-Object System.Text.UTF8Encoding($false)))
  $src = $src.Remove($ms[0].Index, $old.Length).Insert($ms[0].Index, $newCall)
  [IO.File]::WriteAllText($main, $src, (New-Object System.Text.UTF8Encoding($false)))
  Write-Host '  [ok] banner fixed'
}

Write-Host '============ step 2/4: cargo check (syntax gate) ============'
 $ErrorActionPreference = 'Continue'
cargo check
if ($LASTEXITCODE -ne 0) { Write-Host '  [X] cargo check FAILED - ABORT, exe will NOT start'; exit 1 }
Write-Host '  [ok] cargo check passed'

Write-Host '============ step 3/4: release build ============'
cargo build --release --bin scalping_agents
if ($LASTEXITCODE -ne 0) { Write-Host '  [X] build FAILED - ABORT, exe will NOT start'; exit 1 }
Write-Host '  [ok] release build OK'

Write-Host '============ step 4/4: pre-flight + launch ============'
 $binHit = Select-String -Path $exe -Pattern $BOTNAME -Quiet
if ($binHit) { Write-Host '  [ok] binary contains banner string' } else { Write-Host '  [!] banner string not found in binary (warning)' }
Get-Process -Name scalping_agents -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2
 $port = netstat -ano | findstr :9090
if ($port) { Write-Host '  [X] port 9090 busy - ABORT:'; $port; exit 1 }
Write-Host ('  exe built: ' + (Get-Item $exe).LastWriteTime)
Write-Host '  LAUNCHING...'
& $exe