# =====================================================================
# install_p63.ps1 - P63: Telegram transport hardening (RF-blocking)
#
# Problem: api.telegram.org is blocked in RF -> every alert = 2 attempts,
#          WARN "retry in 3s" spam in bot.log, useless traffic.
# What it does (idempotent, safe to re-run):
#   1) Copies patched src\utils\telegram.rs (backup first):
#        - TG_PROXY (http:// | socks5:// | socks5h://) - route via local
#          proxy/tunnel (v2rayN 10808, Clash 7890 mixed). URL never logged;
#        - TG_ENABLED=false kill-switch (alerts off without deleting token);
#        - retry 3s -> 15s, max 3 attempts per message (was: fixed 3s spam);
#        - circuit breaker: 3 failed attempts in a row -> pause 10 min
#          (then 20/40/60, cap 60). During pause alerts are counted and sent
#          as ONE digest after recovery - no more WARN storm;
#        - 401/403/404 = bad token/chat -> alerts disabled until restart
#          (AUTH_DEAD), single log line.
#   2) Patches Cargo.toml: reqwest features += "socks" (tokio-socks crate;
#      first build needs crates.io access ONCE). Use -NoSocks to skip
#      (then http:// TG_PROXY still works, socks5:// is ignored at runtime).
#   3) Appends the v11 tail to check_packs.ps1.
#   4) Checks .env for TG_PROXY / TG_ENABLED presence (values never printed).
#
# Public API unchanged -> only telegram.rs + Cargo.toml touched.
# 8 new unit tests (p63_tests) -> cargo test --lib must show 73 passed.
#
# Usage:  powershell -ExecutionPolicy Bypass -File .\P63_pack\install_p63.ps1
#         powershell -ExecutionPolicy Bypass -File .\P63_pack\install_p63.ps1 -NoSocks
# After:  powershell -ExecutionPolicy Bypass -File .\build.ps1
# =====================================================================
param([switch]$NoSocks)
$ErrorActionPreference = 'Stop'

# --- repo root detection (works from pack folder or from repo root) ---
$here = $PSScriptRoot
if (Test-Path (Join-Path $here 'src')) {
    $root = $here
} elseif (Test-Path (Join-Path (Split-Path $here -Parent) 'src')) {
    $root = Split-Path $here -Parent
} else {
    Write-Host '[P63] ERROR: repo root not found (no src\ next to or above this pack folder)'
    exit 1
}
Write-Host ("[P63] root = {0}" -f $root)

$dateTag = Get-Date -Format 'yyyy-MM-dd'
$bakDir = Join-Path $root ("backups\" + $dateTag)
New-Item -ItemType Directory -Force -Path $bakDir | Out-Null

# --- 1) patched sources ---
$targets = @(
    'src\utils\telegram.rs'
)
$copied = 0
$skipped = 0
foreach ($rel in $targets) {
    $dst = Join-Path $root $rel
    $srcFile = Join-Path $here (Join-Path 'patched' $rel)
    if (-not (Test-Path $srcFile)) {
        Write-Host ("[P63] ERROR: pack file missing: {0} - the pack folder must stay intact" -f $rel)
        exit 1
    }
    if ((Test-Path $dst) -and ([System.IO.File]::ReadAllText($dst)).Contains('P63/')) {
        Write-Host ("[P63] {0} : already patched - skip" -f $rel)
        $skipped++
        continue
    }
    if (Test-Path $dst) {
        $bakPath = Join-Path $bakDir ($rel + '.bak-P63')
        New-Item -ItemType Directory -Force -Path (Split-Path $bakPath -Parent) | Out-Null
        Copy-Item $dst $bakPath -Force
        Write-Host ("[P63] backup: {0} -> backups\{1}\{2}.bak-P63" -f $rel, $dateTag, $rel)
    }
    New-Item -ItemType Directory -Force -Path (Split-Path $dst -Parent) | Out-Null
    Copy-Item $srcFile $dst -Force
    Write-Host ("[P63] copied: {0}" -f $rel)
    $copied++
}
Write-Host ("[P63] files: {0} copied, {1} skipped (already patched)" -f $copied, $skipped)

# --- 2) Cargo.toml: reqwest features += "socks" (P63/tg) ---
$tomlPath = Join-Path $root 'Cargo.toml'
if (-not (Test-Path $tomlPath)) {
    Write-Host '[P63] FAIL: Cargo.toml not found'
    exit 1
}
$toml = [System.IO.File]::ReadAllText($tomlPath)
if ($toml.Contains('P63/tg')) {
    Write-Host '[P63] Cargo.toml: socks feature already present - skip'
} elseif ($NoSocks) {
    Write-Host '[P63] -NoSocks: Cargo.toml untouched (TG_PROXY http:// works, socks5:// ignored at runtime)'
} else {
    $tomlBak = Join-Path $bakDir 'Cargo.toml.bak-P63'
    Copy-Item $tomlPath $tomlBak -Force
    Write-Host ("[P63] backup: Cargo.toml -> backups\{0}\Cargo.toml.bak-P63" -f $dateTag)
    $patched = $toml -replace '(?m)^reqwest\s*=[^\r\n]*', 'reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls", "socks"] } # P63/tg: socks = SOCKS5 for Telegram TG_PROXY'
    if (-not $patched.Contains('"socks"') -or -not $patched.Contains('P63/tg')) {
        Write-Host '[P63] FAIL: reqwest line in Cargo.toml not recognized - nothing written'
        exit 1
    }
    [System.IO.File]::WriteAllText($tomlPath, $patched, (New-Object System.Text.UTF8Encoding($false)))
    Write-Host '[P63] Cargo.toml: reqwest features += "socks" (first build fetches tokio-socks from crates.io)'
}

# --- 3) check_packs.ps1: append v11 tail ---
$cpPath = Join-Path $root 'check_packs.ps1'
$tailTpl = Join-Path $here 'check_packs_v11_tail.txt'
if (Test-Path $cpPath) {
    $cpRaw = [System.IO.File]::ReadAllText($cpPath)
    if ($cpRaw.Contains('appended by P63')) {
        Write-Host '[P63] check_packs.ps1: v11 tail already present - skip'
    } elseif (-not (Test-Path $tailTpl)) {
        Write-Host '[P63] WARN: check_packs_v11_tail.txt missing - check_packs not updated'
    } else {
        $cpBak = Join-Path $bakDir 'check_packs.ps1.bak-P63'
        Copy-Item $cpPath $cpBak -Force
        Write-Host ("[P63] backup: check_packs.ps1 -> backups\{0}\check_packs.ps1.bak-P63" -f $dateTag)
        $tail = [System.IO.File]::ReadAllText($tailTpl)
        [System.IO.File]::AppendAllText($cpPath, ("`r`n" + $tail), (New-Object System.Text.UTF8Encoding($false)))
        Write-Host '[P63] check_packs.ps1: v11 tail appended'
    }
} else {
    Write-Host '[P63] WARN: check_packs.ps1 not found - skipped'
}

# --- 4) .env hints (presence only, values never printed) ---
$envFile = Join-Path $root '.env'
$hasProxy = $false
$hasEnabled = $false
if (Test-Path $envFile) {
    $envTxt = [System.IO.File]::ReadAllText($envFile)
    $hasProxy = ($envTxt -match '(?m)^\s*TG_PROXY\s*=')
    $hasEnabled = ($envTxt -match '(?m)^\s*TG_ENABLED\s*=')
}
if ($hasProxy) {
    Write-Host '[P63] .env: TG_PROXY present - alerts will go through proxy after restart'
} else {
    Write-Host '[P63] NOTE: Telegram is blocked in RF. To make alerts work, add ONE line to .env:'
    Write-Host '        TG_PROXY=socks5://127.0.0.1:10808   # v2rayN default socks port'
    Write-Host '      (or TG_PROXY=http://127.0.0.1:10809 for v2rayN http, TG_PROXY=socks5://127.0.0.1:7890 for Clash mixed)'
    Write-Host '      Without proxy the breaker will open after ~1 min and WARN-spam stops (alerts resume automatically).'
}
if (-not $hasEnabled) {
    Write-Host '      Optional kill-switch: TG_ENABLED=false in .env turns alerts off completely.'
}

# --- marker count + result (rs files + Cargo.toml) ---
$total = 0
foreach ($rel in $targets) {
    $dst = Join-Path $root $rel
    if (Test-Path $dst) { $total += ([regex]::Matches([System.IO.File]::ReadAllText($dst), 'P63/')).Count }
}
if ((-not $NoSocks) -and (Test-Path $tomlPath)) {
    $total += ([regex]::Matches([System.IO.File]::ReadAllText($tomlPath), 'P63/')).Count
}
$expected = 14
if ($NoSocks) { $expected = 13 }
Write-Host ("[P63] markers P63/: {0}  (expected {1}: telegram.rs=13, Cargo.toml=1)" -f $total, $expected)
if ($total -lt $expected) {
    Write-Host '[P63] RESULT: FAIL - restore from backups\*.bak-P63 if needed'
    exit 1
}
Write-Host '[P63] RESULT: OK'
Write-Host ''
Write-Host 'Next:'
Write-Host '  1) powershell -ExecutionPolicy Bypass -File .\build.ps1    # stop -> test (73) -> build -> deploy'
Write-Host '     (first build downloads tokio-socks from crates.io - needs internet once)'
Write-Host '  2) powershell -ExecutionPolicy Bypass -File .\check_packs.ps1  # P63 row must show 14'
Write-Host '  3) expected log lines: "Telegram: transport via proxy (scheme=...)" (or "direct"),'
Write-Host '     "Telegram: alerts ON (P63: ...)" - and NO more "retry in 3s" storm'
