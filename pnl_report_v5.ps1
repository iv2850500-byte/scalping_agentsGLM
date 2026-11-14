# ============================================================================
# pnl_report_v5.ps1  [P10:pnl-report]  BUILD P10.4 [P10.4:meta-attach]
# P10.1: all expressions single-line (no backtick/pipe continuations) -
#        PS 5.1 does not join them in LF-only files (field-tested failure).
# P10.2: multi-DB: journal lives in global state.db, risk_stats/positions in
#        per-bot db_path_override DBs (state_*.db) -> probe sqlite_master,
#        ATTACH, UNION (dedup: live side-DBs win over stale main rows).
# P10.3: risk_stats becomes a first-class source: bots WITHOUT trade_journal
#        (live per-bot DBs) enter 01_per_bot_summary from the risk_stats
#        snapshot (one row per bot, lifetime cumulative, gross_loss stored
#        positive). Bots already present in journal CLOSE keep journal rows.
#        New 11_riskstats_snapshot.csv (raw per-bot snapshot dump).
# P10.4: field hotfix: the risk_stats row-count probe ran against snapDb
#        WITHOUT the ATTACH header -> "no such table: dbx_*.risk_stats";
#        under ErrorActionPreference=Stop the first stderr line of sqlite3.exe
#        (NativeCommandError) aborted the script before the console section.
#        Fix: meta query is prefixed with $attachSql; every native sqlite3
#        call is wrapped in try/catch so stderr can never abort the report
#        mid-way (errors are printed as [!] lines instead).
# SQL PnL-report for scalping bots (SQLite: data\state.db)
# Windows PowerShell 5.1+ | ASCII-only | reads DB via consistent snapshot
# (no locks on the live bot process; WAL-safe)
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File .\pnl_report_v5.ps1
#   params: -DbPath <path> -OutDir <dir> -TopN 20 -Days 0 -InitialBalance 0
#
# Output: reports\pnl_<stamp>\  with 01..11 CSV files + snapshots of all data\*.db
# ============================================================================

param(
    [string]$DbPath = "",
    [string]$OutDir = "",
    [int]$TopN = 20,
    [int]$Days = 0,
    [double]$InitialBalance = 0.0,
    [switch]$NoDownload
)

$ErrorActionPreference = 'Stop'
try { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 } catch {}

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $root
$stamp = Get-Date -Format 'yyyyMMdd_HHmmss'
if (-not $OutDir) { $OutDir = Join-Path $root ('reports\pnl_' + $stamp) }
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null

Write-Host '============================================================'
Write-Host ' P10 SQL PnL-report  [P10:pnl-report]  BUILD P10.4'
Write-Host (' Generated : ' + (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'))
Write-Host (' Root      : ' + $root)
if ($Days -gt 0)   { Write-Host (' Window    : last ' + $Days + ' days (UTC)') }
if ($InitialBalance -gt 0) { Write-Host (' InitBal   : ' + $InitialBalance) }
Write-Host '============================================================'

# ---------------------------------------------------------------- find DB --
function Find-Database {
    param([string]$Explicit)
    if ($Explicit) {
        if (Test-Path $Explicit) { return (Resolve-Path $Explicit).Path }
        throw ('DB not found: ' + $Explicit)
    }
    $found = @()
    $p1 = Join-Path $root 'data\state.db'
    if (Test-Path $p1) { $found += $p1 }
    $dataDir = Join-Path $root 'data'
    if (Test-Path $dataDir) {
        $found += @(Get-ChildItem -Path $dataDir -Filter '*.db' -File -ErrorAction SilentlyContinue | Sort-Object Length -Descending | ForEach-Object { $_.FullName })
    }
    $found += @(Get-ChildItem -Path $root -Filter '*.db' -File -ErrorAction SilentlyContinue | Sort-Object Length -Descending | ForEach-Object { $_.FullName })
    $found = @($found | Select-Object -Unique)
    if ($found.Count -eq 0) { throw 'No *.db found. Expected data\state.db in project root.' }
    if ($found.Count -gt 1) {
        Write-Host '[i] Multiple DB candidates found:'
        foreach ($f in $found) { Write-Host ('      ' + $f) }
        Write-Host ('[i] Using: ' + $found[0] + '  (override with -DbPath)')
    }
    return $found[0]
}

# ------------------------------------------------------------ find sqlite --
function Resolve-Sqlite {
    param([string]$Root, [switch]$NoDownload)
    $local = Join-Path $Root 'tools\sqlite3.exe'
    if (Test-Path $local) { return $local }
    $cmd = Get-Command sqlite3.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    if ($NoDownload) {
        throw 'sqlite3.exe not found in tools\ or PATH; -NoDownload blocks auto-download.'
    }
    Write-Host '[*] sqlite3.exe not found -> downloading sqlite-tools from sqlite.org ...'
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    } catch {}
    $urls = @(
        'https://www.sqlite.org/2026/sqlite-tools-win-x64-3540100.zip',
        'https://www.sqlite.org/2025/sqlite-tools-win-x64-3510100.zip',
        'https://www.sqlite.org/2025/sqlite-tools-win-x64-3500400.zip'
    )
    $zip = Join-Path $env:TEMP ('sqlite_tools_' + $PID + '.zip')
    $tmpdir = Join-Path $env:TEMP ('sqlite_tools_' + $PID)
    $ok = $false
    foreach ($u in $urls) {
        try {
            Write-Host ('    GET ' + $u)
            Invoke-WebRequest -Uri $u -OutFile $zip -UseBasicParsing -TimeoutSec 60
            if (Test-Path $tmpdir) { Remove-Item $tmpdir -Recurse -Force }
            New-Item -ItemType Directory -Path $tmpdir -Force | Out-Null
            Expand-Archive -Path $zip -DestinationPath $tmpdir -Force
            $exe = Get-ChildItem -Path $tmpdir -Filter 'sqlite3.exe' -Recurse -File | Select-Object -First 1
            if ($exe) {
                New-Item -ItemType Directory -Path (Join-Path $Root 'tools') -Force | Out-Null
                Copy-Item $exe.FullName $local -Force
                $ok = $true
                break
            }
        } catch {
            Write-Host ('    WARN: ' + $_.Exception.Message)
        }
    }
    if (-not $ok) {
        throw ('sqlite3.exe download failed. Manual: open https://www.sqlite.org/download.html, ' +
               'download sqlite-tools-win-x64-*.zip (Precompiled Binaries for Windows), ' +
               'put sqlite3.exe into tools\ folder and re-run.')
    }
    Write-Host ('[+] sqlite3.exe ready: ' + $local)
    return $local
}

# ---------------------------------------------------------- snapshot DB ----
$db     = Find-Database -Explicit $DbPath
$sqlite = Resolve-Sqlite -Root $root -NoDownload:$NoDownload
$snapDb = Join-Path $OutDir (Split-Path -Leaf $db)

# ---- list all DBs: main first, then the rest of data\*.db (per-bot overrides)
$allDbs = @($db)
if (-not $DbPath) {
    $dataDir = Join-Path $root 'data'
    $others = @(Get-ChildItem -Path $dataDir -Filter '*.db' -File -ErrorAction SilentlyContinue | Sort-Object Name | ForEach-Object { $_.FullName })
    $allDbs = @($db) + @($others | Where-Object { $_ -ne $db })
}
$allDbs = @($allDbs | Select-Object -Unique)

function New-Snapshot {
    param([string]$Src, [string]$Dst, [string]$Exe)
    $dstFwd = $Dst -replace '\\', '/'
    try { $null = & $Exe -readonly $Src '.timeout 5000' ('.backup ''' + $dstFwd + '''') 2>&1 } catch {}
    $sz = 0
    if (Test-Path $Dst) { $sz = (Get-Item $Dst).Length }
    if ($LASTEXITCODE -ne 0 -or $sz -eq 0) {
        Write-Host '[!] .backup failed -> fallback: file copy db + -wal + -shm'
        Copy-Item $Src $Dst -Force
        if (Test-Path ($Src + '-wal')) { Copy-Item ($Src + '-wal') ($Dst + '-wal') -Force }
        if (Test-Path ($Src + '-shm')) { Copy-Item ($Src + '-shm') ($Dst + '-shm') -Force }
    }
    $chk = @()
    try { $chk = @(& $Exe -batch $Dst 'PRAGMA integrity_check;' 2>&1) } catch { $chk = @($_.Exception.Message) }
    $integ = "$(@($chk)[-1])"
    if ($integ.Trim() -ne 'ok') { throw ('integrity check failed on snapshot ' + $Dst + ': ' + $integ) }
}

function Test-Table {
    param([string]$SnapPath, [string]$Table, [string]$Exe)
    try { $r = & $Exe -readonly $SnapPath ("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='" + $Table + "';") 2>$null } catch { $r = '' }
    return ("$r".Trim() -eq '1')
}

Write-Host ('[*] sqlite3    : ' + $sqlite)
Write-Host ('[i] Will snapshot ' + $allDbs.Count + ' DB(s) into ' + $OutDir)
$snapMap   = @{}
$journalAlias = ''
$rsSources  = @()
$posSources = @()
$needAttach = @()
$prio = 0
foreach ($d in $allDbs) {
    $nm = Split-Path -Leaf $d
    $sd = Join-Path $OutDir $nm
    New-Snapshot -Src $d -Dst $sd -Exe $sqlite
    Write-Host ('[+] Snapshot OK: ' + $nm + ' (integrity ok)')
    $base = [System.IO.Path]::GetFileNameWithoutExtension($nm)
    $isMain = ($d -eq $db)
    $alias = 'main'
    if (-not $isMain) { $alias = 'dbx_' + ($base -replace '[^A-Za-z0-9_]', '_') }
    $hasJ = Test-Table -SnapPath $sd -Table 'trade_journal' -Exe $sqlite
    $hasR = Test-Table -SnapPath $sd -Table 'risk_stats'    -Exe $sqlite
    $hasP = Test-Table -SnapPath $sd -Table 'positions'     -Exe $sqlite
    Write-Host ('    tables: trade_journal=' + $hasJ + ' risk_stats=' + $hasR + ' positions=' + $hasP + '  (as ' + $alias + ')')
    if ($hasJ -and ($journalAlias -eq '')) {
        $journalAlias = $alias
        if (-not $isMain) { $needAttach += @{ alias = $alias; path = $sd } }
    }
    if ($hasR) {
        $thisPrio = 99
        if (-not $isMain) {
            $thisPrio = $prio
            $prio = $prio + 1
            if (-not ($needAttach | Where-Object { $_.alias -eq $alias })) { $needAttach += @{ alias = $alias; path = $sd } }
        }
        $rsSources += @{ alias = $alias; prio = $thisPrio }
    }
    if ($hasP) {
        if (-not $isMain) {
            if (-not ($needAttach | Where-Object { $_.alias -eq $alias })) { $needAttach += @{ alias = $alias; path = $sd } }
        }
        $posSources += @{ alias = $alias; label = $base }
    }
}
if ($journalAlias -eq '') { throw 'trade_journal table not found in any DB' }
Write-Host ('[i] Journal source: ' + $journalAlias + ' | risk_stats sources: ' + $rsSources.Count + ' | positions sources: ' + $posSources.Count)

$attachSql = ''
foreach ($a in $needAttach) {
    $attachSql += "ATTACH DATABASE '" + ($a.path -replace '\\', '/') + "' AS " + $a.alias + ";`n"
}

# union builders: rs CTE = risk_stats across DBs (one row per bot, side-DBs prio
# 0.., main 99); P10.3 adds gross_profit/gross_loss for merged per-bot summary.
$rsCols = 'bot_name, realized_pnl, total_trades, win_trades, loss_trades, gross_profit, gross_loss, daily_loss, stat_day, suspended, suspend_reason'
if ($rsSources.Count -gt 0) {
    $rsParts = @()
    foreach ($s in $rsSources) { $rsParts += ('SELECT ' + $rsCols + ', ' + $s.prio + ' AS src_prio FROM ' + $s.alias + '.risk_stats') }
    $rsCte = 'WITH rs_raw AS (' + ($rsParts -join ' UNION ALL ') + '), rs AS (SELECT bot_name, realized_pnl, total_trades, win_trades, loss_trades, gross_profit, gross_loss, daily_loss, stat_day, suspended, suspend_reason FROM (SELECT *, ROW_NUMBER() OVER (PARTITION BY bot_name ORDER BY src_prio) AS rn FROM rs_raw) WHERE rn = 1)'
} else {
    $rsCte = "WITH rs AS (SELECT '' AS bot_name, '' AS realized_pnl, NULL AS total_trades, NULL AS win_trades, NULL AS loss_trades, '' AS gross_profit, '' AS gross_loss, '' AS daily_loss, '' AS stat_day, NULL AS suspended, '' AS suspend_reason WHERE 0)"
}
$posCols = "bot_name, symbol, side, entry_price, quantity, COALESCE(stop_loss, '') AS stop_loss, COALESCE(take_profit, '') AS take_profit, opened_at, COALESCE(origin_strategy, '') AS origin_strategy, COALESCE(funding_accrued, 0) AS funding_accrued, COALESCE(fees_paid, 0) AS fees_paid, COALESCE(partial_taken, 0) AS partial_taken"
if ($posSources.Count -gt 0) {
    $posParts = @()
    foreach ($s in $posSources) { $posParts += ("SELECT '" + $s.label + "' AS source_db, " + $posCols + ' FROM ' + $s.alias + '.positions') }
    $posQuery = ($posParts -join ' UNION ALL ') + ' ORDER BY bot_name, symbol'
} else {
    $posQuery = "SELECT '' AS source_db, '' AS bot_name, '' AS symbol, '' AS side, '' AS entry_price, '' AS quantity, '' AS stop_loss, '' AS take_profit, '' AS opened_at, '' AS origin_strategy, '' AS funding_accrued, '' AS fees_paid, '' AS partial_taken WHERE 0"
}

# --------------------------------------------------------- build params ----
$daysFilter = ''
if ($Days -gt 0) {
    $daysFilter = ' AND substr(timestamp,1,10) >= date(''now'', ''' + (-$Days) + ' days'')'
}
$topn    = [string][Math]::Max(1, $TopN)
$outFwd  = $OutDir -replace '\\', '/'
$cultInv = [System.Globalization.CultureInfo]::InvariantCulture
$initBal = [double]$InitialBalance.ToString($cultInv)

# ------------------------------------------------------------ SQL script ---
# placeholders: {DAYS_FILTER} {TOPN} {OUTDIR} {INITIAL}
$SQL_MAIN = @'
.bail off
{ATTACH}
.headers on
.mode csv
.once '{OUTDIR}/01_per_bot_summary.csv'
{RS_CTE}
SELECT * FROM (
SELECT 'journal' AS source,
       j.bot_name,
       j.trades,
       j.wins,
       j.losses,
       j.winrate_pct,
       j.gross_profit,
       j.gross_loss,
       j.profit_factor,
       j.net_pnl,
       j.expectancy,
       j.best_trade,
       j.worst_trade,
       j.first_trade,
       j.last_trade,
       CAST(r.suspended AS INTEGER) AS suspended
FROM (
    SELECT bot_name,
           COUNT(*) AS trades,
           SUM(CASE WHEN pnl > 0 THEN 1 ELSE 0 END) AS wins,
           SUM(CASE WHEN pnl < 0 THEN 1 ELSE 0 END) AS losses,
           ROUND(100.0 * SUM(CASE WHEN pnl > 0 THEN 1 ELSE 0 END)
                 / NULLIF(SUM(CASE WHEN pnl <> 0 THEN 1 ELSE 0 END), 0), 1) AS winrate_pct,
           ROUND(SUM(CASE WHEN pnl > 0 THEN pnl ELSE 0 END), 4) AS gross_profit,
           ROUND(-SUM(CASE WHEN pnl < 0 THEN pnl ELSE 0 END), 4) AS gross_loss,
           ROUND(SUM(CASE WHEN pnl > 0 THEN pnl ELSE 0 END)
                 / NULLIF(-SUM(CASE WHEN pnl < 0 THEN pnl ELSE 0 END), 0), 2) AS profit_factor,
           ROUND(SUM(pnl), 4) AS net_pnl,
           ROUND(SUM(pnl) / COUNT(*), 4) AS expectancy,
           ROUND(MAX(pnl), 4) AS best_trade,
           ROUND(MIN(pnl), 4) AS worst_trade,
           MIN(substr(timestamp, 1, 10)) AS first_trade,
           MAX(substr(timestamp, 1, 10)) AS last_trade
    FROM {JRN}.trade_journal
    WHERE event = 'CLOSE'{DAYS_FILTER}
    GROUP BY bot_name
) j
LEFT JOIN rs r ON r.bot_name = j.bot_name
UNION ALL
SELECT 'risk_stats' AS source,
       r.bot_name,
       CAST(r.total_trades AS INTEGER),
       CAST(r.win_trades AS INTEGER),
       CAST(r.loss_trades AS INTEGER),
       ROUND(100.0 * CAST(r.win_trades AS REAL)
             / NULLIF(CAST(r.total_trades AS REAL), 0), 1) AS winrate_pct,
       ROUND(CAST(r.gross_profit AS REAL), 4) AS gross_profit,
       ROUND(CAST(r.gross_loss AS REAL), 4) AS gross_loss,
       ROUND(CAST(r.gross_profit AS REAL)
             / NULLIF(CAST(r.gross_loss AS REAL), 0), 2) AS profit_factor,
       ROUND(CAST(r.realized_pnl AS REAL), 4) AS net_pnl,
       ROUND(CAST(r.realized_pnl AS REAL)
             / NULLIF(CAST(r.total_trades AS REAL), 0), 4) AS expectancy,
       NULL AS best_trade,
       NULL AS worst_trade,
       '' AS first_trade,
       r.stat_day AS last_trade,
       CAST(r.suspended AS INTEGER) AS suspended
FROM rs r
WHERE r.bot_name NOT IN (
    SELECT DISTINCT bot_name FROM {JRN}.trade_journal
    WHERE event = 'CLOSE'{DAYS_FILTER}
)
)
ORDER BY net_pnl DESC;

.once '{OUTDIR}/02_funnel.csv'
SELECT bot_name,
       SUM(CASE WHEN event = 'SIGNAL' THEN 1 ELSE 0 END) AS signals,
       SUM(CASE WHEN event = 'EXECUTION' THEN 1 ELSE 0 END) AS executions,
       SUM(CASE WHEN event = 'CLOSE' THEN 1 ELSE 0 END) AS closes,
       SUM(CASE WHEN event = 'EXECUTION' AND details = 'PLACED' THEN 1 ELSE 0 END) AS placed,
       SUM(CASE WHEN event = 'EXECUTION' AND details = 'MISSED' THEN 1 ELSE 0 END) AS missed
FROM {JRN}.trade_journal
WHERE 1 = 1{DAYS_FILTER}
GROUP BY bot_name
ORDER BY bot_name;

.once '{OUTDIR}/03_execution_status.csv'
SELECT bot_name, COALESCE(details, '') AS status, COUNT(*) AS cnt
FROM {JRN}.trade_journal
WHERE event = 'EXECUTION'{DAYS_FILTER}
GROUP BY bot_name, details
ORDER BY bot_name, cnt DESC;

.once '{OUTDIR}/04_daily_pnl_bot.csv'
SELECT substr(timestamp, 1, 10) AS day,
       bot_name,
       COUNT(*) AS closes,
       ROUND(SUM(pnl), 4) AS net_pnl,
       ROUND(SUM(CASE WHEN pnl > 0 THEN pnl ELSE 0 END), 4) AS gross_profit,
       ROUND(-SUM(CASE WHEN pnl < 0 THEN pnl ELSE 0 END), 4) AS gross_loss
FROM {JRN}.trade_journal
WHERE event = 'CLOSE'{DAYS_FILTER}
GROUP BY day, bot_name
ORDER BY day, bot_name;

.once '{OUTDIR}/05_daily_pnl_portfolio.csv'
SELECT substr(timestamp, 1, 10) AS day,
       COUNT(*) AS closes,
       ROUND(SUM(pnl), 4) AS net_pnl,
       ROUND(SUM(CASE WHEN pnl > 0 THEN pnl ELSE 0 END), 4) AS gross_profit,
       ROUND(-SUM(CASE WHEN pnl < 0 THEN pnl ELSE 0 END), 4) AS gross_loss
FROM {JRN}.trade_journal
WHERE event = 'CLOSE'{DAYS_FILTER}
GROUP BY day
ORDER BY day;

.once '{OUTDIR}/06_equity_curve.csv'
WITH raw AS (
    SELECT bot_name, id, timestamp, pnl
    FROM {JRN}.trade_journal
    WHERE event = 'CLOSE'{DAYS_FILTER}
    UNION ALL
    SELECT bot_name, 0 AS id, '' AS timestamp, 0.0 AS pnl
    FROM (SELECT DISTINCT bot_name FROM {JRN}.trade_journal WHERE event = 'CLOSE'{DAYS_FILTER})
),
cum AS (
    SELECT bot_name, id, timestamp, pnl,
           SUM(pnl) OVER (PARTITION BY bot_name ORDER BY id, timestamp) AS cum_pnl
    FROM raw
),
eq AS (
    SELECT bot_name, id, timestamp, pnl, cum_pnl,
           {INITIAL} + cum_pnl AS equity,
           MAX({INITIAL} + cum_pnl) OVER (
               PARTITION BY bot_name ORDER BY id, timestamp
               ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS peak
    FROM cum
)
SELECT bot_name, id, timestamp,
       ROUND(pnl, 4) AS pnl,
       ROUND(equity, 4) AS equity,
       ROUND(peak, 4) AS peak,
       ROUND(peak - equity, 4) AS drawdown,
       ROUND(100.0 * (peak - equity) / NULLIF(peak, 0), 2) AS drawdown_pct
FROM eq
WHERE id > 0
ORDER BY bot_name, id, timestamp;

.once '{OUTDIR}/07_max_drawdown.csv'
WITH raw AS (
    SELECT bot_name, id, timestamp, pnl
    FROM {JRN}.trade_journal
    WHERE event = 'CLOSE'{DAYS_FILTER}
    UNION ALL
    SELECT bot_name, 0 AS id, '' AS timestamp, 0.0 AS pnl
    FROM (SELECT DISTINCT bot_name FROM {JRN}.trade_journal WHERE event = 'CLOSE'{DAYS_FILTER})
),
cum AS (
    SELECT bot_name, id, timestamp, pnl,
           SUM(pnl) OVER (PARTITION BY bot_name ORDER BY id, timestamp) AS cum_pnl
    FROM raw
),
eq AS (
    SELECT bot_name, id, timestamp, pnl, cum_pnl,
           {INITIAL} + cum_pnl AS equity,
           MAX({INITIAL} + cum_pnl) OVER (
               PARTITION BY bot_name ORDER BY id, timestamp
               ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS peak
    FROM cum
),
dd AS (
    SELECT bot_name, id, timestamp, pnl, equity, peak, peak - equity AS drawdown
    FROM eq
    WHERE id > 0
),
ranked AS (
    SELECT bot_name, id, timestamp, pnl,
           ROUND(equity, 4) AS equity_at,
           ROUND(peak, 4) AS peak_at,
           ROUND(drawdown, 4) AS max_dd,
           ROW_NUMBER() OVER (PARTITION BY bot_name ORDER BY drawdown DESC, id ASC) AS rn
    FROM dd
)
SELECT bot_name, max_dd, timestamp AS at_time, pnl AS trade_pnl, equity_at, peak_at
FROM ranked
WHERE rn = 1
ORDER BY bot_name;

.once '{OUTDIR}/08_risk_stats_recon.csv'
{RS_CTE}
SELECT j.bot_name,
       ROUND(j.journal_net, 4) AS journal_net_pnl,
       j.journal_closes,
       r.realized_pnl AS risk_realized_pnl,
       r.total_trades AS risk_total_trades,
       r.win_trades AS risk_wins,
       r.loss_trades AS risk_losses,
       r.daily_loss AS risk_daily_loss,
       r.stat_day AS risk_stat_day,
       r.suspended AS suspended,
       r.suspend_reason
FROM (
    SELECT bot_name, SUM(pnl) AS journal_net, COUNT(*) AS journal_closes
    FROM {JRN}.trade_journal
    WHERE event = 'CLOSE'{DAYS_FILTER}
    GROUP BY bot_name
) j
LEFT JOIN rs r ON r.bot_name = j.bot_name
ORDER BY j.bot_name;

.once '{OUTDIR}/09_open_positions.csv'
{POS_QUERY};

.once '{OUTDIR}/10_recent_trades.csv'
SELECT id, bot_name, timestamp, symbol, side, price, quantity, pnl,
       COALESCE(details, '') AS details
FROM {JRN}.trade_journal
WHERE event = 'CLOSE'{DAYS_FILTER}
ORDER BY id DESC
LIMIT {TOPN};

.once '{OUTDIR}/11_riskstats_snapshot.csv'
{RS_CTE}
SELECT bot_name,
       CAST(total_trades AS INTEGER) AS total_trades,
       CAST(win_trades AS INTEGER) AS win_trades,
       CAST(loss_trades AS INTEGER) AS loss_trades,
       CAST(realized_pnl AS REAL) AS realized_pnl,
       CAST(gross_profit AS REAL) AS gross_profit,
       CAST(gross_loss AS REAL) AS gross_loss,
       CAST(daily_loss AS REAL) AS daily_loss_today,
       stat_day AS last_active_day,
       CAST(suspended AS INTEGER) AS suspended,
       suspend_reason
FROM rs
ORDER BY bot_name;
'@

$sql = $SQL_MAIN.Replace('{DAYS_FILTER}', $daysFilter).Replace('{TOPN}', $topn).Replace('{OUTDIR}', $outFwd).Replace('{INITIAL}', $initBal).Replace('{ATTACH}', $attachSql).Replace('{JRN}', $journalAlias).Replace('{RS_CTE}', $rsCte).Replace('{POS_QUERY}', $posQuery)
$sqlPath = Join-Path $OutDir '_report.sql'
Set-Content -Path $sqlPath -Value $sql -Encoding ASCII

# --------------------------------------------------------------- execute ---
Write-Host '[*] Running SQL report ...'
try {
    & $sqlite -batch $snapDb ('.read ''' + ($sqlPath -replace '\\', '/') + '''') 2>&1 | ForEach-Object { Write-Host ('    ' + $_) }
} catch {
    Write-Host ('[!] SQL report error: ' + $_.Exception.Message)
}
if ($LASTEXITCODE -ne 0) { Write-Host '[!] some SQL sections reported errors (see lines above) - CSVs may be partial' }

$metaSql = "SELECT '" + $journalAlias + ":trade_journal_rows', COUNT(*) FROM " + $journalAlias + ".trade_journal;"
Write-Host '[+] Row counts:'
try {
    $meta = @(& $sqlite -batch $snapDb $metaSql 2>&1)
    $meta | ForEach-Object { Write-Host ('    ' + $_) }
} catch {
    Write-Host ('[!] journal row-count probe failed: ' + $_.Exception.Message)
}
if ($rsSources.Count -gt 0) {
    $metaRs = @()
    foreach ($s in $rsSources) { $metaRs += ("SELECT '" + $s.alias + ":risk_stats_rows', COUNT(*) FROM " + $s.alias + ".risk_stats") }
    $metaSql2 = $attachSql + (($metaRs -join ' UNION ALL ') + ';')
    try {
        $meta2 = @(& $sqlite -batch $snapDb $metaSql2 2>&1)
        $meta2 | ForEach-Object { Write-Host ('    ' + $_) }
    } catch {
        Write-Host ('[!] risk_stats row-count probe failed: ' + $_.Exception.Message)
    }
}

# ---------------------------------------------------------- console view ---
function Show-Csv {
    param([string]$Path, [string]$Title, [int]$Width = 220)
    Write-Host ''
    Write-Host ('=== ' + $Title + ' ===')
    if (-not (Test-Path $Path)) { Write-Host '  (missing)'; return }
    $rows = @(Import-Csv -Path $Path -Encoding UTF8)
    if ($rows.Count -eq 0) { Write-Host '  (no rows yet)'; return }
    $rows | Format-Table -AutoSize | Out-String -Width $Width | Write-Host
}

Show-Csv -Path (Join-Path $OutDir '01_per_bot_summary.csv') -Title 'Per-bot summary (journal CLOSE + risk_stats snapshot)'
Show-Csv -Path (Join-Path $OutDir '07_max_drawdown.csv')   -Title 'Max drawdown (equity low vs running peak; journal bots only)'
Show-Csv -Path (Join-Path $OutDir '02_funnel.csv')         -Title 'Signal funnel (SIGNAL -> EXECUTION -> CLOSE; journal bots only)'
Show-Csv -Path (Join-Path $OutDir '08_risk_stats_recon.csv') -Title 'Journal vs risk_stats reconciliation'
Show-Csv -Path (Join-Path $OutDir '11_riskstats_snapshot.csv') -Title 'risk_stats snapshot (one row per bot, lifetime)'
Show-Csv -Path (Join-Path $OutDir '09_open_positions.csv') -Title 'Open positions'
Show-Csv -Path (Join-Path $OutDir '10_recent_trades.csv')  -Title ('Recent ' + $topn + ' closed trades')

Write-Host ''
Write-Host ('[+] CSV files in: ' + $OutDir)
Get-ChildItem -Path $OutDir -Filter '*.csv' -File | ForEach-Object {
    Write-Host ('      ' + $_.Name + '  (' + $_.Length + ' bytes)')
}
Write-Host '[i] pnl values are the bot journal accounting (net of fees as recorded at CLOSE).'
Write-Host '[i] risk_stats = one row per bot, lifetime cumulative (upsert by bot_name);'
Write-Host '[i] gross_loss is stored positive; zero-pnl closes are not counted in total_trades.'
Write-Host '[i] Live bots w/o trade_journal: per-trade metrics (funnel/daily/equity/DD) are'
Write-Host '[i] journal-only; their numbers come from source=risk_stats rows in the summary.'
Write-Host '[i] Testnet data. drawdown_pct is empty while peak equity = 0 (use -InitialBalance).'
Write-Host '[i] Re-run SQL anytime:  sqlite3 snapshot.db ".read _report.sql"'
Write-Host '=== P10 done ==='
