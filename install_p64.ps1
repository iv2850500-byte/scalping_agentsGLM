# ============================================================
# P64 installer - WFA conveyor (17 momentum filters, feature-gated)
#                + supervisor-restarts + E5b typed errors
# ============================================================
# Contents:
#   NEW  src\bin\wfa_pipeline.rs      WFA conveyor through the LIVE
#                                     MomentumStrategy (lib) + 17-filter
#                                     funnel, required-features = wfa
#   NEW  src\utils\app_error.rs       P64/E5b typed errors (thiserror)
#                                     + canonical exit codes
#   NEW  supervisor.ps1               process-level supervisor (restarts,
#                                     stall watchdog, budget, TG alerts)
#   NEW  wfa_pipeline.ps1             one-command WFA runner
#   PATCH src\strategies\momentum.rs  replay clock (WFA) + wfa_funnel()
#                                     snapshot (feature-gated, no-op in
#                                     release)
#   PATCH src\main.rs                 process boundary: main -> ExitCode
#                                     via AppError (E5b)
#   PATCH src\utils\mod.rs            + pub mod app_error
#   PATCH Cargo.toml                  + thiserror = "2", + [[bin]]
#                                     wfa_pipeline (required-features)
#
# Expected after install: 42 'P64/' markers in src\**\*.rs + Cargo.toml.
# cargo test --lib must show 78 tests (73 before + 5 new t64_*).
# ============================================================
$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
Set-Location $root
Write-Host "[P64] root = $root"

$patched = Join-Path $root "patched"
if (-not (Test-Path $patched)) { Write-Host "[P64] ERROR: patched\ not found (unpack zip fully)"; exit 1 }

function Count-Markers([string]$text) {
    return ([regex]::Matches($text, [regex]::Escape("P64/"))).Count
}
function Count-Literal([string]$text, [string]$needle) {
    return ([regex]::Matches($text, [regex]::Escape($needle))).Count
}

# ---------- idempotency ----------
$appErrPath = Join-Path $root "src\utils\app_error.rs"
if ((Test-Path $appErrPath) -and ((Get-Content $appErrPath -Raw) -match "P64")) {
    Write-Host "[P64] already installed - skip (nothing to do)"
    Write-Host "[P64] verify: powershell -ExecutionPolicy Bypass -File .\check_packs.ps1"
    exit 0
}

# ---------- pre-flight sentinels (fail-fast, no partial installs) ----------
$momentumPath = Join-Path $root "src\strategies\momentum.rs"
$mainPath = Join-Path $root "src\main.rs"
$utilsModPath = Join-Path $root "src\utils\mod.rs"
$cargoPath = Join-Path $root "Cargo.toml"

$pre = @(
    @{ p = $momentumPath;  n = "momentum.rs" },
    @{ p = $mainPath;      n = "main.rs" },
    @{ p = $utilsModPath;  n = "utils\mod.rs" },
    @{ p = $cargoPath;     n = "Cargo.toml" }
)
foreach ($f in $pre) {
    if (-not (Test-Path $f.p)) { Write-Host ("[P64] ERROR: {0} not found - wrong root?" -f $f.n); exit 1 }
}

$mom = Get-Content $momentumPath -Raw
if (Count-Literal $mom "P64/" -gt 0) { Write-Host "[P64] ERROR: momentum.rs already contains P64 markers - partial install; restore from backups and retry"; exit 1 }
if (Count-Literal $mom "blocked_by_supertrend: u64" -lt 1) { Write-Host "[P64] ERROR: momentum.rs drifted (no 'blocked_by_supertrend: u64') - refusing to overwrite; send the file to the maintainer"; exit 1 }
$chronoCount = Count-Literal $mom "chrono::Utc::now"
if ($chronoCount -ne 2) { Write-Host ("[P64] ERROR: momentum.rs drifted (chrono::Utc::now x{0}, expected 2) - refusing to overwrite" -f $chronoCount); exit 1 }

$mn = Get-Content $mainPath -Raw
if (Count-Literal $mn "P64/" -gt 0) { Write-Host "[P64] ERROR: main.rs already contains P64 markers - partial install"; exit 1 }
$mnMainCount = Count-Literal $mn "async fn main() -> Result<()> {"
if ($mnMainCount -ne 1) { Write-Host ("[P64] ERROR: main.rs drifted (async fn main found x{0}, expected 1) - refusing to overwrite" -f $mnMainCount); exit 1 }
if (Count-Literal $mn "#[tokio::main]" -lt 1) { Write-Host "[P64] ERROR: main.rs drifted (no #[tokio::main]) - refusing to overwrite"; exit 1 }

$um = Get-Content $utilsModPath -Raw
if ($um -match "app_error") { Write-Host "[P64] ERROR: utils\mod.rs already references app_error - partial install"; exit 1 }

$ct = Get-Content $cargoPath -Raw
if (-not ($ct -match "(?m)^anyhow\s*=")) { Write-Host "[P64] ERROR: Cargo.toml drifted (no anyhow line) - refusing to patch"; exit 1 }

# ---------- backups ----------
$date = Get-Date -Format "yyyy-MM-dd"
function Backup-File([string]$rel) {
    $src = Join-Path $root $rel
    if (-not (Test-Path $src)) { return }
    $dst = Join-Path $root ("backups\" + $date + "\" + $rel + ".bak-P64")
    $dstDir = Split-Path $dst -Parent
    New-Item -ItemType Directory -Force -Path $dstDir | Out-Null
    Copy-Item -Path $src -Destination $dst -Force
    Write-Host ("[P64] backup: {0} -> {1}" -f $rel, $dst)
}
Backup-File "src\strategies\momentum.rs"
Backup-File "src\main.rs"
Backup-File "src\utils\mod.rs"
Backup-File "Cargo.toml"

# ---------- copy new files ----------
$copies = @(
    @{ s = "patched\src\bin\wfa_pipeline.rs";      d = "src\bin\wfa_pipeline.rs" },
    @{ s = "patched\src\utils\app_error.rs";       d = "src\utils\app_error.rs" },
    @{ s = "patched\src\strategies\momentum.rs";   d = "src\strategies\momentum.rs" },
    @{ s = "patched\src\main.rs";                  d = "src\main.rs" },
    @{ s = "patched\src\utils\mod.rs";             d = "src\utils\mod.rs" },
    @{ s = "patched\supervisor.ps1";               d = "supervisor.ps1" },
    @{ s = "patched\wfa_pipeline.ps1";             d = "wfa_pipeline.ps1" }
)
$copied = 0
foreach ($c in $copies) {
    $src = Join-Path $root $c.s
    if (-not (Test-Path $src)) { Write-Host ("[P64] ERROR: missing in pack: {0}" -f $c.s); exit 1 }
    $dst = Join-Path $root $c.d
    $dstDir = Split-Path $dst -Parent
    New-Item -ItemType Directory -Force -Path $dstDir | Out-Null
    Copy-Item -Path $src -Destination $dst -Force
    $copied++
    Write-Host ("[P64] {0} : copied" -f $c.d)
}

# ---------- patch Cargo.toml ----------
$ct = Get-Content $cargoPath -Raw
if ($ct -notmatch [regex]::Escape("thiserror")) {
    $ct = $ct -creplace "(?m)^(anyhow\s*=[^\r\n]*)$", ('$1' + "`r`n" + 'thiserror = "2" # P64/: typed errors (E5b)')
    Write-Host "[P64] Cargo.toml : + thiserror = 2"
} else {
    Write-Host "[P64] Cargo.toml : thiserror already present - skip"
}
if ($ct -notmatch [regex]::Escape("wfa_pipeline")) {
    $ct = $ct.TrimEnd() + "`r`n`r`n# P64/: WFA conveyor through the LIVE MomentumStrategy (lib) +`r`n# 17-filter funnel; excluded from release build (required-features),`r`n# like the other wfa bins (P44/rel).`r`n[[bin]]`r`nname = `"wfa_pipeline`"`r`npath = `"src/bin/wfa_pipeline.rs`"`r`nrequired-features = [`"wfa`"]`r`n"
    Write-Host "[P64] Cargo.toml : + [[bin]] wfa_pipeline (required-features = wfa)"
} else {
    Write-Host "[P64] Cargo.toml : wfa_pipeline bin already present - skip"
}
# Cargo.toml: keep UTF-8 without BOM
[System.IO.File]::WriteAllText($cargoPath, $ct, (New-Object System.Text.UTF8Encoding($false)))

# ---------- marker count ----------
$total = 0
Get-ChildItem -Path (Join-Path $root "src") -Recurse -Filter "*.rs" | ForEach-Object {
    $total += Count-Markers (Get-Content $_.FullName -Raw)
}
$total += Count-Markers (Get-Content $cargoPath -Raw)
Write-Host ("[P64] markers: {0} (expected 51)" -f $total)
if ($total -lt 51) {
    Write-Host "[P64] ERROR: marker count below expected - install incomplete"; exit 1
}

# ---------- check_packs.ps1: append the P64 tail (idempotent) ----------
$tailPath = Join-Path $root "check_packs_v12_tail.txt"
$checkPath = Join-Path $root "check_packs.ps1"
if ((Test-Path $tailPath) -and (Test-Path $checkPath)) {
    $ck = Get-Content $checkPath -Raw
    if ($ck -notmatch [regex]::Escape("P64 block")) {
        $tail = [System.IO.File]::ReadAllText($tailPath)
        [System.IO.File]::WriteAllText($checkPath, $ck.TrimEnd() + "`r`n" + $tail, (New-Object System.Text.UTF8Encoding($false)))
        Write-Host "[P64] check_packs.ps1 : + P64 block (v12 tail)"
    } else {
        Write-Host "[P64] check_packs.ps1 : P64 block already present - skip"
    }
} else {
    Write-Host "[P64] check_packs.ps1/tail not found - skip tail append (check markers manually)"
}

Write-Host ""
Write-Host "[P64] .env hints (print-only, edit .env manually):"
Write-Host "[P64]   HEARTBEAT_SEC=60                  # bot heartbeat; supervisor staleness = 3x+60s"
Write-Host "[P64]   TG_BOT_TOKEN=... TG_CHAT_ID=...   # supervisor alerts (best-effort)"
Write-Host "[P64]   TG_PROXY=http://127.0.0.1:10809   # http proxy for TG alerts from RF (socks5 NOT supported here)"
Write-Host ""
Write-Host "[P64] supervisor (process-level restarts; do NOT run together with deploy.ps1):"
Write-Host "[P64]   powershell -ExecutionPolicy Bypass -File .\supervisor.ps1"
Write-Host "[P64]   before build/deploy:  New-Item -ItemType File PAUSE"
Write-Host "[P64]   after build/deploy:   Remove-Item PAUSE"
Write-Host "[P64]   stop supervisor:      powershell -ExecutionPolicy Bypass -File .\supervisor.ps1 -StopSupervisor"
Write-Host ""
Write-Host "[P64] WFA conveyor (research, feature-gated):"
Write-Host "[P64]   powershell -ExecutionPolicy Bypass -File .\wfa_pipeline.ps1             # TAOUSDT, data from klines\"
Write-Host "[P64]   powershell -ExecutionPolicy Bypass -File .\wfa_pipeline.ps1 -Days 450   # fresh Bybit data"
Write-Host "[P64]   cargo run --features wfa --bin wfa_pipeline -- --data TAOUSDT-1h.csv --test 720 --step 720"
Write-Host ""
Write-Host "Next: powershell -ExecutionPolicy Bypass -File .\build.ps1   # stop -> test (expect 78) -> build -> deploy"
Write-Host "[P64] RESULT: OK"
