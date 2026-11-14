# P32 run script: the 6 WFA acceptance runs via the no-LTO profile 'wfa'.
# Expected numbers must match the P31 reference table 1:1 (README_P32.md),
# because float math does not depend on LTO.

$ErrorActionPreference = 'Continue'

# --- locate project root ---
$root = $null
foreach ($c in @((Get-Location).Path, (Split-Path -Parent $PSScriptRoot))) {
    if (Test-Path -LiteralPath (Join-Path $c 'Cargo.toml')) { $root = $c; break }
}
if (-not $root) {
    throw 'Cargo.toml not found. Run this from the project root (C:\Users\BASE_PC\scalping_agentsGLM).'
}
Set-Location -LiteralPath $root
Write-Host "[run] project root: $root"

# --- make sure [profile.wfa] exists (auto-apply if apply_p32.ps1 was skipped) ---
$tomlPath = Join-Path $root 'Cargo.toml'
$tomlText = Get-Content -LiteralPath $tomlPath -Raw
if ($tomlText -notmatch '(?m)^\s*\[profile\.wfa\]') {
    Write-Host '[run] [profile.wfa] missing - running apply_p32.ps1 first...' -ForegroundColor Yellow
    & (Join-Path $PSScriptRoot 'apply_p32.ps1')
    $tomlText = Get-Content -LiteralPath $tomlPath -Raw
    if ($tomlText -notmatch '(?m)^\s*\[profile\.wfa\]') {
        throw '[run] could not add [profile.wfa] - patch Cargo.toml manually (README_P32.md).'
    }
}

$runs = @(
    @{ data = 'TAOUSDT-1h-full.csv'; out = 'run_taofull';       extra = @() },
    @{ data = 'TAOUSDT-1h-450d.csv'; out = 'run_tao450';        extra = @() },
    @{ data = 'TAOUSDT-1h-450d.csv'; out = 'run_tao450_stress'; extra = @('--fee', '0.0006', '--spread', '0.0003') },
    @{ data = 'BTCUSDT-1h-full.csv'; out = 'run_btc450';        extra = @() },
    @{ data = 'ETHUSDT-1h-full.csv'; out = 'run_eth450';        extra = @() },
    @{ data = 'SOLUSDT-1h-full.csv'; out = 'run_sol450';        extra = @() }
)

$failed  = @()
$skipped = @()

foreach ($r in $runs) {
    $dataPath = Join-Path $root $r.data
    if (-not (Test-Path -LiteralPath $dataPath)) {
        Write-Host ("[run] SKIP {0}: file not found: {1}" -f $r.out, $r.data) -ForegroundColor Yellow
        $skipped += $r.out
        continue
    }
    Write-Host ''
    Write-Host ("[run] === {0}  ->  {1} ===" -f $r.data, $r.out) -ForegroundColor Cyan
    $log = Join-Path $root ($r.out + '.log')
    $cargoArgs = @('run', '--profile', 'wfa', '--bin', 'wfa_momentum', '--', '--data', $r.data, '--out', $r.out) + $r.extra
    & cargo @cargoArgs 2>&1 | ForEach-Object { $_.ToString() } | Tee-Object -FilePath $log
    if ($LASTEXITCODE -ne 0) {
        Write-Host ("[run] FAILED: {0} (log: {1})" -f $r.out, $log) -ForegroundColor Red
        $failed += $r.out
    } else {
        Write-Host ("[run] ok: {0}" -f $r.out) -ForegroundColor Green
    }
}

Write-Host ''
Write-Host '=== P32 WFA summary ==='
if ($skipped.Count -gt 0) { Write-Host ("skipped: " + ($skipped -join ', ')) -ForegroundColor Yellow }
if ($failed.Count -gt 0) {
    Write-Host ("failed: " + ($failed -join ', ')) -ForegroundColor Red
    Write-Host 'If it is STATUS_ACCESS_VIOLATION again - see README_P32.md, section Plan B.'
} else {
    Write-Host 'all runs finished.' -ForegroundColor Green
    Write-Host 'Compare run_*/summary.json with the reference table in README_P32.md (numbers must match 1:1).'
}
