# ============================================================
# P67 fix-installer - deterministic repair of the P64/P65/P66
# state (NO new markers: the target state is exactly
# P64=51 | P65=6 | P66=22 | cargo test --lib = 80).
# ============================================================
# Symptom (repo before this fix):
#   markers P64=50 (exp 51) | P65=0 (exp 6) | P66=14 (exp 22)
#   tests = 78 (exp 80); bot itself RUNS fine (live path intact).
# Root cause: mixed drift in files that are NOT compiled by
#   build.ps1 (wfa bins are required-features = ["wfa"]) plus
#   P65 never applied (its pre-flight needs the P64 wrapper in
#   main.rs):
#   1) src\bin\wfa_pipeline.rs  = old P64 version (0 P66 markers);
#      threshold_grid.rs (13 P66) + Cargo [[bin]] threshold_grid
#      ARE present -> P66 = 13 + 1 = 14.
#   2) P65 (E5c: exit-code migration) not installed -> P65 = 0,
#      tests stay 78 (2 t65_* missing).
#   3) P64 = 50 instead of 51 -> one P64 marker lost: either
#      main.rs rolled back to pre-P64 (e.g. restored from
#      backups\...\main.rs.bak-P64 while fighting P65 pre-flight)
#      or the Cargo.toml thiserror line lost its '# P64/' comment.
#      This installer repairs BOTH variants.
#
# What it does (wholesale copies, like P64/P65/P66):
#   REPLACE src\main.rs              P65 version (ExitCode wrapper +
#                                    classify + config/allow_live ->
#                                    AppError::Config -> exit 1)
#   REPLACE src\utils\app_error.rs   P65 version (+ classify + 2 t65_*,
#                                    keeps all 5 t64_* tests)
#   REPLACE src\bin\wfa_pipeline.rs  P66 version (MOM_* env thresholds,
#                                    keeps all 15 P64 markers)
#   REPLACE src\bin\threshold_grid.rs P66 version (idempotent re-copy)
#   REPLACE wfa_grid.ps1             P66 version (idempotent re-copy)
#   NORMALIZE Cargo.toml             canonical thiserror line + both
#                                    [[bin]] blocks with their P64/P66
#                                    comment lines (idempotent)
#   UNTOUCHED: momentum.rs, utils\mod.rs, live-path files, config.
#
# Expected after install:
#   P64 = 51 | P65 = 6 | P66 = 22  (check_packs.ps1 tails v12/v13/v14
#   already expect exactly these numbers - no tail updates needed)
#   cargo test --lib = 80 (73 base + 5 t64_* + 2 t65_*)
#
# If you made MANUAL edits to main.rs they are preserved in
#   backups\<date>\src\main.rs.bak-P67 - diff before discarding.
# ============================================================
$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
Set-Location $root
Write-Host "[P67] root = $root"

$patched = Join-Path $root "patched"
if (-not (Test-Path $patched)) { Write-Host "[P67] ERROR: patched\ not found (unpack the pack into the repo root)"; exit 1 }

function Count-Literal([string]$text, [string]$needle) {
    return ([regex]::Matches($text, [regex]::Escape($needle))).Count
}

$mainPath    = Join-Path $root "src\main.rs"
$appErrPath  = Join-Path $root "src\utils\app_error.rs"
$modPath     = Join-Path $root "src\utils\mod.rs"
$momPath     = Join-Path $root "src\strategies\momentum.rs"
$pipePath    = Join-Path $root "src\bin\wfa_pipeline.rs"
$gridPath    = Join-Path $root "src\bin\threshold_grid.rs"
$gridPs1Path = Join-Path $root "wfa_grid.ps1"
$cargoPath   = Join-Path $root "Cargo.toml"

# ---------- idempotency ----------
if ((Test-Path $mainPath) -and (Test-Path $pipePath)) {
    $mn0 = Get-Content $mainPath -Raw
    $pn0 = Get-Content $pipePath -Raw
    if ((Count-Literal $mn0 "P65/") -ge 3 -and (Count-Literal $pn0 "P66/") -ge 8) {
        Write-Host "[P67] target state already reached (main.rs has P65, wfa_pipeline.rs has P66) - skip"
        Write-Host "[P67] verify: powershell -ExecutionPolicy Bypass -File .\check_packs.ps1"
        exit 0
    }
}

# ---------- pre-flight sentinels (fail-fast, no partial installs) ----------
foreach ($f in @($momPath, $appErrPath, $modPath, $pipePath, $cargoPath)) {
    if (-not (Test-Path $f)) { Write-Host ("[P67] ERROR: not found: {0} - P64 is NOT installed; install P64 first" -f $f); exit 1 }
}
if ((Count-Literal (Get-Content $momPath -Raw) "P64/") -lt 1) { Write-Host "[P67] ERROR: momentum.rs has no P64 markers - P64 is NOT installed; install P64 first"; exit 1 }
if ((Count-Literal (Get-Content $appErrPath -Raw) "P64/") -lt 1) { Write-Host "[P67] ERROR: app_error.rs has no P64 markers - P64 is NOT installed; install P64 first"; exit 1 }
if ((Count-Literal (Get-Content $modPath -Raw) "app_error") -lt 1) { Write-Host "[P67] ERROR: utils\mod.rs does not declare app_error - P64 is NOT installed; install P64 first"; exit 1 }

# ---------- backups ----------
$date = Get-Date -Format "yyyy-MM-dd"
function Backup-File([string]$rel) {
    $src = Join-Path $root $rel
    if (-not (Test-Path $src)) { return }
    $dst = Join-Path $root ("backups\" + $date + "\" + $rel + ".bak-P67")
    $dstDir = Split-Path $dst -Parent
    New-Item -ItemType Directory -Force -Path $dstDir | Out-Null
    Copy-Item -Path $src -Destination $dst -Force
    Write-Host ("[P67] backup: {0} -> {1}" -f $rel, $dst)
}
Backup-File "src\main.rs"
Backup-File "src\utils\app_error.rs"
Backup-File "src\bin\wfa_pipeline.rs"
Backup-File "src\bin\threshold_grid.rs"
Backup-File "wfa_grid.ps1"
Backup-File "Cargo.toml"

# ---------- copy patched files (wholesale) ----------
$copies = @(
    @{ s = "patched\src\main.rs";               d = "src\main.rs" },
    @{ s = "patched\src\utils\app_error.rs";    d = "src\utils\app_error.rs" },
    @{ s = "patched\src\bin\wfa_pipeline.rs";   d = "src\bin\wfa_pipeline.rs" },
    @{ s = "patched\src\bin\threshold_grid.rs"; d = "src\bin\threshold_grid.rs" },
    @{ s = "patched\wfa_grid.ps1";              d = "wfa_grid.ps1" }
)
foreach ($c in $copies) {
    $src = Join-Path $root $c.s
    if (-not (Test-Path $src)) { Write-Host ("[P67] ERROR: missing in pack: {0}" -f $c.s); exit 1 }
    $dst = Join-Path $root $c.d
    $dstDir = Split-Path $dst -Parent
    New-Item -ItemType Directory -Force -Path $dstDir | Out-Null
    Copy-Item -Path $src -Destination $dst -Force
    Write-Host ("[P67] {0} : copied" -f $c.d)
}

# ---------- Cargo.toml normalization (idempotent) ----------
$nl = "`r`n"
$ct = Get-Content $cargoPath -Raw

# 1) thiserror: add if missing; normalize the line so it carries the P64 comment
if ($ct -notmatch "(?m)^thiserror\s*=") {
    if ($ct -notmatch "(?m)^anyhow\s*=") { Write-Host "[P67] ERROR: Cargo.toml drifted (no anyhow line) - refusing to patch"; exit 1 }
    $ct = $ct -creplace "(?m)^(anyhow\s*=[^\r\n]*)$", ('$1' + $nl + 'thiserror = "2" # P64/: typed errors (E5b)')
    Write-Host "[P67] Cargo.toml : + thiserror = 2 (with P64 comment)"
} elseif ($ct -notmatch [regex]::Escape('thiserror = "2" # P64/')) {
    $ct = $ct -creplace "(?m)^thiserror[^\r\n]*\r?$", 'thiserror = "2" # P64/: typed errors (E5b)'
    Write-Host "[P67] Cargo.toml : thiserror line normalized (+ P64 comment)"
}

# 2) [[bin]] wfa_pipeline: add block if missing; restore P64 comment if block exists bare
$wfaBlock = '# P64/: WFA conveyor through the LIVE MomentumStrategy (lib) +' + $nl +
            '# 17-filter funnel; excluded from release build (required-features),' + $nl +
            '# like the other wfa bins (P44/rel).' + $nl +
            '[[bin]]' + $nl +
            'name = "wfa_pipeline"' + $nl +
            'path = "src/bin/wfa_pipeline.rs"' + $nl +
            'required-features = ["wfa"]'
if ($ct -notmatch [regex]::Escape('name = "wfa_pipeline"')) {
    $ct = $ct.TrimEnd() + $nl + $nl + $wfaBlock + $nl
    Write-Host "[P67] Cargo.toml : + [[bin]] wfa_pipeline (with P64 comment)"
} elseif ($ct -notmatch [regex]::Escape('# P64/: WFA conveyor')) {
    $ct = $ct -creplace '(?s)\[\[bin\]\]\s*name\s*=\s*"wfa_pipeline".*?required-features\s*=\s*\[\s*"wfa"\s*\]', $wfaBlock
    Write-Host "[P67] Cargo.toml : [[bin]] wfa_pipeline block normalized (+ P64 comment)"
}

# 3) [[bin]] threshold_grid: add block if missing; restore P66 comment if block exists bare
$gridBlock = '# P66/: threshold grid (volume/atr/ofi) through the LIVE MomentumStrategy;' + $nl +
             '# excluded from release build (required-features), like wfa_pipeline (P44/rel).' + $nl +
             '[[bin]]' + $nl +
             'name = "threshold_grid"' + $nl +
             'path = "src/bin/threshold_grid.rs"' + $nl +
             'required-features = ["wfa"]'
if ($ct -notmatch [regex]::Escape('name = "threshold_grid"')) {
    $ct = $ct.TrimEnd() + $nl + $nl + $gridBlock + $nl
    Write-Host "[P67] Cargo.toml : + [[bin]] threshold_grid (with P66 comment)"
} elseif ($ct -notmatch [regex]::Escape('# P66/: threshold grid')) {
    $ct = $ct -creplace '(?s)\[\[bin\]\]\s*name\s*=\s*"threshold_grid".*?required-features\s*=\s*\[\s*"wfa"\s*\]', $gridBlock
    Write-Host "[P67] Cargo.toml : [[bin]] threshold_grid block normalized (+ P66 comment)"
}

[System.IO.File]::WriteAllText($cargoPath, $ct, (New-Object System.Text.UTF8Encoding($false)))

# ---------- final marker counts (target state) ----------
$p64 = 0; $p65 = 0; $p66 = 0
Get-ChildItem -Path (Join-Path $root "src") -Recurse -Filter "*.rs" | ForEach-Object {
    $s = Get-Content $_.FullName -Raw
    $p64 += Count-Literal $s "P64/"
    $p65 += Count-Literal $s "P65/"
    $p66 += Count-Literal $s "P66/"
}
$ct2 = Get-Content $cargoPath -Raw
$p64 += Count-Literal $ct2 "P64/"
$p66 += Count-Literal $ct2 "P66/"
Write-Host ("[P67] markers: P64={0} (exp 51) | P65={1} (exp 6) | P66={2} (exp 22)" -f $p64, $p65, $p66)
if ($p64 -lt 51 -or $p65 -lt 6 -or $p66 -lt 22) {
    Write-Host "[P67] ERROR: marker count below target - install incomplete"; exit 1
}

Write-Host ""
Write-Host "[P67] state repaired. check_packs.ps1 tails (v12/v13/v14) already expect 51/6/22 - unchanged."
Write-Host "[P67] exit-code contract now ACTIVE (P65): 0=clean | 1=FATAL config/keys (supervisor does NOT restart) | 2=transient (restart) | 3=stall | 101=panic"
Write-Host "[P67] test it: break config.toml on purpose, run target\release\scalping_agents.exe, echo `$LASTEXITCODE   # expect 1"
Write-Host ""
Write-Host "[P67] OPTIONAL (research tooling only, NOT needed for the live bot):"
Write-Host "[P67]   cargo check --features wfa   # validates wfa_pipeline + threshold_grid compile"
Write-Host ""
Write-Host "Next: powershell -ExecutionPolicy Bypass -File .\build.ps1   # stop -> test (expect 80) -> build -> deploy"
Write-Host "[P67] RESULT: OK"
