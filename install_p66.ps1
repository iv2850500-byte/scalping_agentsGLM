# ============================================================
# P66 installer - threshold tuning: env-ized funnel thresholds
# (MOM_ATR_MIN_PCT / MOM_OFI_MIN / MOM_VOLUME_MIN) + grid search
# (threshold_grid bin) + repeat-WFA workflow.
# ============================================================
# REQUIRES: P64 installed (momentum.rs with P64 markers, Cargo.toml with
# the wfa_pipeline bin entry). P65 (E5c) is NOT required: P66 does not
# touch main.rs / app_error.rs - install order P65 before/after P66 both OK.
#
# What it does:
#   NEW    src\bin\threshold_grid.rs  grid search over volume/atr/ofi
#          thresholds through the LIVE MomentumStrategy (lib), feature-
#          gated (required-features = ["wfa"], NOT in the release build)
#   PATCH  src\bin\wfa_pipeline.rs    build_strategy() now reads MOM_*
#          env thresholds (defaults = live config defaults 0.05/0.15/1.5,
#          so WITHOUT env the funnel layout is identical to P64) + the
#          report prints the thresholds used
#   NEW    wfa_grid.ps1               runner (data + build + grid run)
#   EDIT   Cargo.toml                 + [[bin]] threshold_grid entry
#
# Live-bot tuning itself is ALREADY config-driven (strategies\mod.rs reads
# strategy.params from config.toml): volume_anomaly_threshold /
# min_atr_threshold / ofi_threshold. No momentum.rs changes in P66 =>
# zero risk to the live path; cargo test --lib count is UNCHANGED.
#
# Expected after install: 22 'P66/' markers (21 in src\**\*.rs + 1 in
# Cargo.toml).
# ============================================================
$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
Set-Location $root
Write-Host "[P66] root = $root"

$patched = Join-Path $root "patched"
if (-not (Test-Path $patched)) { Write-Host "[P66] ERROR: patched\ not found (unpack zip fully)"; exit 1 }

function Count-Literal([string]$text, [string]$needle) {
    return ([regex]::Matches($text, [regex]::Escape($needle))).Count
}

# ---------- idempotency ----------
$pipePath = Join-Path $root "src\bin\wfa_pipeline.rs"
if (Test-Path $pipePath) {
    if ((Count-Literal (Get-Content $pipePath -Raw) "P66/") -gt 0) {
        Write-Host "[P66] already installed - skip (nothing to do)"
        Write-Host "[P66] verify: powershell -ExecutionPolicy Bypass -File .\check_packs.ps1"
        exit 0
    }
} else {
    Write-Host "[P66] ERROR: src\bin\wfa_pipeline.rs not found - P64 is NOT installed; install P64 first"; exit 1
}

# ---------- pre-flight sentinels (fail-fast, no partial installs) ----------
$momPath = Join-Path $root "src\strategies\momentum.rs"
if (-not (Test-Path $momPath)) { Write-Host "[P66] ERROR: src\strategies\momentum.rs not found"; exit 1 }
if ((Count-Literal (Get-Content $momPath -Raw) "P64/") -lt 1) {
    Write-Host "[P66] ERROR: momentum.rs has no P64 markers - P64 is NOT installed; install P64 first"; exit 1
}

$cargoPath = Join-Path $root "Cargo.toml"
$cg = Get-Content $cargoPath -Raw
if ((Count-Literal $cg 'name = "wfa_pipeline"') -ne 1) {
    Write-Host "[P66] ERROR: Cargo.toml drifted (wfa_pipeline bin entry not found x1) - P64 Cargo part missing; install P64 first"; exit 1
}
if ((Count-Literal $cg 'name = "threshold_grid"') -ne 0) {
    Write-Host "[P66] ERROR: Cargo.toml already has threshold_grid - partial install"; exit 1
}

$pn = Get-Content $pipePath -Raw
if ((Count-Literal $pn "fn build_strategy") -ne 1) {
    Write-Host "[P66] ERROR: wfa_pipeline.rs drifted (fn build_strategy not found x1) - refusing to patch; send the file to the maintainer"; exit 1
}
if ((Count-Literal $pn "P64/") -lt 3) {
    Write-Host ("[P66] ERROR: wfa_pipeline.rs drifted (P64 markers x{0}, expected >= 3) - refusing to patch" -f (Count-Literal $pn "P64/")); exit 1
}

# ---------- backups ----------
$date = Get-Date -Format "yyyy-MM-dd"
function Backup-File([string]$rel) {
    $src = Join-Path $root $rel
    if (-not (Test-Path $src)) { return }
    $dst = Join-Path $root ("backups\" + $date + "\" + $rel + ".bak-P66")
    $dstDir = Split-Path $dst -Parent
    New-Item -ItemType Directory -Force -Path $dstDir | Out-Null
    Copy-Item -Path $src -Destination $dst -Force
    Write-Host ("[P66] backup: {0} -> {1}" -f $rel, $dst)
}
Backup-File "src\bin\wfa_pipeline.rs"

# ---------- copy patched files ----------
$copies = @(
    @{ s = "patched\src\bin\threshold_grid.rs"; d = "src\bin\threshold_grid.rs" },
    @{ s = "patched\src\bin\wfa_pipeline.rs";   d = "src\bin\wfa_pipeline.rs" },
    @{ s = "patched\wfa_grid.ps1";              d = "wfa_grid.ps1" }
)
foreach ($c in $copies) {
    $src = Join-Path $root $c.s
    if (-not (Test-Path $src)) { Write-Host ("[P66] ERROR: missing in pack: {0}" -f $c.s); exit 1 }
    $dst = Join-Path $root $c.d
    Copy-Item -Path $src -Destination $dst -Force
    Write-Host ("[P66] {0} : copied" -f $c.d)
}

# ---------- Cargo.toml: + [[bin]] threshold_grid (idempotent) ----------
$binEntry = @"

# P66/: threshold grid (volume/atr/ofi) through the LIVE MomentumStrategy;
# excluded from release build (required-features), like wfa_pipeline (P44/rel).
[[bin]]
name = "threshold_grid"
path = "src/bin/threshold_grid.rs"
required-features = ["wfa"]
"@
if ((Count-Literal $cg 'name = "threshold_grid"') -eq 0) {
    [System.IO.File]::WriteAllText($cargoPath, $cg.TrimEnd() + $binEntry + "`r`n", (New-Object System.Text.UTF8Encoding($false)))
    Write-Host "[P66] Cargo.toml : + [[bin]] threshold_grid (required-features = wfa)"
}

# ---------- marker count ----------
$p66Total = 0
Get-ChildItem -Path (Join-Path $root "src") -Recurse -Filter "*.rs" | ForEach-Object {
    $p66Total += Count-Literal (Get-Content $_.FullName -Raw) "P66/"
}
$p66Total += Count-Literal (Get-Content $cargoPath -Raw) "P66/"
Write-Host ("[P66] markers: {0} (expected 22)" -f $p66Total)
if ($p66Total -lt 22) {
    Write-Host "[P66] ERROR: marker count below expected - install incomplete"; exit 1
}

# ---------- check_packs.ps1: append the P66 tail (idempotent) ----------
$tailPath = Join-Path $root "check_packs_v14_tail.txt"
$checkPath = Join-Path $root "check_packs.ps1"
if ((Test-Path $tailPath) -and (Test-Path $checkPath)) {
    $ck = Get-Content $checkPath -Raw
    if ($ck -notmatch [regex]::Escape("P66 block")) {
        $tail = [System.IO.File]::ReadAllText($tailPath)
        [System.IO.File]::WriteAllText($checkPath, $ck.TrimEnd() + "`r`n" + $tail, (New-Object System.Text.UTF8Encoding($false)))
        Write-Host "[P66] check_packs.ps1 : + P66 block (v14 tail)"
    } else {
        Write-Host "[P66] check_packs.ps1 : P66 block already present - skip"
    }
} else {
    Write-Host "[P66] check_packs.ps1/tail not found - skip tail append (check markers manually)"
}

Write-Host ""
Write-Host "[P66] workflow (grid -> validate -> config.toml):"
Write-Host "[P66]   1) powershell -ExecutionPolicy Bypass -File .\wfa_grid.ps1 -Days 450"
Write-Host "[P66]   2) take [ok] winner from wfa_out\grid_report.txt, then repeat WFA:"
Write-Host '[P66]      $env:MOM_VOLUME_MIN="..."; $env:MOM_ATR_MIN_PCT="..."; $env:MOM_OFI_MIN="..."'
Write-Host "[P66]      powershell -ExecutionPolicy Bypass -File .\wfa_pipeline.ps1 -Days 450"
Write-Host "[P66]   3) only after VERDICT PASS edit config.toml strategy.params:"
Write-Host "[P66]      volume_anomaly_threshold / min_atr_threshold / ofi_threshold"
Write-Host ""
Write-Host "Next: powershell -ExecutionPolicy Bypass -File .\build.ps1   # test count UNCHANGED by P66"
Write-Host "[P66] RESULT: OK"
