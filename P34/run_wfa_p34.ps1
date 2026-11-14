# =============================================================================
# P34 run_wfa_p34.ps1
# Runs the 5 reference WFA runs of the BounceTrader strategy with wfa_bounce.exe
# and compares n / WR / R / PF against the P30b etalon (must match 1:1).
#
# Usage:   cd P34 ; .\run_wfa_p34.ps1
# Exit code: 0 = all 5 runs match the etalon, 1 = mismatch or failure.
# Each run also leaves run_<name>.log in the P34 folder (Tee).
# No network access, no compilation, pure offline CSV calculation.
# =============================================================================

$ErrorActionPreference = 'Stop'

$root    = $PSScriptRoot
$exe     = Join-Path $root 'wfa_bounce.exe'
$dataDir = Join-Path $root 'data'

# ---- preflight: exe and data files must exist ------------------------------
$missing = @()
if (-not (Test-Path -LiteralPath $exe)) {
    $missing += 'wfa_bounce.exe'
}
$requiredData = @(
    'BTCUSDT-1h-full.csv',
    'ETHUSDT-1h-full.csv',
    'SOLUSDT-1h-full.csv',
    'BTCUSDT-1h-720d.csv'
)
foreach ($f in $requiredData) {
    if (-not (Test-Path -LiteralPath (Join-Path $dataDir $f))) {
        $missing += ('data\' + $f)
    }
}
if ($missing.Count -gt 0) {
    Write-Host '[!] Missing files:'
    foreach ($m in $missing) { Write-Host ('    ' + $m) }
    Write-Host '[!] Unpack the zip fully (Expand-Archive) before running.'
    exit 1
}

# UTF-8 console output so the exe log (Cyrillic headers) is not garbled.
try {
    [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false
} catch {
    # older hosts may not allow this; parsing below is ASCII-only anyway
}

Set-Location -LiteralPath $root

# ---- run definitions: commands mirror README_P30b.md 1:1 --------------------
# P30b commands were:
#   cargo run --release --bin wfa_bounce -- --out run_fresh_2026
#   cargo run --release --bin wfa_bounce -- --fee 0.0006 --spread 0.0003 --out run_fresh_stress
#   cargo run --release --bin wfa_bounce -- --data ETHUSDT-1h-full.csv --out eth
#   cargo run --release --bin wfa_bounce -- --data SOLUSDT-1h-full.csv --out sol
#   cargo run --release --bin wfa_bounce -- --data BTCUSDT-1h-720d.csv --out btc720
# Here the standalone exe replaces cargo; flags and values are identical
# (fee 0.0005 / spread 0.0002 / funding 0.0001 / bootstrap seed 42 are the
# exe defaults, exactly as in P30b).
$runs = @(
    @{ name = 'run_fresh_2026';   data = 'BTCUSDT-1h-full.csv'; extra = @();
        exp = @{ n = '18'; WR = '55.6'; R = '+7.15'; PF = '1.72' } },
    @{ name = 'run_fresh_stress'; data = 'BTCUSDT-1h-full.csv'; extra = @('--fee','0.0006','--spread','0.0003');
        exp = @{ n = '11'; WR = '45.5'; R = '+0.97'; PF = '1.13' } },
    @{ name = 'eth';              data = 'ETHUSDT-1h-full.csv'; extra = @();
        exp = @{ n = '13'; WR = '38.5'; R = '-0.61'; PF = '0.94' } },
    @{ name = 'sol';              data = 'SOLUSDT-1h-full.csv'; extra = @();
        exp = @{ n = '5';  WR = '0.0';  R = '-6.35'; PF = '0.00' } },
    @{ name = 'btc720';           data = 'BTCUSDT-1h-720d.csv'; extra = @();
        exp = @{ n = '26'; WR = '50.0'; R = '+5.81'; PF = '1.36' } }
)

# Production OOS line format (stdout of the exe, the line right after the
# '=== PRODUCTION CONFIG OOS (fix, 1:1 ...)' header):
#   n=18 WR=55.6% R=+7.15 PF=1.72 maxDD=6.26R avg=+0.397R
# We anchor on the header first so plateau-table lines can never be picked.
function Get-ProductionStats {
    param([string] $LogPath)
    $lines = Get-Content -LiteralPath $LogPath
    $inSection = $false
    foreach ($line in $lines) {
        if (-not $inSection) {
            if ($line -like '*PRODUCTION CONFIG OOS*') { $inSection = $true }
            continue
        }
        if ($line -match '^n=(\d+) WR=([0-9.]+)% R=([+-][0-9]+\.[0-9]{2}) PF=([0-9]+\.[0-9]{2})') {
            return @{ n = $Matches[1]; WR = $Matches[2]; R = $Matches[3]; PF = $Matches[4] }
        }
    }
    return $null
}

$failed      = $false
$summaryRows = @()

foreach ($r in $runs) {
    $logPath = Join-Path $root ($r.name + '.log')
    Write-Host ''
    Write-Host ('=== ' + $r.name + '  (data\' + $r.data + ') ===')

    $cmdArgs = @('--data', (Join-Path $dataDir $r.data), '--out', $r.name) + $r.extra

    # stdout goes to the log (like the P30b reference logs); stderr passes to console.
    $output = & $exe @cmdArgs
    $exitCode = $LASTEXITCODE
    $output | Tee-Object -FilePath $logPath | Out-Null

    if ($exitCode -ne 0) {
        Write-Host ('[!] ' + $r.name + ': exe exited with code ' + $exitCode + ' - see console output above')
        $summaryRows += [pscustomobject]@{ Run = $r.name; Got_n = '-'; Got_WR = '-'; Got_R = '-'; Got_PF = '-'; Exp_n = '-'; Exp_WR = '-'; Exp_R = '-'; Exp_PF = '-'; Verdict = 'EXE_FAIL' }
        $failed = $true
        continue
    }

    $exp = $r.exp
    $got = Get-ProductionStats -LogPath $logPath
    if ($null -eq $got) {
        Write-Host ('[!] ' + $r.name + ': production OOS line not found in log')
        $summaryRows += [pscustomobject]@{ Run = $r.name; Got_n = '?'; Got_WR = '?'; Got_R = '?'; Got_PF = '?'; Exp_n = $exp.n; Exp_WR = $exp.WR; Exp_R = $exp.R; Exp_PF = $exp.PF; Verdict = 'PARSE_FAIL' }
        $failed = $true
    } else {
        $ok = ($got.n -eq $exp.n) -and ($got.WR -eq $exp.WR) -and ($got.R -eq $exp.R) -and ($got.PF -eq $exp.PF)
        $verdict = 'MATCH'
        if (-not $ok) { $verdict = 'MISMATCH'; $failed = $true }
        Write-Host ('  got n=' + $got.n + ' WR=' + $got.WR + '% R=' + $got.R + ' PF=' + $got.PF + ' | expected n=' + $exp.n + ' WR=' + $exp.WR + '% R=' + $exp.R + ' PF=' + $exp.PF + ' -> ' + $verdict)
        $summaryRows += [pscustomobject]@{ Run = $r.name; Got_n = $got.n; Got_WR = $got.WR; Got_R = $got.R; Got_PF = $got.PF; Exp_n = $exp.n; Exp_WR = $exp.WR; Exp_R = $exp.R; Exp_PF = $exp.PF; Verdict = $verdict }
    }
}

# ---- final summary: got vs expected ----------------------------------------
# P34 NOTE (P33a lesson): in .NET format strings a positive width already
# means right-align; the alignment field accepts only an integer.
Write-Host ''
Write-Host '==================== P34 SUMMARY (got vs expected, P30b etalon) ===================='
$fmt = '{0,-16} {1,5} {2,6} {3,7} {4,6} {5,5} {6,6} {7,7} {8,6} {9,11}'
Write-Host ($fmt -f 'RUN','GOT n','GOT WR','GOT R','GOT PF','EXP n','EXP WR','EXP R','EXP PF','VERDICT')
Write-Host ('-' * 86)
foreach ($row in $summaryRows) {
    Write-Host ($fmt -f $row.Run, $row.Got_n, $row.Got_WR, $row.Got_R, $row.Got_PF, $row.Exp_n, $row.Exp_WR, $row.Exp_R, $row.Exp_PF, $row.Verdict)
}
Write-Host ('-' * 86)

if ($failed) {
    Write-Host 'RESULT: FAIL - numbers differ from the P30b etalon (or a run failed).'
    Write-Host 'Send all run_*.log files for diagnosis.'
    exit 1
}

Write-Host 'RESULT: OK - all 5 reference runs match the P30b etalon 1:1.'
Write-Host 'Result dirs run_<name>/ (summary.json, per_fold.csv, oos_trades_production.csv) and run_*.log are saved.'
exit 0
