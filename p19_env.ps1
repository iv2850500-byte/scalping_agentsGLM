# ============================================================
# p19_env.ps1  (config-only fix, NO build)
# .env: RECONCILE_STRICT=0 -> reconcile failure falls back to
# local StateManager instead of skipping the entry (testnet).
# ============================================================
 $ErrorActionPreference = 'Continue'
 $envFile = Join-Path (Get-Location) '.env'

Write-Host '[p19] .env check:'
if (Test-Path $envFile) {
  $cur = Select-String -Path $envFile -Pattern 'RECONCILE_STRICT'
  if ($cur) { $cur | ForEach-Object { Write-Host ('  now: ' + $_.Line) } }
  else { Write-Host '  now: no RECONCILE_STRICT line (default strict=1 active)' }
} else {
  Write-Host '  .env NOT FOUND - will create'
}

if (Test-Path $envFile) {
  Copy-Item $envFile ($envFile + '.bak-p19') -Force
  Write-Host '[p19] backup -> .env.bak-p19'
}

 $want = 'RECONCILE_STRICT=0'
if (Test-Path $envFile) {
  $lines = Get-Content $envFile -Encoding UTF8
  $found = $false
  $new = foreach ($l in $lines) {
    if ($l -match '^\s*RECONCILE_STRICT\s*=' -or $l -match '^\s*#\s*RECONCILE_STRICT\s*=') {
      $found = $true; $want
    } else { $l }
  }
  if ($found) {
    [System.IO.File]::WriteAllLines($envFile, [string[]]$new)
  } else {
    [System.IO.File]::AppendAllText($envFile, "`r`n" + $want + "`r`n")
  }
} else {
  [System.IO.File]::WriteAllText($envFile, $want + "`r`n")
}
Write-Host ('[p19] set: ' + $want)

# guarantee for THIS restart too (session env inherits into deploy -> bot)
 $env:RECONCILE_STRICT = '0'

Write-Host '[p19] restarting bot via p16_deploy.ps1 ...'
powershell -ExecutionPolicy Bypass -File .\p16_deploy.ps1

Start-Sleep -Seconds 15
 $today = Get-Date -Format 'yyyy-MM-dd'
 $log = Join-Path 'logs' ('bot.log.' + $today)
Write-Host '[p19] verify in fresh log:'
Get-Content $log -Encoding UTF8 | Select-String 'RECONCILE_STRICT' | Select-Object -Last 3
Write-Host ''
Write-Host '[p19] Expect: RECONCILE_STRICT = false  -> fix is LIVE'
Write-Host '[p19] If still true -> tell me, plan B ready'