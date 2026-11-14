# ============================================================
# p19_harvest.ps1  (READ-ONLY - no build, no restart)
# Collects: kline poller, resilient client, websocket topology
# Output: p19_harvest.txt  (send this file back)
# ============================================================
 $ErrorActionPreference = 'Continue'
 $out = Join-Path (Get-Location) 'p19_harvest.txt'
Set-Content -Path $out -Encoding UTF8 -Value ("P19 harvest started " + (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'))

function Section([string]$tag) {
  Add-Content -Path $out -Encoding UTF8 -Value ''
  Add-Content -Path $out -Encoding UTF8 -Value ("##### P19-" + $tag)
}

function FindFile([string]$name) {
  Get-ChildItem -Path src -Recurse -File -Filter $name -ErrorAction SilentlyContinue |
    Select-Object -First 1 -ExpandProperty FullName
}

function Dump([string]$path, [int]$max) {
  Section ("FILE|" + $path)
  if (-not $path -or -not (Test-Path $path)) {
    Add-Content -Path $out -Encoding UTF8 -Value '!!! FILE NOT FOUND'
    return
  }
  $i = 0
  Get-Content -Path $path -Encoding UTF8 | ForEach-Object {
    $i++
    if ($i -le $max) { Add-Content -Path $out -Encoding UTF8 -Value ('{0,5}: {1}' -f $i, $_) }
  }
  Section ("ENDFILE|" + $path + "|lines=" + $i)
}

function GrepAll([string]$pattern) {
  Section ("GREP|" + $pattern)
  $files = Get-ChildItem -Path src -Recurse -File -Filter '*.rs' -ErrorAction SilentlyContinue
  foreach ($f in $files) {
    $i = 0
    Get-Content -Path $f.FullName -Encoding UTF8 | ForEach-Object {
      $i++
      if ($_ -match $pattern) {
        Add-Content -Path $out -Encoding UTF8 -Value ((Split-Path $f.FullName -Leaf) + ':' + $i + ': ' + $_.Trim())
      }
    }
  }
}

Section 'INVENTORY'
Get-ChildItem -Path src -Recurse -File -Filter '*.rs' |
  Select-Object @{n='Path';e={$_.FullName.Replace((Get-Location).Path + '\','')}}, Length |
  Format-Table -AutoSize | Out-String | Add-Content -Path $out -Encoding UTF8

 $k = FindFile 'kline_poller.rs';      if ($k) { Dump $k 1200 }
 $r = FindFile 'resilient_client.rs';  if ($r) { Dump $r 1200 }
 $m = FindFile 'mod.rs';               if ($m -and $m -like '*\exchange\*') { Dump $m 300 }

 $wsf = Get-ChildItem -Path src -Recurse -File -Filter '*.rs' |
  Where-Object { Select-String -Path $_.FullName -Pattern 'bookTicker' -Quiet } |
  Select-Object -First 1
if ($wsf) { Dump $wsf.FullName 1200 }

GrepAll 'reqwest::Client|Client::builder|pool_idle|pool_max|connect_timeout|\.timeout\('
GrepAll 'streams=|bookTicker|@kline|aggTrade|agg_trade|SUBSCRIBE|subscribe'
GrepAll 'broadcast::|mpsc::|watch::|unbounded|channel\('
GrepAll 'struct Candle|on_candle|new_candle|push_candle|update_candle'

Section 'DONE'
Write-Host ''
Write-Host '[harvest] DONE ->' $out
Write-Host ('[harvest] size: ' + (Get-Item $out).Length + ' bytes')