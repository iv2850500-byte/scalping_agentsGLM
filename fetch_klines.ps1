#Requires -Version 5.1
<#
P30 fetch_klines.ps1 — загрузчик исторических свечей Binance (публичный REST, без ключей)
в формат CSV 1:1 для wfa-бинов (wfa_bounce / wfa_momentum):

    close_time,open,high,low,close,volume,taker_buy_volume

  close_time — ms UTC закрытия свечи (как в исходном BTCUSDT-1h-full.csv).
  Значения OHLCV пишутся КАК ЕСТЬ строками из API (без переформатирования float).

Примеры:
  powershell -ExecutionPolicy Bypass -File .\fetch_klines.ps1 -Symbol BTCUSDT -Days 450
  powershell -ExecutionPolicy Bypass -File .\fetch_klines.ps1 -Symbol ETHUSDT -Days 450 -Out ETHUSDT-1h-full.csv
  powershell -ExecutionPolicy Bypass -File .\fetch_klines.ps1 -Symbol BTCUSDT -Days 3 -Market spot -Out _test.csv

Примечания:
  - По умолчанию ФЬЮЧЕРСЫ (fapi) — то, чем торгует бот. -Market spot для спота.
  - Незакрытая текущая свеча не пишется. Дубли отсеиваются. Ретраи x3.
  - ВАЖНО: файл перезаписывается целиком (предсказуемо, без слияний).
#>
param(
    [ValidateSet("futures","spot")]
    [string]$Market  = "futures",
    [string]$Symbol  = "BTCUSDT",
    [ValidateSet("1m","5m","15m","30m","1h","2h","4h","1d")]
    [string]$Interval = "1h",
    [ValidateRange(1,1500)]
    [int]$Days       = 450,
    [string]$Out     = ""
)

$ErrorActionPreference = "Stop"
# Windows PowerShell 5.1: без этого TLS1.2 может не включиться по умолчанию
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

if (-not $Out) { $Out = "{0}-{1}-full.csv" -f $Symbol.ToUpper(), $Interval }

$base = if ($Market -eq "futures") { "https://fapi.binance.com/fapi/v1/klines" }
        else                       { "https://api.binance.com/api/v3/klines" }

$limit  = 1000
$nowMs  = [int64]([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds())
$endMs  = $nowMs
$startMs = $nowMs - [int64]$Days * 86400000

$seen     = New-Object 'System.Collections.Generic.HashSet[int64]'
$rows     = New-Object 'System.Collections.Generic.List[string]'
$lastClose = [int64]0
$reqN     = 0

Write-Host "[fetch] $Symbol $Interval ($Market): последние $Days дн. -> $Out"

while ($true) {
    $uri = "{0}?symbol={1}&interval={2}&startTime={3}&endTime={4}&limit={5}" -f `
        $base, $Symbol.ToUpper(), $Interval, $startMs, $endMs, $limit

    $kl = $null
    for ($tryN = 1; $tryN -le 3; $tryN++) {
        try { $kl = Invoke-RestMethod -Uri $uri -Method Get -TimeoutSec 30; break }
        catch {
            if ($tryN -eq 3) { throw "[fetch] API недоступен после 3 попыток: $($_.Exception.Message). Если это 451/403 (гео-блок) — пришли вывод в чат." }
            Start-Sleep -Seconds (2 * $tryN)
        }
    }
    if ($kl -isnot [array]) { $kl = @($kl) }   # PS5.1 разворачивает массив из 1 элемента
    $reqN++
    if ($kl.Count -eq 0) { break }

    $chunk = 0
    foreach ($k in $kl) {
        $ct = [int64]$k[6]                       # close_time, ms
        if ($ct -ge $nowMs) { continue }         # незакрытая свеча — пропустить
        if (-not $seen.Add($ct)) { continue }    # дедуп
        $rows.Add(("{0},{1},{2},{3},{4},{5},{6}" -f $ct, $k[1], $k[2], $k[3], $k[4], $k[5], $k[9]))
        if ($ct -gt $lastClose) { $lastClose = $ct }
        $chunk++
    }

    $next = [int64]$kl[$kl.Count - 1][6] + 1
    $from = [DateTimeOffset]::FromUnixTimeMilliseconds([int64]$kl[0][6]).UtcDateTime.ToString("yyyy-MM-dd")
    $to   = [DateTimeOffset]::FromUnixTimeMilliseconds($lastClose).UtcDateTime.ToString("yyyy-MM-dd")
    Write-Host ("  запрос {0,3}: +{1,5} свечей ({2} .. {3}) | всего {4}" -f $reqN, $chunk, $from, $to, $rows.Count)

    if ($kl.Count -lt $limit -or $next -ge $endMs) { break }
    $startMs = $next
    Start-Sleep -Milliseconds 200
}

if ($rows.Count -eq 0) { throw "[fetch] API вернул 0 закрытых свечей — проверь символ/интервал/маркет" }

$sorted = $rows | Sort-Object { [int64]($_.Split(',')[0]) }
$lines = New-Object 'System.Collections.Generic.List[string]'
$lines.Add("close_time,open,high,low,close,volume,taker_buy_volume")
foreach ($r in $sorted) { $lines.Add($r) }
$dest = Join-Path (Get-Location).Path $Out
[System.IO.File]::WriteAllLines($dest, $lines)

$firstTs = [int64]($sorted[0].Split(',')[0])
$lastTs  = [int64]($sorted[$sorted.Count - 1].Split(',')[0])
$fd = [DateTimeOffset]::FromUnixTimeMilliseconds($firstTs).UtcDateTime.ToString("yyyy-MM-dd")
$ld = [DateTimeOffset]::FromUnixTimeMilliseconds($lastTs).UtcDateTime.ToString("yyyy-MM-dd")
Write-Host ("[fetch] OK: {0} свечей за {1} запросов | {2} .. {3} -> {4}" -f $rows.Count, $reqN, $fd, $ld, $Out)
Write-Host  "[fetch] запуск WFA: cargo run --release --bin wfa_bounce -- --data $Out --out результат"
