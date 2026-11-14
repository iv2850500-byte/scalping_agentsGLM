# p19b_harvest.ps1 -- READ-ONLY dump for P19b (kline streams via WS)
 $ErrorActionPreference = 'SilentlyContinue'
 $out = 'p19b_harvest.txt'
Remove-Item $out
'=== P19B HARVEST ===' | Out-File $out -Encoding utf8

function Dump-File($path) {
  if (Test-Path $path) {
    "=== FILE: $path ===" | Out-File $out -Append -Encoding utf8
    $i = 0
    Get-Content $path -Encoding UTF8 | ForEach-Object { $i++; "{0,5}: {1}" -f $i, $_ } | Out-File $out -Append -Encoding utf8
  } else { "=== MISSING: $path ===" | Out-File $out -Append -Encoding utf8 }
}

Get-ChildItem -Path 'src\websocket' -Filter '*.rs' | ForEach-Object { Dump-File $_.FullName }
Dump-File 'src\exchange\kline_poller.rs'

'=== GREP streams/subscribe across src ===' | Out-File $out -Append -Encoding utf8
Get-ChildItem -Path 'src' -Recurse -Filter '*.rs' | ForEach-Object {
  $hits = Select-String -Path $_.FullName -Pattern 'streams=|SUBSCRIBE|subscribe|@kline|bookTicker|connect_async' -Encoding UTF8
  foreach ($h in $hits) { "{0}:{1}: {2}" -f $_.FullName, $h.LineNumber, $h.Line.Trim() | Out-File $out -Append -Encoding utf8 }
}

'=== GREP candle publish/channel API across src ===' | Out-File $out -Append -Encoding utf8
Get-ChildItem -Path 'src' -Recurse -Filter '*.rs' | ForEach-Object {
  $hits = Select-String -Path $_.FullName -Pattern 'publish_candle|on_candle|CandleEvent|broadcast::|mpsc::channel|candle_tx|candle_rx' -Encoding UTF8
  foreach ($h in $hits) { "{0}:{1}: {2}" -f $_.FullName, $h.LineNumber, $h.Line.Trim() | Out-File $out -Append -Encoding utf8 }
}

'=== GREP wiring in main.rs ===' | Out-File $out -Append -Encoding utf8
Get-Content 'src\main.rs' -Encoding UTF8 | Select-String -Pattern 'KlinePoller|kline|WebSocket|websocket|connect_async' -Context 1,2 | Out-File $out -Append -Encoding utf8

'[p19b-harvest] DONE -> p19b_harvest.txt'
'[p19b-harvest] size: ' + (Get-Item $out).Length + ' bytes'