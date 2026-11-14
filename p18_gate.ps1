# ============================================================
# p18_gate.ps1 :: P18 - protective trigger precision (tickSize)
# Kill class: -1111 "Precision is over the maximum" on SL/TP triggers.
# TAOUSDT 2026-10-18: SL 258.58243243, tickSize=0.01, chain 8/7/6
# digits all rejected -> position 110+ sec unprotected.
# Fix: round protective triggers to tickSize (FilterRegistry, Decimal)
#      at ALL send sites. Entries already rounded via apply_filters.
# Sites: EE place_protective_orders (sl/tp), EE Exit_UpdateSL (trail),
#        main.rs orphan SL fallback (synthetic 2%).
# Protocol: backup -> atomic idempotent patches -> cargo check -> build.
# Bot NOT restarted. Deploy separately: p16_deploy.ps1 (reusable).
# ============================================================
 $ErrorActionPreference = 'Stop'
Set-Location -Path $PSScriptRoot

 $Stamp  = Get-Date -Format 'yyyyMMdd-HHmmss'
 $Backup = "backups\p18-$Stamp"
 $script:BackedUp = @{}

 $EE = 'src\execution\execution_engine.rs'
 $MN = 'src\main.rs'

Write-Host "[gate] backup dir: $Backup"

function Patch-File {
  param(
    [string]$Path,
    [string[]]$Old,
    [string[]]$New,
    [int]$Expect = 1
  )
  if (-not (Test-Path $Path)) { throw "[patch] FILE NOT FOUND: $Path" }
  $full = Join-Path (Get-Location).Path $Path
  $src  = [System.IO.File]::ReadAllText($full)
  $src  = $src.Replace("`r`n", "`n")

  $oldText = ($Old -join "`n")
  $newText = ($New -join "`n")

  $count = ([regex]::Matches($src, [regex]::Escape($oldText))).Count
  if ($count -eq 0 -and $src.Contains($newText)) {
    Write-Host "[patch] $Path :: already applied, skip"
    return
  }
  if ($count -ne $Expect) {
    throw ("[patch] ABORT {0} :: anchor matched {1}, expected {2} - nothing written" -f $Path, $count, $Expect)
  }
  foreach ($ch in $newText.ToCharArray()) {
    if ([int]$ch -gt 127) { throw "[patch] ABORT $Path :: non-ASCII char in replacement" }
  }
  if (-not $script:BackedUp.ContainsKey($Path)) {
    $dst = Join-Path $Backup $Path
    $dir = Split-Path $dst -Parent
    if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
    Copy-Item -Path $Path -Destination $dst -Force
    $script:BackedUp[$Path] = $true
    Write-Host "[backup] $Path"
  }
  $patched = $src.Replace($oldText, $newText)
  [System.IO.File]::WriteAllText($full, $patched, (New-Object System.Text.UTF8Encoding($false)))
  Write-Host "[patch] $Path :: OK (anchor x$Expect)"
}

# ---------- 1/6 EE: tick-align SL/TP in place_protective_orders --------
Patch-File -Path $EE -Old @(
'        let close_side = if signal.side == "BUY" { "SELL" } else { "BUY" };',
'',
'        if let Some(sl) = signal.stop_loss {'
) -New @(
'        let close_side = if signal.side == "BUY" { "SELL" } else { "BUY" };',
'',
'        // P18: protective triggers must be multiples of tickSize, otherwise',
'        // -1111 rejects them (TAOUSDT 2026-10-18: SL/TP rejected 3x, position',
'        // 110+ sec unprotected). Decimal round to nearest tick, no float dust.',
'        let p18_tick = |v: f64| -> f64 {',
'            match self.filters.as_ref().and_then(|r| r.get(&signal.symbol).ok()) {',
'                Some(f) => match Decimal::from_f64(v) {',
'                    Some(d) => f.round_price(d).to_f64().unwrap_or(v),',
'                    None => v,',
'                },',
'                None => v,',
'            }',
'        };',
'        let sl_f = signal.stop_loss.map(|v| p18_tick(v));',
'        let tp_f = signal.take_profit.map(|v| p18_tick(v));',
'',
'        if sl_f != signal.stop_loss || tp_f != signal.take_profit {',
'            tracing::info!(',
'                "[{}] P18: protective triggers tick-aligned: sl {:?} -> {:?}, tp {:?} -> {:?}",',
'                self.config.name, signal.stop_loss, sl_f, signal.take_profit, tp_f',
'            );',
'        }',
'',
'        if let Some(sl) = sl_f {'
) -Expect 1

# ---------- 2/6 EE: TP branch uses the rounded value --------------------
Patch-File -Path $EE -Old @(
'        if let Some(tp) = signal.take_profit {'
) -New @(
'        if let Some(tp) = tp_f {'
) -Expect 1

# ---------- 3/6 EE: Exit_UpdateSL trail trigger --------------------------
Patch-File -Path $EE -Old @(
'                    let (sl_id, _tp_id) = self.state_manager.get_protective_ids(&signal.symbol);',
'                    let new_sl = signal.price;'
) -New @(
'                    let (sl_id, _tp_id) = self.state_manager.get_protective_ids(&signal.symbol);',
'                    // P18: trail trigger must be a tick multiple, otherwise -1111',
'                    // kills BOTH stops: the old one is already cancelled here.',
'                    let new_sl = match self',
'                        .filters',
'                        .as_ref()',
'                        .and_then(|r| r.get(&signal.symbol).ok())',
'                        .and_then(|f| {',
'                            Decimal::from_f64(signal.price)',
'                                .map(|d| f.round_price(d).to_f64().unwrap_or(signal.price))',
'                        })',
'                    {',
'                        Some(v) if v > 0.0 => v,',
'                        _ => signal.price,',
'                    };',
'                    if (new_sl - signal.price).abs() > f64::EPSILON {',
'                        tracing::info!(',
'                            "[{}] P18: UpdateSL trigger {} -> {} (tickSize)",',
'                            self.config.name, signal.price, new_sl',
'                        );',
'                    }'
) -Expect 1

# ---------- 4/6 main.rs: FromPrimitive import ----------------------------
Patch-File -Path $MN -Old @(
'use rust_decimal::prelude::ToPrimitive;'
) -New @(
'use rust_decimal::prelude::{FromPrimitive, ToPrimitive};'
) -Expect 1

# ---------- 5/6 main.rs: registry clone into reconciler task -------------
Patch-File -Path $MN -Old @(
'        let bot = bot_name.clone();',
'        let sym = symbol.clone();',
'        let max_dur = config.max_position_duration_sec;',
'        children.spawn(async move {'
) -New @(
'        let bot = bot_name.clone();',
'        let sym = symbol.clone();',
'        let max_dur = config.max_position_duration_sec;',
'        // P18: registry for the reconciler task (orphan SL tick rounding)',
'        let filters = filters.clone();',
'        children.spawn(async move {'
) -Expect 1

# ---------- 6/6 main.rs: round synthetic SL in orphan fallback -----------
Patch-File -Path $MN -Old @(
'                    let Some(sl) = pos.stop_loss else { continue };',
'                    let close_side = if pos.side == state::state_manager::PositionSide::Long { "SELL" } else { "BUY" };',
'                    match client',
'                        .place_protective_order(&pos.symbol, close_side, true, sl, None)'
) -New @(
'                    let Some(sl) = pos.stop_loss else { continue };',
'                    let close_side = if pos.side == state::state_manager::PositionSide::Long { "SELL" } else { "BUY" };',
'                    // P18: synthetic 2% SL is not tick-aligned -> -1111. Round it.',
'                    let sl = match filters.as_ref().and_then(|r| r.get(&pos.symbol).ok()) {',
'                        Some(f) => match rust_decimal::Decimal::from_f64(sl) {',
'                            Some(d) => f.round_price(d).to_f64().unwrap_or(sl),',
'                            None => sl,',
'                        },',
'                        None => sl,',
'                    };',
'                    match client',
'                        .place_protective_order(&pos.symbol, close_side, true, sl, None)'
) -Expect 1

# ---------- gate: compile -------------------------------------------------
Write-Host ''
Write-Host '[gate] cargo check...'
 $ErrorActionPreference = 'Continue'
 $chk = & cargo check 2>&1
 $chk | ForEach-Object { "$_" } | Select-Object -Last 12
 $ErrorActionPreference = 'Stop'
if ($LASTEXITCODE -ne 0) { throw "[gate] cargo check FAILED - rollback: copy files from $Backup back into src\" }

Write-Host '[gate] cargo build --release --target-dir target_p16 (few minutes)...'
 $ErrorActionPreference = 'Continue'
 $bld = & cargo build --release --target-dir target_p16 2>&1
 $bld | ForEach-Object { "$_" } | Select-Object -Last 6
 $ErrorActionPreference = 'Stop'
if ($LASTEXITCODE -ne 0) { throw "[gate] build FAILED - rollback: copy files from $Backup back into src\" }

 $exe = 'target_p16\release\scalping_agents.exe'
if (Test-Path $exe) {
  $i = Get-Item $exe
  Write-Host ''
  Write-Host ("[gate] exe: {0}" -f $i.FullName)
  Write-Host ("[gate] size={0} bytes, built {1}" -f $i.Length, $i.LastWriteTime)
}
Write-Host ''
Write-Host '[gate] DONE. patched: 6 sites (EE x3, main.rs x3). Bot NOT restarted.'
Write-Host '[gate] Deploy when YOU decide: powershell -ExecutionPolicy Bypass -File .\p16_deploy.ps1'
Write-Host "[gate] Rollback: copy files from $Backup back into src\"