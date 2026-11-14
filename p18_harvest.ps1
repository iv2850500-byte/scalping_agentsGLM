# ============================================================
# p18_harvest.ps1 :: READ-ONLY collector for P18 (precision/filters)
# Run: powershell -ExecutionPolicy Bypass -File .\p18_harvest.ps1
# Reads src\ only. Writes p18_harvest.txt. Patches NOTHING.
# Safe to run on the live bot.
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

Emit ("##### p18_harvest :: " + (Get-Date -Format 'yyyy-MM-dd HH:mm:ss') + " :: cwd=" + (Get-Location).Path)

# --- 1) binance_futures.rs: precision chain + fallback {:.8} (audit lines 173 / 229)
 $root = 'src\binance_futures.rs'
if (-not (Test-Path $root)) {
  Emit "##### FATAL: $root not found - run this script from the repo root"
} else {
  Dump-Region $root 140 260 'P18-precision-fallback'
  Dump-Region $root 990 1070 'P18-exit-updatesl-region'
  [void](Grep-Token $root 'pub (async )?fn ' 'P18-bf-api-map' 0 70)
  [void](Grep-Token $root 'hmac|signature|recvWindow' 'P18-sign-core' 2 6)
  [void](Grep-Token $root 'OP_MARKET|STOP_MARKET|TAKE_PROFIT|OP_LIMIT' 'P18-bf-order-types' 1 15)
  [void](Grep-Tree 'src' 'Exit_UpdateSL|UpdateSL' 'P18-exit-callers' 3 6)
}

# --- 2) symbol_filters.rs: FilterRegistry API
Emit ""
Emit "##### ===== symbol_filters.rs ====="
 $sf = 'src\symbol_filters.rs'
if (-not (Test-Path $sf)) {
  Emit "##### $sf NOT FOUND - candidates:"
  Get-ChildItem -Path src -Recurse -Filter *.rs -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -match 'filter' } |
    ForEach-Object { Emit ("  candidate: " + $_.FullName) }
} else {
  Dump-Region $sf 1 30 'P18-sf-head'
  [int]$sfn = Grep-Token $sf 'pub (async )?fn |pub struct|pub enum|impl ' 'P18-sf-api' 3 40
  if ($sfn -eq 0) { Dump-Region $sf 1 300 'P18-sf-full' }
}

# --- 3) tree-wide: 1e-8 consts, registry usage, exit params, filter tokens, fallback formats
[void](Grep-Tree 'src' '1e-8|0\.00000001' 'P18-consts' 2 12)
[void](Grep-Tree 'src' 'symbol_filters|FilterRegistry' 'P18-registry-usage' 2 12)
[void](Grep-Tree 'src' 'reduceOnly|workingType|closePosition' 'P18-exit-params' 2 12)
[void](Grep-Tree 'src' 'tick_?size|TICK_SIZE|PRICE_FILTER|step_?size|LOT_SIZE' 'P18-filters' 2 15)
[void](Grep-Tree 'src' '\{:\.8' 'P18-fallback8-tree' 4 8)

 $p = Join-Path (Get-Location).Path 'p18_harvest.txt'
[System.IO.File]::WriteAllLines($p, $out)
Write-Host ("[harvest] written: " + $p + "  (" + $out.Count + " lines)")
Write-Host "[harvest] send p18_harvest.txt back (attach the file; if that fails, paste the text, 2 messages is fine)."