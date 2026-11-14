# =============================================================================
# P33 run_wfa_p33.ps1
# Runs the 6 reference WFA runs of the momentum strategy with wfa_momentum.exe
# and compares n / R / PF against the P31 etalon (must match 1:1).
#
# Usage:   cd P33 ; .\run_wfa_p33.ps1
# Exit code: 0 = all 6 runs match the etalon, 1 = mismatch or failure.
# Each run also leaves run_<name>.log in the P33 folder (Tee).
# No network access, no compilation, pure offline CSV calculation.
# =============================================================================

$ErrorActionPreference = 'Stop'

$root    = $PSScriptRoot
$exe     = Join-Path $root 'wfa_momentum.exe'
$dataDir = Join-Path $root 'data'

# ---- preflight: exe and data files must exist ------------------------------
$missing = @()
if (-not (Test-Path -LiteralPath $exe)) {
    $missing += 'wfa_momentum.exe'
}
$requiredData = @(
    'TAOUSDT-1h-full.csv',
    'TAOUSDT-1h-450d.csv',
    'BTCUSDT-1h-full.csv',
    'ETHUSDT-1h-full.csv',
    'SOLUSDT-1h-full.csv'
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

# ---- run definitions: commands mirror README_P31.md 1:1 --------------------
# P31 commands were:
#   cargo run --release --bin wfa_momentum -- --data TAOUSDT-1h-full.csv --out run_taofull
#   cargo run --release --bin wfa_momentum -- --data TAOUSDT-1h-450d.csv --out run_tao450
#   cargo run --release --bin wfa_momentum -- --data TAOUSDT-1h-450d.csv --fee 0.0006 --spread 0.0003 --out run_tao450_stress
#   cargo run --release --bin wfa_momentum -- --data BTCUSDT-1h-full.csv --out run_btc450
#   cargo run --release --bin wfa_momentum -- --data ETHUSDT-1h-full.csv --out run_eth450
#   cargo run --release --bin wfa_momentum -- --data SOLUSDT-1h-full.csv --out run_sol450
# Here the standalone exe replaces cargo; flags and values are identical
# (fee 0.0005 / spread 0.0002 are the exe defaults, as in P31).
$runs = @(
    @{ name = 'run_taofull';       data = 'TAOUSDT-1h-full.csv'; extra = @();
        exp20 = @{ n = '159'; R = '-29.68'; PF = '0.76' }; exp25 = @{ n = '162'; R = '-51.86'; PF = '0.64' } },
    @{ name = 'run_tao450';        data = 'TAOUSDT-1h-450d.csv'; extra = @();
        exp20 = @{ n = '76';  R = '-10.89'; PF = '0.81' }; exp25 = @{ n = '80';  R = '-19.51'; PF = '0.72' } },
    @{ name = 'run_tao450_stress'; data = 'TAOUSDT-1h-450d.csv'; extra = @('--fee','0.0006','--spread','0.0003');
        exp20 = @{ n = '76';  R = '-13.17'; PF = '0.78' }; exp25 = @{ n = '80';  R = '-22.51'; PF = '0.68' } },
    @{ name = 'run_btc450';        data = 'BTCUSDT-1h-full.csv'; extra = @();
        exp20 = @{ n = '74';  R = '-20.62'; PF = '0.66' }; exp25 = @{ n = '76';  R = '-28.85'; PF = '0.58' } },
    @{ name = 'run_eth450';        data = 'ETHUSDT-1h-full.csv'; extra = @();
        exp20 = @{ n = '54';  R = '-19.02'; PF = '0.58' }; exp25 = @{ n = '54';  R = '-24.27'; PF = '0.51' } },
    @{ name = 'run_sol450';        data = 'SOLUSDT-1h-full.csv'; extra = @();
        exp20 = @{ n = '56';  R = '-6.24';  PF = '0.85' }; exp25 = @{ n = '57';  R = '+0.22';  PF = '1.01' } }
)

# Plateau line format (stdout of the exe):
#   lb12 ofi0.10 rr2.0 mtp2.0% va1.0         n=159 WR=31.4% R=-29.68 PF=0.76 maxDD=38.24R avg=-0.187R
function Get-PlateauStats {
    param(
        [string] $LogPath,
        [string] $RrTag
    )
    $rrEsc = $RrTag -replace '\.', '\.'
    $pattern = '^lb12 ofi0\.10 ' + $rrEsc + ' mtp2\.0% va1\.0\s+n=(\d+) WR=([0-9.]+)% R=([+-][0-9]+\.[0-9]{2}) PF=([0-9]+\.[0-9]{2})'
    $m = Select-String -LiteralPath $LogPath -Pattern $pattern | Select-Object -Last 1
    if ($null -eq $m) {
        return $null
    }
    return @{ n = $m.Matches[0].Groups[1].Value; R = $m.Matches[0].Groups[3].Value; PF = $m.Matches[0].Groups[4].Value }
}

$failed      = $false
$summaryRows = @()

foreach ($r in $runs) {
    $logPath = Join-Path $root ($r.name + '.log')
    Write-Host ''
    Write-Host ('=== ' + $r.name + '  (data\' + $r.data + ') ===')

    $cmdArgs = @('--data', (Join-Path $dataDir $r.data), '--out', $r.name) + $r.extra

    # stdout goes to the log (like the P31 reference logs); stderr passes to console.
    $output = & $exe @cmdArgs
    $exitCode = $LASTEXITCODE
    $output | Tee-Object -FilePath $logPath | Out-Null

    if ($exitCode -ne 0) {
        Write-Host ('[!] ' + $r.name + ': exe exited with code ' + $exitCode + ' - see console output above')
        $summaryRows += [pscustomobject]@{ Run = $r.name; Cfg = '-'; Got_n = '-'; Got_R = '-'; Got_PF = '-'; Exp_n = '-'; Exp_R = '-'; Exp_PF = '-'; Verdict = 'EXE_FAIL' }
        $failed = $true
        continue
    }

    foreach ($pair in @(@('rr2.0', 'exp20'), @('rr2.5', 'exp25'))) {
        $rrTag = $pair[0]
        $exp   = $r[$pair[1]]
        $got   = Get-PlateauStats -LogPath $logPath -RrTag $rrTag
        if ($null -eq $got) {
            Write-Host ('[!] ' + $r.name + ' ' + $rrTag + ': plateau line not found in log')
            $summaryRows += [pscustomobject]@{ Run = $r.name; Cfg = $rrTag; Got_n = '?'; Got_R = '?'; Got_PF = '?'; Exp_n = $exp.n; Exp_R = $exp.R; Exp_PF = $exp.PF; Verdict = 'PARSE_FAIL' }
            $failed = $true
        } else {
            $ok = ($got.n -eq $exp.n) -and ($got.R -eq $exp.R) -and ($got.PF -eq $exp.PF)
            $verdict = 'MATCH'
            if (-not $ok) { $verdict = 'MISMATCH'; $failed = $true }
            Write-Host ('  ' + $rrTag + ': got n=' + $got.n + ' R=' + $got.R + ' PF=' + $got.PF + ' | expected n=' + $exp.n + ' R=' + $exp.R + ' PF=' + $exp.PF + ' -> ' + $verdict)
            $summaryRows += [pscustomobject]@{ Run = $r.name; Cfg = $rrTag; Got_n = $got.n; Got_R = $got.R; Got_PF = $got.PF; Exp_n = $exp.n; Exp_R = $exp.R; Exp_PF = $exp.PF; Verdict = $verdict }
        }
    }
}

# ---- final summary: got vs expected ----------------------------------------
Write-Host ''
Write-Host '==================== P33 SUMMARY (got vs expected, P31 etalon) ===================='
$fmt = '{0,-17} {1,-6} {2,7} {3,8} {4,7} {5,7} {6,8} {7,7} {8,10}'
Write-Host ($fmt -f 'RUN','CFG','GOT n','GOT R','GOT PF','EXP n','EXP R','EXP PF','VERDICT')
Write-Host ('-' * 92)
foreach ($row in $summaryRows) {
    Write-Host ($fmt -f $row.Run, $row.Cfg, $row.Got_n, $row.Got_R, $row.Got_PF, $row.Exp_n, $row.Exp_R, $row.Exp_PF, $row.Verdict)
}
Write-Host ('-' * 92)

if ($failed) {
    Write-Host 'RESULT: FAIL - numbers differ from the P31 etalon (or a run failed).'
    Write-Host 'Send all run_*.log files for diagnosis.'
    exit 1
}

Write-Host 'RESULT: OK - all 6 reference runs match the P31 etalon 1:1.'
Write-Host 'Result dirs run_<name>/ (summary.json, per_fold.csv, oos_trades_production.csv) and run_*.log are saved.'
exit 0

