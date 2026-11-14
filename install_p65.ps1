# ============================================================
# P65 installer - E5c: migrate critical main.rs call-sites to AppError
# ============================================================
# REQUIRES: P64 installed (utils\app_error.rs with P64 markers + main.rs
# with the P64 ExitCode wrapper). This pack makes exit codes MEANINGFUL:
#
#   BEFORE (P64): main wrapper maps EVERY anyhow error -> AppError::Other
#                 -> exit 2 -> supervisor restarts even on a broken config.
#   AFTER  (P65): config load + allow_live barrier return AppError::Config
#                 -> exit 1 -> supervisor does NOT restart (fatal);
#                 wrapper downcasts migrated AppError via classify().
#
# Contents:
#   PATCH src\main.rs            3 call-sites: wrapper classify(),
#                                config load -> Config, allow_live -> Config
#   PATCH src\utils\app_error.rs + classify() + 2 tests (t65_*)
#
# Expected after install: 6 'P65/' markers in src\**\*.rs.
# cargo test --lib must show 80 tests (78 after P64 + 2 new t65_*).
# ============================================================
$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
Set-Location $root
Write-Host "[P65] root = $root"

$patched = Join-Path $root "patched"
if (-not (Test-Path $patched)) { Write-Host "[P65] ERROR: patched\ not found (unpack zip fully)"; exit 1 }

function Count-Literal([string]$text, [string]$needle) {
    return ([regex]::Matches($text, [regex]::Escape($needle))).Count
}

# ---------- idempotency ----------
$mainPath = Join-Path $root "src\main.rs"
$mn = Get-Content $mainPath -Raw
if (Count-Literal $mn "P65/" -gt 0) {
    Write-Host "[P65] already installed - skip (nothing to do)"
    Write-Host "[P65] verify: powershell -ExecutionPolicy Bypass -File .\check_packs.ps1"
    exit 0
}

# ---------- pre-flight sentinels (fail-fast, no partial installs) ----------
$appErrPath = Join-Path $root "src\utils\app_error.rs"
if (-not (Test-Path $appErrPath)) { Write-Host "[P65] ERROR: src\utils\app_error.rs not found - P64 is NOT installed; install P64 first"; exit 1 }
$ae = Get-Content $appErrPath -Raw
if (Count-Literal $ae "P64/" -lt 1) { Write-Host "[P65] ERROR: app_error.rs has no P64 markers - P64 is NOT installed; install P64 first"; exit 1 }
if (Count-Literal $ae "P65/" -gt 0) { Write-Host "[P65] ERROR: app_error.rs already contains P65 markers - partial install"; exit 1 }
if (Count-Literal $ae "pub fn classify" -ne 0) { Write-Host "[P65] ERROR: app_error.rs already has classify - partial install"; exit 1 }

$momPath = Join-Path $root "src\strategies\momentum.rs"
if ((Test-Path $momPath) -and ((Count-Literal (Get-Content $momPath -Raw) "P64/") -lt 1)) {
    Write-Host "[P65] ERROR: momentum.rs has no P64 markers - P64 is NOT installed; install P64 first"; exit 1
}

if (Count-Literal $mn "let app_err: utils::app_error::AppError = e.into();" -ne 1) {
    Write-Host "[P65] ERROR: main.rs drifted (P64 wrapper signature not found x1) - refusing to patch; send main.rs to the maintainer"; exit 1
}
if (Count-Literal $mn "async fn main() -> std::process::ExitCode {" -ne 1) {
    Write-Host "[P65] ERROR: main.rs drifted (P64 ExitCode wrapper not found) - refusing to patch"; exit 1
}
if (Count-Literal $mn 'let config = config::AppConfig::load_from("config.toml")?;' -ne 1) {
    Write-Host "[P65] ERROR: main.rs drifted (config load line not found x1) - refusing to patch"; exit 1
}
if (Count-Literal $mn "anyhow::bail!(" -ne 1) {
    Write-Host ("[P65] ERROR: main.rs drifted (anyhow::bail! x{0}, expected 1) - refusing to patch" -f (Count-Literal $mn "anyhow::bail!")); exit 1
}

# ---------- backups ----------
$date = Get-Date -Format "yyyy-MM-dd"
function Backup-File([string]$rel) {
    $src = Join-Path $root $rel
    if (-not (Test-Path $src)) { return }
    $dst = Join-Path $root ("backups\" + $date + "\" + $rel + ".bak-P65")
    $dstDir = Split-Path $dst -Parent
    New-Item -ItemType Directory -Force -Path $dstDir | Out-Null
    Copy-Item -Path $src -Destination $dst -Force
    Write-Host ("[P65] backup: {0} -> {1}" -f $rel, $dst)
}
Backup-File "src\main.rs"
Backup-File "src\utils\app_error.rs"

# ---------- copy patched files (wholesale, like P64) ----------
$copies = @(
    @{ s = "patched\src\main.rs";             d = "src\main.rs" },
    @{ s = "patched\src\utils\app_error.rs";  d = "src\utils\app_error.rs" }
)
foreach ($c in $copies) {
    $src = Join-Path $root $c.s
    if (-not (Test-Path $src)) { Write-Host ("[P65] ERROR: missing in pack: {0}" -f $c.s); exit 1 }
    $dst = Join-Path $root $c.d
    Copy-Item -Path $src -Destination $dst -Force
    Write-Host ("[P65] {0} : copied" -f $c.d)
}

# ---------- marker count ----------
$p65Total = 0
Get-ChildItem -Path (Join-Path $root "src") -Recurse -Filter "*.rs" | ForEach-Object {
    $p65Total += Count-Literal (Get-Content $_.FullName -Raw) "P65/"
}
Write-Host ("[P65] markers: {0} (expected 6)" -f $p65Total)
if ($p65Total -lt 6) {
    Write-Host "[P65] ERROR: marker count below expected - install incomplete"; exit 1
}
$p64Total = 0
Get-ChildItem -Path (Join-Path $root "src") -Recurse -Filter "*.rs" | ForEach-Object {
    $p64Total += Count-Literal (Get-Content $_.FullName -Raw) "P64/"
}
$p64Total += Count-Literal (Get-Content (Join-Path $root "Cargo.toml") -Raw) "P64/"
Write-Host ("[P64] markers still present: {0} (expected 50)" -f $p64Total)
if ($p64Total -lt 50) {
    Write-Host "[P65] ERROR: P64 markers lost - refuse to continue"; exit 1
}

# ---------- check_packs.ps1: append the P65 tail (idempotent) ----------
$tailPath = Join-Path $root "check_packs_v13_tail.txt"
$checkPath = Join-Path $root "check_packs.ps1"
if ((Test-Path $tailPath) -and (Test-Path $checkPath)) {
    $ck = Get-Content $checkPath -Raw
    if ($ck -notmatch [regex]::Escape("P65 block")) {
        $tail = [System.IO.File]::ReadAllText($tailPath)
        [System.IO.File]::WriteAllText($checkPath, $ck.TrimEnd() + "`r`n" + $tail, (New-Object System.Text.UTF8Encoding($false)))
        Write-Host "[P65] check_packs.ps1 : + P65 block (v13 tail)"
    } else {
        Write-Host "[P65] check_packs.ps1 : P65 block already present - skip"
    }
} else {
    Write-Host "[P65] check_packs.ps1/tail not found - skip tail append (check markers manually)"
}

Write-Host ""
Write-Host "[P65] exit-code contract (for supervisor.ps1 from P64):"
Write-Host "[P65]   0 = clean stop | 1 = FATAL config/keys (NO restart) | 2 = transient (restart) | 3 = stall | 101 = panic (restart)"
Write-Host "[P65] test it: break config.toml on purpose, run target\release\scalping_agents.exe, echo $LASTEXITCODE   # expect 1"
Write-Host ""
Write-Host "Next: powershell -ExecutionPolicy Bypass -File .\build.ps1   # stop -> test (expect 80) -> build -> deploy"
Write-Host "[P65] RESULT: OK"
