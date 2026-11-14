# ============================================================
# p18_harvest2.ps1 :: READ-ONLY, targeted. Nothing is patched.
# Run: powershell -ExecutionPolicy Bypass -File .\p18_harvest2.ps1
# Result: p18_harvest2.txt -> attach it back to the chat.
# ============================================================
 $ErrorActionPreference = 'Stop'
Set-Location -Path $PSScriptRoot

 $out = New-Object System.Collections.Generic.List[string]
function Emit([string]$s) { $script:out.Add($s) | Out-Null }

function Get-Lines([string]$Path) {
  $full = Join-Path (Get-Location).Path $Path
  if (-not (Test-Path $full)) { return $null }
  return ,[System.IO.File]::ReadAllLines($full)
}

function Dump-Region([string]$Path, [int]$Lo, [int]$Hi, [string]$Tag) {
  $lines = Get-Lines $Path
  if ($null -eq $lines) { Emit "##### $Tag :: FILE NOT FOUND: $Path"; return }
  $lo = [Math]::Max(1, $Lo); $hi = [Math]::Min($lines.Count, $Hi)
  Emit ""
  Emit "##### $Tag :: $Path  lines $lo-$hi  (file total $($lines.Count))"
  for ($i = $lo; $i -le $hi; $i++) { Emit ("{0,5}: {1}" -f $i, $lines[$i-1]) }
}

function Grep-Token([string]$Path, [string]$Pattern, [string]$Tag, [int]$Ctx, [int]$Max) {
  $lines = Get-Lines $Path
  if ($null -eq $lines) { Emit "##### $Tag :: FILE NOT FOUND: $Path"; return -1 }
  $hits = New-Object System.Collections.Generic.List[int]
  for ($i = 0; $i -lt $lines.Count; $i++) {
    if ($lines[$i] -match $Pattern) { [void]$hits.Add($i) }
  }
  Emit ""
  Emit "##### $Tag :: $Path  matches=$($hits.Count)  pattern=/$Pattern/"
  $limit = [Math]::Min($hits.Count, $Max)
  if ($hits.Count -gt $Max) { Emit "##### (showing first $Max of $($hits.Count))" }
  $shown = @{}
  for ($k = 0; $k -lt $limit; $k++) {
    $h = $hits[$k]
    for ($j = [Math]::Max(0, $h - $Ctx); $j -le [Math]::Min($lines.Count - 1, $h + $Ctx); $j++) {
      if (-not $shown.ContainsKey($j)) { $shown[$j] = $true; Emit ("{0,5}: {1}" -f ($j + 1), $lines[$j]) }
    }
    Emit "  ----"
  }
  return $hits.Count
}

function Grep-Tree([string]$Dir, [string]$Pattern, [string]$Tag, [int]$Ctx, [int]$Max) {
  Emit ""
  Emit "##### $Tag :: tree $Dir  pattern=/$Pattern/"
  if (-not (Test-Path $Dir)) { Emit "##### DIR NOT FOUND: $Dir"; return }
  $files = @(Get-ChildItem -Path $Dir -Recurse -Filter *.rs | Sort-Object FullName)
  $cwd = (Get-Location).Path
  $total = 0
  $map = @()
  foreach ($f in $files) {
    $lines = [System.IO.File]::ReadAllLines($f.FullName)
    $c = 0
    for ($i = 0; $i -lt $lines.Count; $i++) { if ($lines[$i] -match $Pattern) { $c++ } }
    if ($c -gt 0) { $map += (($f.FullName.Replace($cwd + '\', '')) + " :: " + $c) }
    $total += $c
  }
  Emit "##### total: $total match(es) in $($map.Count) file(s)"
  foreach ($m in $map) { Emit "  $m" }
  $budget = $Max
  foreach ($f in $files) {
    if ($budget -le 0) { break }
    $lines = [System.IO.File]::ReadAllLines($f.FullName)
    $local = New-Object System.Collections.Generic.List[int]
    for ($i = 0; $i -lt $lines.Count; $i++) { if ($lines[$i] -match $Pattern) { [void]$local.Add($i) } }
    if ($local.Count -eq 0) { continue }
    Emit ("  ---- " + $f.FullName.Replace($cwd + '\', ''))
    $shown = @{}
    for ($k = 0; $k -lt $local.Count; $k++) {
      if ($budget -le 0) { Emit "  ... (capped)"; break }
      $h = $local[$k]; $budget--
      for ($j = [Math]::Max(0, $h - $Ctx); $j -le [Math]::Min($lines.Count - 1, $h + $Ctx); $j++) {
        if (-not $shown.ContainsKey($j)) { $shown[$j] = $true; Emit ("{0,5}: {1}" -f ($j + 1), $lines[$j]) }
      }
    }
  }
}

Emit ("##### p18_harvest2 :: " + (Get-Date -Format 'yyyy-MM-dd HH:mm:ss') + " :: cwd=" + (Get-Location).Path)

 $BF = 'src\exchange\binance_futures.rs'
 $SF = 'src\exchange\symbol_filters.rs'
 $TR = 'src\exchange\traits.rs'
 $EE = 'src\execution\execution_engine.rs'
 $MN = 'src\main.rs'

# --- 1) binance_futures.rs: protective order fn (algo path + classic fallback + chain)
Dump-Region $BF 100 280 'P18-bf-protective'
[void](Grep-Token $BF 'pub (async )?fn ' 'P18-bf-api-map' 0 50)
[void](Grep-Token $BF 'hmac|signature|recvWindow' 'P18-bf-sign' 2 4)
[void](Grep-Token $BF 'OP_MARKET|OP_LIMIT|STOP_MARKET|TAKE_PROFIT_MARKET|algotype|CONDITIONAL' 'P18-bf-ordertypes' 1 15)
[void](Grep-Token $BF '\{:\.\d' 'P18-bf-numfmt' 2 15)
[void](Grep-Token $BF 'tick_size|step_size|apply_filters|FilterRegistry' 'P18-bf-filter-use' 2 10)

# --- 2) symbol_filters.rs: struct fields + rounding API
Dump-Region $SF 1 360 'P18-sf-body'
[void](Grep-Token $SF 'pub (async )?fn ' 'P18-sf-api-map' 0 40)

# --- 3) traits.rs: full (protective order trait signature)
Dump-Region $TR 1 130 'P18-traits-full'

# --- 4) execution_engine.rs: SL/TP wiring + trailing update
Dump-Region $EE 620 800 'P18-ee-protective-zone'
Dump-Region $EE 990 1090 'P18-ee-1000'
[void](Grep-Token $EE 'Exit_UpdateSL|UpdateSL|update_sl' 'P18-ee-updatesl' 12 6)
[void](Grep-Token $EE 'apply_filters' 'P18-ee-applyfilters' 5 10)
[void](Grep-Token $EE 'place_order|place_and_wait' 'P18-ee-place' 3 12)
[void](Grep-Token $EE 'pub (async )?fn ' 'P18-ee-api-map' 0 40)

# --- 5) main.rs: audit lines 1016/1029 candidates + orphan stop loop
[void](Grep-Token $MN 'Exit_UpdateSL|UpdateSL|update_sl' 'P18-mn-updatesl' 10 6)
Dump-Region $MN 1000 1050 'P18-mn-1000'

# --- 6) strategies: hardcoded TICK_SIZE / QTY_STEP usage
[void](Grep-Tree 'src\strategies' 'TICK_SIZE|QTY_STEP' 'P18-strat-consts' 2 20)

# --- 7) protective order plumbing across the tree
[void](Grep-Tree 'src' 'place_protective' 'P18-tree-protective' 3 12)

 $p = Join-Path (Get-Location).Path 'p18_harvest2.txt'
[System.IO.File]::WriteAllLines($p, $out)
Write-Host ("[harvest2] written: " + $p + "  (" + $out.Count + " lines)")
Write-Host "[harvest2] attach p18_harvest2.txt to the chat (or paste in parts)."