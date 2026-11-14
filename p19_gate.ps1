# ============================================================
# p19_gate.ps1  (v2) -- P19: kill stale idle HTTP connections
# v2 fix: run cargo via cmd /c so stderr lines do not throw
#         NativeCommandError under $ErrorActionPreference='Stop'
# Fix: pool_idle_timeout(20s) + tcp_keepalive(15s) on EVERY
#      reqwest client in src\. Idempotent marker: /* P19-POOL */
# Patch -> backup -> cargo check -> build --release into target_p16.
# Bot is NOT restarted here. Next step: p16_deploy.ps1
# ============================================================
 $ErrorActionPreference = 'Stop'
 $ts   = Get-Date -Format 'yyyyMMdd-HHmmss'
 $bkd  = Join-Path 'backups' "p19-$ts"
 $MARK = '/* P19-POOL */'
 $INS  = '.pool_idle_timeout(std::time::Duration::from_secs(20)).tcp_keepalive(std::time::Duration::from_secs(15))'
 $REP  = 'Client::builder()' + $INS + $MARK + '.build().expect("P19-POOL http client")'

Write-Host "[p19] backup dir: $bkd"

 $procs = Get-Process cargo,rustc,rust-analyzer -ErrorAction SilentlyContinue
foreach ($p in $procs) { Write-Host ("[p19] NOTE: {0} PID={1} is running (may hold cargo lock; cargo will wait)" -f $p.ProcessName, $p.Id) }

New-Item -ItemType Directory -Path $bkd -Force | Out-Null

 $jobs   = @()
 $report = @()
 $total  = 0

try {
  $files = Get-ChildItem -Path 'src' -Recurse -Filter '*.rs' | Sort-Object FullName
  foreach ($f in $files) {
    $text = [System.IO.File]::ReadAllText($f.FullName)
    if ($text.Contains($MARK)) { $report += "SKIP already patched: $($f.FullName)"; continue }
    $text = $text.Replace("`r`n", "`n")

    $ops = @()
    foreach ($m in [regex]::Matches($text, 'Client::builder\(\)')) {
      $from = $m.Index + $m.Length
      $bIdx = $text.IndexOf('.build(', $from)
      if ($bIdx -lt 0) { $report += "WARN no .build( after builder: $($f.Name)"; continue }
      $seg = $text.Substring($from, $bIdx - $from)
      if ($seg.Contains(';')) { $report += "WARN ';' in builder segment, skipped: $($f.Name)"; continue }
      if ($seg.Contains('pool_idle_timeout')) { $report += "SKIP pool cfg already set: $($f.Name)"; continue }
      $ops += @{ Idx = $bIdx; Len = 7; Rep = ($INS + $MARK + '.build(') }
    }
    foreach ($m in [regex]::Matches($text, '(?<![A-Za-z0-9_])Client::new\(\)')) {
      $ops += @{ Idx = $m.Index; Len = $m.Length; Rep = $REP }
    }
    if ($ops.Count -eq 0) { continue }

    foreach ($op in ($ops | Sort-Object { $_.Idx } -Descending)) {
      $text = $text.Substring(0, $op.Idx) + $op.Rep + $text.Substring($op.Idx + $op.Len)
    }

    $rel = $f.FullName.Substring((Get-Location).Path.Length + 1) -replace '\\','__'
    $bk  = Join-Path $bkd $rel
    Copy-Item $f.FullName $bk -Force
    [System.IO.File]::WriteAllText($f.FullName, $text)

    $chk = [System.IO.File]::ReadAllText($f.FullName)
    $c = ([regex]::Matches($chk, [regex]::Escape($MARK))).Count
    if ($c -ne $ops.Count) { throw "PARSE FAIL $($f.FullName): markers=$c expected=$($ops.Count)" }

    $jobs  += @{ full = $f.FullName; bk = $bk; sites = $ops.Count }
    $total += $ops.Count
    $report += ("PATCHED {0} site(s): {1}" -f $ops.Count, $f.FullName)
  }

  Write-Host "[p19] ---- sites ----"
  $report | ForEach-Object { Write-Host "  $_" }

  $already = ($report | Where-Object { $_ -like 'SKIP already patched*' }).Count
  if ($total -eq 0) {
    if ($already -gt 0) { Write-Host "[p19] already patched earlier, nothing to do"; exit 0 }
    Write-Host "[p19] GATE FAIL: no client sites found; diag -> p19_diag.txt"
    Get-ChildItem -Path 'src\exchange','src\webui','src\utils' -Filter '*.rs' -ErrorAction SilentlyContinue |
      ForEach-Object { "=== $($_.FullName) ==="; Get-Content $_.FullName -Encoding UTF8 | Select-String -Pattern 'client|Client' -Context 2,4 } |
      Out-File p19_diag.txt -Encoding utf8
    exit 1
  }

  Write-Host "[p19] cargo check (sitting on 'Blocking waiting for file lock' a while is NORMAL)..."
  $env:CARGO_TARGET_DIR = 'target_p16'
  $cout = cmd /c "cargo check 2>&1"
  $cout | Select-Object -Last 20 | ForEach-Object { Write-Host "  $_" }
  if ($LASTEXITCODE -ne 0) { throw "cargo check failed, exit=$LASTEXITCODE" }

  Write-Host "[p19] cargo build --release..."
  $t0 = Get-Date
  $bout = cmd /c "cargo build --release 2>&1"
  $bout | Select-Object -Last 25 | ForEach-Object { Write-Host "  $_" }
  if ($LASTEXITCODE -ne 0) { throw "cargo build failed, exit=$LASTEXITCODE" }
  $dt = [int]((Get-Date) - $t0).TotalSeconds

  $exe = Join-Path 'target_p16' 'release\scalping_agents.exe'
  if (Test-Path $exe) {
    $it = Get-Item $exe
    Write-Host ("[p19] exe: {0}  bytes={1}  built={2}  build={3}s" -f $exe, $it.Length, $it.LastWriteTime, $dt)
  }
  Write-Host "[p19] GATE OK: $total site(s) patched in $($jobs.Count) file(s). Bot NOT restarted."
}
catch {
  Write-Host "[p19] ERROR: $_"
  foreach ($j in $jobs) { Copy-Item $j.bk $j.full -Force }
  Write-Host "[p19] files restored from backup. GATE FAIL."
  exit 1
}