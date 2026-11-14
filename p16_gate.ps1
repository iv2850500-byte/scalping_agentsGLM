# ============================================================
# p16_gate.ps1  (PS 5.1, ASCII only)
# P16: equity-based position sizing.
# Gate = backup -> patch -> cargo check -> build (side dir).
# Does NOT touch the running bot. Deploy = p16_deploy.ps1 (separate step).
# Run:  powershell -ExecutionPolicy Bypass -File .\p16_gate.ps1
# ============================================================
 $ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

if (-not (Test-Path 'src\main.rs')) { throw 'run this script from the project root' }

 $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
 $bdir  = "backups\p16-$stamp"
New-Item -ItemType Directory -Force -Path $bdir | Out-Null
Write-Host "[gate] backup dir: $bdir"
 $script:patched = 0
 $script:skipped = 0

function Patch-File {
  param([string]$Path, [string[]]$Old, [string[]]$New)

  if (-not (Test-Path $Path)) { throw "patch: file not found: $Path" }
  $rp  = (Resolve-Path $Path).Path
  $raw = [IO.File]::ReadAllText($rp)
  $norm = $raw.Replace("`r`n", "`n")

  $oldN = ($Old -join "`n").Replace("`r`n", "`n")
  $newN = ($New -join "`n").Replace("`r`n", "`n")

  # idempotency: new text already in place -> skip (safe to re-run)
  if ($norm.Contains($newN)) {
    Write-Host "[patch] $Path :: already applied, skip"
    $script:skipped = $script:skipped + 1
    return
  }

  if (-not $norm.Contains($oldN)) {
    $probe = ($oldN -split "`n")[0].Trim()
    if ($probe.Length -gt 40) { $probe = $probe.Substring(0, 40) }
    Write-Host "[patch] $Path :: ANCHOR NOT FOUND, probing: '$probe'"
    if ($probe) {
      Select-String -Path $rp -SimpleMatch -Pattern $probe |
        Select-Object -First 5 |
        ForEach-Object { Write-Host ("    line {0}: {1}" -f $_.LineNumber, $_.Line.TrimEnd()) }
    }
    throw "patch $Path : anchor not found :: $probe"
  }

  $cnt = [regex]::Matches($norm, [regex]::Escape($oldN)).Count
  if ($cnt -gt 1) { Write-Host "[patch] $Path :: WARNING anchor found x$cnt, replacing all" }

  Copy-Item $rp (Join-Path $bdir (($Path -replace '[\\/]','__'))) -Force

  $upd = $norm.Replace($oldN, $newN)
  $eol = if ($raw.Contains("`r`n")) { "`r`n" } else { "`n" }
  [IO.File]::WriteAllText($rp, $upd.Replace("`n", $eol), (New-Object System.Text.UTF8Encoding($false)))
  $script:patched = $script:patched + 1
  Write-Host "[patch] $Path OK"
}

function Patch-Regex {
  param([string]$Path, [string]$Pattern, [string]$Replacement, [int]$Expect = 1, [string]$SkipIf = '')
  if (-not (Test-Path $Path)) { throw "patch: file not found: $Path" }
  $rp  = (Resolve-Path $Path).Path
  $raw = [System.IO.File]::ReadAllText($rp, [System.Text.Encoding]::UTF8)

  if ($SkipIf -and $raw.Contains($SkipIf)) {
    Write-Host "[patch] $Path :: already applied (marker '$SkipIf'), skip"
    $script:skipped = $script:skipped + 1
    return
  }

  Copy-Item $rp (Join-Path $bdir (($Path -replace '[\\/]','__'))) -Force
  $eol = if ($raw.Contains("`r`n")) { "`r`n" } else { "`n" }
  $rep = $Replacement.Replace('{EOL}', $eol)
  $m = [regex]::Matches($raw, $Pattern)
  if ($m.Count -ne $Expect) { throw "regex $Path : matched $($m.Count), expected $Expect. ABORT (originals in $bdir)" }
  $raw = [regex]::Replace($raw, $Pattern, $rep)
  [System.IO.File]::WriteAllText($rp, $raw, (New-Object System.Text.UTF8Encoding($false)))
  $script:patched = $script:patched + 1
  Write-Host "[patch] $Path OK (regex x$Expect)"
}

# ---- 1) src/risk/mod.rs: live_equity field + ctor + sync + getters ----
Patch-File -Path 'src\risk\mod.rs' `
  -Old @('use std::sync::Arc;') `
  -New @('use std::sync::{Arc, RwLock};')

Patch-File -Path 'src\risk\mod.rs' `
  -Old @('    equity_peak: f64,','    prop_dd_pct: f64,') `
  -New @('    equity_peak: f64,','    prop_dd_pct: f64,','','    // P16: live equity mirror (initial + realized PnL), read by strategies','    live_equity: Arc<RwLock<f64>>,')

Patch-File -Path 'src\risk\mod.rs' `
  -Old @('        let initial_balance = config.initial_balance.to_f64().unwrap_or(1000.0);','        let current_equity = initial_balance + realized_pnl.to_f64().unwrap_or(0.0);') `
  -New @('        let initial_balance = config.initial_balance.to_f64().unwrap_or(1000.0);','        let current_equity = initial_balance + realized_pnl.to_f64().unwrap_or(0.0);','        let live_equity = Arc::new(RwLock::new(current_equity)); // P16')

Patch-File -Path 'src\risk\mod.rs' `
  -Old @('            equity_peak,','            prop_dd_pct,') `
  -New @('            equity_peak,','            prop_dd_pct,','            live_equity,')

Patch-File -Path 'src\risk\mod.rs' `
  -Old @('        let equity = initial + self.realized_pnl.to_f64().unwrap_or(0.0);','        if equity > self.equity_peak {') `
  -New @('        let equity = initial + self.realized_pnl.to_f64().unwrap_or(0.0);','        if let Ok(mut w) = self.live_equity.write() { *w = equity; } // P16','        if equity > self.equity_peak {')

Patch-File -Path 'src\risk\mod.rs' `
  -Old @('    pub async fn flush_stats(&mut self) {') `
  -New @('    // P16: live equity accessors','    pub fn equity_handle(&self) -> Arc<RwLock<f64>> {','        self.live_equity.clone()','    }','','    pub fn live_equity(&self) -> f64 {','        self.live_equity.read().map(|g| *g).unwrap_or(0.0)','    }','','    pub async fn flush_stats(&mut self) {')

# ---- 2) src/agents/risk_agent.rs: expose handle ----
Patch-File -Path 'src\agents\risk_agent.rs' `
  -Old @('    pub fn risk_multiplier(&self) -> f64 {') `
  -New @('    // P16: expose live equity handle for strategy sizing sync','    pub fn equity_handle(&self) -> std::sync::Arc<std::sync::RwLock<f64>> {','        self.manager.equity_handle()','    }','','    pub fn risk_multiplier(&self) -> f64 {')

# ---- 3) src/strategies/mod.rs: trait default ----
Patch-File -Path 'src\strategies\mod.rs' `
  -Old @('    fn on_position_unknown(&mut self) {}') `
  -New @('    fn on_position_unknown(&mut self) {}','','    // P16: push live equity into the strategy (sizing base)','    fn set_live_equity(&mut self, _eq: f64) {}')

# ---- 4) src/main.rs: capture handle, startup sync, push on every close ----
Patch-File -Path 'src\main.rs' `
  -Old @('    let risk_manager = risk::RiskManager::new(&config, &global, global_risk.clone()).await?;') `
  -New @('    let risk_manager = risk::RiskManager::new(&config, &global, global_risk.clone()).await?;','    // P16: live equity handle (initial + persisted realized PnL, updated on close)','    let equity_handle = risk_manager.equity_handle();')

Patch-File -Path 'src\main.rs' `
  -Old @('    let mut strategy = strategies::create_strategy(&config, &symbol, ob_state, ob_analyzer)?;') `
  -New @('    let mut strategy = strategies::create_strategy(&config, &symbol, ob_state, ob_analyzer)?;','    // P16: sync strategy sizing base with live equity at startup','    let eq0 = equity_handle.read().map(|g| *g).unwrap_or(0.0);','    strategy.set_live_equity(eq0);','    tracing::info!("[{}] P16: sizing base = live equity {:.2}", bot_name, eq0);')

# full exits (2 sites)
Patch-Regex -Path 'src\main.rs' `
  -Pattern '(?m)^([ ]+)strategy\.notify_position_closed\(pnl\);\r?\n[ ]+risk_agent\.record_trade\(&exit_signal\.symbol, pnl\);' `
  -Replacement ('$1strategy.notify_position_closed(pnl);{EOL}$1risk_agent.record_trade(&exit_signal.symbol, pnl);{EOL}$1strategy.set_live_equity(equity_handle.read().map(|g| *g).unwrap_or(0.0)); // P16-exit') `
  -Expect 2 -SkipIf 'P16-exit'

# partial exit (1 site)
Patch-Regex -Path 'src\main.rs' `
  -Pattern '(?m)^([ ]+)risk_agent\.record_trade\(&exit_signal\.symbol, pnl\);\r?\n([ ]+)let _ = trade_journal' `
  -Replacement ('$1risk_agent.record_trade(&exit_signal.symbol, pnl);{EOL}$1strategy.set_live_equity(equity_handle.read().map(|g| *g).unwrap_or(0.0)); // P16-exit-part{EOL}$2let _ = trade_journal') `
  -Expect 2 -SkipIf 'P16-exit-part'

# ---- 5) six strategies: implement set_live_equity ----
 $ins   = @('    // P16: sizing base follows live equity','    fn set_live_equity(&mut self, eq: f64) { self.balance = eq; }','')
 $oPnl  = @('    fn notify_position_closed(&mut self, pnl: f64) {')
 $oPnlU = @('    fn notify_position_closed(&mut self, _pnl: f64) {')

Patch-File -Path 'src\strategies\momentum.rs'       -Old $oPnl  -New ($ins + $oPnl)
Patch-File -Path 'src\strategies\momentum_v3.rs'    -Old $oPnl  -New ($ins + $oPnl)
Patch-File -Path 'src\strategies\liquidity_grab.rs' -Old $oPnlU -New ($ins + $oPnlU)
Patch-File -Path 'src\strategies\keltner_dca.rs'    -Old $oPnlU -New ($ins + $oPnlU)
Patch-File -Path 'src\strategies\bounce_trader.rs'  -Old $oPnlU -New ($ins + $oPnlU)
Patch-File -Path 'src\strategies\gold_session_trader.rs' `
  -Old @('    fn notify_position_closed(&mut self, _pnl: f64) { self.filter.on_position_closed(self.last_kline_ts); }') `
  -New ($ins + @('    fn notify_position_closed(&mut self, _pnl: f64) { self.filter.on_position_closed(self.last_kline_ts); }'))

# ---- 6) cargo check ----
Write-Host '[gate] cargo check...'
 $ErrorActionPreference = 'Continue'
$out = & cargo check 2>&1
$ErrorActionPreference = 'Stop'
 $out | ForEach-Object { "$_" } | Select-Object -Last 40
if ($LASTEXITCODE -ne 0) { throw "cargo check FAILED - originals in $bdir" }

# ---- 7) release build to SIDE target dir (running exe untouched) ----
Write-Host '[gate] cargo build --release --target-dir target_p16 (few minutes)...'
 $ErrorActionPreference = 'Continue'
$out = & cargo build --release --target-dir target_p16 2>&1
$ErrorActionPreference = 'Stop'
 $out | Select-Object -Last 8
if ($LASTEXITCODE -ne 0) { throw "cargo build FAILED - originals in $bdir" }

 $exe = 'target_p16\release\scalping_agents.exe'
if (-not (Test-Path $exe)) { throw "build output not found: $exe - check binary name in Cargo.toml" }
Get-Item $exe | Select-Object FullName, Length, LastWriteTime | Format-List

Write-Host "[gate] DONE. patched: $script:patched, skipped: $script:skipped. Bot NOT restarted."
Write-Host '[gate] Deploy when YOU decide: powershell -ExecutionPolicy Bypass -File .\p16_deploy.ps1'
Write-Host "[gate] Rollback sources: copy files from $bdir back into src\"