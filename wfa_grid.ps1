# ============================================================
# P66: wfa_grid.ps1 - threshold grid runner (threshold_grid bin)
# ============================================================
# Chains in one command:
#   1) (optional, -Days > 0) download fresh 1h klines from Bybit V5 PROD;
#   2) build threshold_grid (feature-gated bin, NOT part of release build);
#   3) run the volume/atr/ofi threshold grid through the LIVE
#      MomentumStrategy (lib) and print the report (wfa_out\grid_report.*).
#
# The winners are NOT applied automatically. Validate each winner with a
# repeat walk-forward (env-ized since P66):
#   $env:MOM_VOLUME_MIN="1.2"; $env:MOM_ATR_MIN_PCT="0.3"; $env:MOM_OFI_MIN="0.05"
#   powershell -ExecutionPolicy Bypass -File .\wfa_pipeline.ps1 -Days 450
# Only after a PASS verdict edit config.toml (strategy.params:
# volume_anomaly_threshold / min_atr_threshold / ofi_threshold).
#
# USAGE:
#   powershell -ExecutionPolicy Bypass -File .\wfa_grid.ps1
#   powershell -ExecutionPolicy Bypass -File .\wfa_grid.ps1 -Days 450
#   powershell -ExecutionPolicy Bypass -File .\wfa_grid.ps1 -Vol 1.0,1.2,1.5 -Ofi 0.05,0.1,0.15
# ============================================================
param(
    [string]$Symbol = "TAOUSDT",
    [string]$Data = "",
    [int]$Days = 0,
    [string]$Vol = "1.0,1.1,1.2,1.3,1.5",
    [string]$Atr = "0.05,0.15,0.3,0.5,0.8",
    [string]$Ofi = "0.02,0.05,0.1,0.15,0.25",
    [switch]$Release,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
Set-Location $root

function Fail([string]$msg) {
    Write-Host "[grid] ERROR: $msg" -ForegroundColor Red
    exit 1
}

# ---- pick up strategy env from .env (same as wfa_pipeline.ps1) ----
$envFile = Join-Path $root ".env"
if (Test-Path $envFile) {
    Get-Content $envFile | ForEach-Object {
        $line = $_.Trim()
        if ($line -match "^([A-Z0-9_]+)=(.*)$") {
            $k = $Matches[1]; $v = $Matches[2].Trim('"')
            if ($k -in @("P23_MAX_SPREAD_PCT", "P24_FUNDING_PCT") -and -not [Environment]::GetEnvironmentVariable($k)) {
                Set-Item -Path ("env:" + $k) -Value $v
                Write-Host "[grid] env from .env: $k=$v"
            }
        }
    }
}

# ---- 1) data: download fresh or reuse existing ----
if ($Days -gt 0) {
    Write-Host "[grid] step 1: download $Symbol 1h x $Days days from Bybit (PROD, public)"
    cargo run --bin download_bybit_klines -- --symbol $Symbol --interval 1h --days $Days --out data/klines
    if ($LASTEXITCODE -ne 0) { Fail "download_bybit_klines failed" }
    $fresh = Get-ChildItem -Path (Join-Path $root "data\klines") -Filter "$Symbol-1h-*.csv" -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $fresh) { Fail "downloaded CSV not found in data\klines" }
    $Data = $fresh.FullName
    Write-Host "[grid] using fresh data: $Data"
}

if ([string]::IsNullOrWhiteSpace($Data)) {
    foreach ($cand in @("klines\$Symbol-1h.csv", "data\klines\$Symbol-1h.csv", "$Symbol-1h.csv")) {
        if (Test-Path $cand) { $Data = $cand; break }
    }
}
if ([string]::IsNullOrWhiteSpace($Data) -or -not (Test-Path $Data)) {
    Fail "no data CSV found; pass -Data path or -Days N to download"
}
Write-Host "[grid] data: $Data"

# ---- 2) build ----
$exe = "target\debug\threshold_grid.exe"
if ($Release) {
    if (-not $SkipBuild) {
        Write-Host "[grid] step 2: cargo build --profile wfa --features wfa --bin threshold_grid (no LTO, P32)"
        cargo build --profile wfa --features wfa --bin threshold_grid
        if ($LASTEXITCODE -ne 0) { Fail "build failed" }
    }
    $exe = "target\wfa\threshold_grid.exe"
} else {
    if (-not $SkipBuild) {
        Write-Host "[grid] step 2: cargo build --features wfa --bin threshold_grid (debug, fast)"
        cargo build --features wfa --bin threshold_grid
        if ($LASTEXITCODE -ne 0) { Fail "build failed" }
    }
}
if (-not (Test-Path $exe)) { Fail "exe not found: $exe (build first or drop -SkipBuild)" }

# ---- 3) run ----
Write-Host "[grid] step 3: run threshold grid (vol=$Vol atr=$Atr ofi=$Ofi)"
& $exe --data $Data --vol $Vol --atr $Atr --ofi $Ofi
$code = $LASTEXITCODE
if ($code -eq 0) {
    Write-Host "[grid] DONE: report in wfa_out\grid_report.txt / .json"
    Write-Host "[grid] NEXT: validate the winner via wfa_pipeline.ps1 with MOM_* env (see report tail)"
} else {
    Fail "threshold_grid exited with code $code"
}
