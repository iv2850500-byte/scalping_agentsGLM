# ============================================================
# P64: wfa_pipeline.ps1 - runner for the WFA conveyor (wfa_pipeline bin)
# ============================================================
# Chains the research pipeline in one command:
#   1) (optional, -Days > 0) download fresh 1h klines from Bybit V5 PROD
#      via the existing download_bybit_klines bin;
#   2) build wfa_pipeline (feature-gated bin, NOT part of release build);
#   3) run the walk-forward replay through the LIVE MomentumStrategy
#      (lib) and print the report (txt + json in wfa_out\).
#
# The strategy inside the replay reads P23_MAX_SPREAD_PCT / P24_FUNDING_PCT
# from the process env (like the live bot). If .env contains those keys,
# they are picked up here (dotenvy is not linked into research bins).
#
# USAGE:
#   powershell -ExecutionPolicy Bypass -File .\wfa_pipeline.ps1
#   powershell -ExecutionPolicy Bypass -File .\wfa_pipeline.ps1 -Days 450
#   powershell -ExecutionPolicy Bypass -File .\wfa_pipeline.ps1 -Data klines\TAOUSDT-1h.csv -Test 720 -Step 720
#   powershell -ExecutionPolicy Bypass -File .\wfa_pipeline.ps1 -Release   # optimized build (profile wfa, no LTO)
#
# NOTE: Bybit V5 klines have taker_buy_volume = 0 -> OFI degenerates (the
# pipeline prints a warning). Prefer CSVs with real taker volume.
# ============================================================
param(
    [string]$Symbol = "TAOUSDT",
    [string]$Data = "",
    [int]$Days = 0,
    [int]$Test = 720,
    [int]$Step = 720,
    [switch]$Release,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
Set-Location $root

function Fail([string]$msg) {
    Write-Host "[wfa] ERROR: $msg" -ForegroundColor Red
    exit 1
}

# ---- pick up strategy env from .env (research bins do not link dotenvy) ----
$envFile = Join-Path $root ".env"
if (Test-Path $envFile) {
    Get-Content $envFile | ForEach-Object {
        $line = $_.Trim()
        if ($line -match "^([A-Z0-9_]+)=(.*)$") {
            $k = $Matches[1]; $v = $Matches[2].Trim('"')
            if ($k -in @("P23_MAX_SPREAD_PCT", "P24_FUNDING_PCT") -and -not [Environment]::GetEnvironmentVariable($k)) {
                Set-Item -Path ("env:" + $k) -Value $v
                Write-Host "[wfa] env from .env: $k=$v"
            }
        }
    }
}

# ---- 1) data: download fresh or reuse existing ----
if ($Days -gt 0) {
    Write-Host "[wfa] step 1: download $Symbol 1h x $Days days from Bybit (PROD, public)"
    cargo run --bin download_bybit_klines -- --symbol $Symbol --interval 1h --days $Days --out data/klines
    if ($LASTEXITCODE -ne 0) { Fail "download_bybit_klines failed" }
    $fresh = Get-ChildItem -Path (Join-Path $root "data\klines") -Filter "$Symbol-1h-*.csv" -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $fresh) { Fail "downloaded CSV not found in data\klines" }
    $Data = $fresh.FullName
    Write-Host "[wfa] using fresh data: $Data"
}

if ([string]::IsNullOrWhiteSpace($Data)) {
    foreach ($cand in @("klines\$Symbol-1h.csv", "data\klines\$Symbol-1h.csv", "$Symbol-1h.csv")) {
        if (Test-Path $cand) { $Data = $cand; break }
    }
}
if ([string]::IsNullOrWhiteSpace($Data) -or -not (Test-Path $Data)) {
    Fail "no data CSV found; pass -Data path or -Days N to download"
}
Write-Host "[wfa] data: $Data"

# ---- 2) build ----
$exe = "target\debug\wfa_pipeline.exe"
if ($Release) {
    if (-not $SkipBuild) {
        Write-Host "[wfa] step 2: cargo build --profile wfa --features wfa --bin wfa_pipeline (no LTO, P32)"
        cargo build --profile wfa --features wfa --bin wfa_pipeline
        if ($LASTEXITCODE -ne 0) { Fail "build failed" }
    }
    $exe = "target\wfa\wfa_pipeline.exe"
} else {
    if (-not $SkipBuild) {
        Write-Host "[wfa] step 2: cargo build --features wfa --bin wfa_pipeline (debug, fast)"
        cargo build --features wfa --bin wfa_pipeline
        if ($LASTEXITCODE -ne 0) { Fail "build failed" }
    }
}
if (-not (Test-Path $exe)) { Fail "exe not found: $exe (build first or drop -SkipBuild)" }

# ---- 3) run ----
Write-Host "[wfa] step 3: run walk-forward replay (live MomentumStrategy + funnel of 17 filters)"
& $exe --data $Data --test $Test --step $Step
$code = $LASTEXITCODE
if ($code -eq 0) {
    Write-Host "[wfa] OK: VERDICT PASS - reports in wfa_out\pipeline_report.txt / .json"
} elseif ($code -eq 2) {
    Write-Host "[wfa] DONE: VERDICT FAIL (research result, not a tool error) - see wfa_out\pipeline_report.txt" -ForegroundColor Yellow
} else {
    Fail "wfa_pipeline exited with code $code"
}
