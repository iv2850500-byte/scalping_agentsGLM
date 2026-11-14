# install_p53_p60.ps1 - applies packs P53..P60 (full-file replacements).
# ASCII-only on purpose (PowerShell -File + no BOM). Run from the repo root:
#   powershell -ExecutionPolicy Bypass -File .\P53-P60_pack\install_p53_p60.ps1
# The pack folder must stay at <repo root>\P53-P60_pack\.
# Source state this pack was built against: src archive of 2026-11-12
# (post-P51, post-dashboard-P52). Full-file replacement = current content
# of those 14 files is REPLACED; everything else is untouched.

$ErrorActionPreference = 'Stop'

$packRoot = $PSScriptRoot
$repoRoot = Split-Path -Parent $packRoot
Set-Location -Path $repoRoot

if (-not (Test-Path (Join-Path $repoRoot 'Cargo.toml'))) {
    Write-Host "[P53-60] ERROR: Cargo.toml not found in '$repoRoot' - run from the pack folder inside the repo"
    exit 1
}

$day = Get-Date -Format 'yyyy-MM-dd'
$bakDir = Join-Path $repoRoot ("backups\" + $day)
New-Item -ItemType Directory -Force -Path $bakDir | Out-Null

$files = @(
    'src\exchange\user_stream.rs',
    'src\exchange\traits.rs',
    'src\exchange\binance_futures.rs',
    'src\exchange\resilient_client.rs',
    'src\risk\global_risk_manager.rs',
    'src\utils\killfile.rs',
    'src\utils\math.rs',
    'src\execution\execution_engine.rs',
    'src\agents\risk_agent.rs',
    'src\main.rs',
    'src\strategies\liquidity_grab.rs',
    'src\strategies\momentum.rs',
    'src\strategies\keltner_dca.rs',
    'src\strategies\bounce_trader.rs'
)

# ---------- 1) backup ----------
foreach ($f in $files) {
    $src = Join-Path $repoRoot $f
    if (Test-Path $src) {
        $tagged = $f -replace '\\', '__'
        Copy-Item $src (Join-Path $bakDir ($tagged + '.bak-P53-60')) -Force
    }
}
Write-Host ("[P53-60] backup: {0} files -> {1}" -f $files.Count, $bakDir)

# ---------- 2) copy patched files ----------
foreach ($f in $files) {
    $from = Join-Path $packRoot $f
    $to   = Join-Path $repoRoot $f
    if (-not (Test-Path $from)) {
        Write-Host "[P53-60] ERROR: pack file missing: $f"
        exit 1
    }
    Copy-Item $from $to -Force
}
Write-Host "[P53-60] copied 14 patched files"

# ---------- 3) append v8 tail to check_packs.ps1 (idempotent) ----------
$cp = Join-Path $repoRoot 'check_packs.ps1'
if (Test-Path $cp) {
    $marker = 'appended by P53-P60 (v8 tail)'
    $content = Get-Content $cp -Raw
    if ($content -notmatch [regex]::Escape($marker)) {
        $tail = @'

# ===================== appended by P53-P60 (v8 tail) =====================
try {
$p52 = 0
$p52bak = Get-ChildItem -Recurse -Path .\backups -Filter 'dashboard.pre-P52.html' -File -ErrorAction SilentlyContinue | Select-Object -First 1
if ($null -ne $p52bak -and (Test-Path .\src\webui\dashboard.html)) {
    if ((Get-FileHash .\src\webui\dashboard.html -Algorithm SHA256).Hash -ne (Get-FileHash $p52bak.FullName -Algorithm SHA256).Hash) { $p52 = 1 }
}
('markers P52/: {0,3}   installed if 1  (dashboard.html replaced; hash differs from dashboard.pre-P52.html backup)' -f $p52)
$mfun = { param($pat) (Get-ChildItem .\src -Recurse -Filter *.rs | Select-String -Pattern $pat -AllMatches | ForEach-Object { $_.Matches.Count } | Measure-Object -Sum).Sum }
('markers P53/: {0,3}   installed if 32 (H9 exchange-close watcher + C4 openAlgoOrders protection reconcile)' -f (& $mfun 'P53/'))
('markers P54/: {0,3}   installed if 16 (portfolio HARD HALT: K6 base, fail-closed persist, engine gate; C8 kill fail-closed)' -f (& $mfun 'P54/'))
('markers P55/: {0,3}   installed if 1  (D2: periodic reconcile switched to get_positions_authoritative)' -f (& $mfun 'P55/'))
('markers P56/: {0,3}   installed if 5  (S11 rails validation + money-math tests)' -f (& $mfun 'P56/'))
('markers P57/: {0,3}   installed if 4  (liquidity gate: orderbook depth before entry)' -f (& $mfun 'P57/'))
('markers P58/: {0,3}   installed if 2  (volume confirmation of sweep)' -f (& $mfun 'P58/'))
('markers P59/: {0,3}   installed if 7  (ATR-normalized stop + EMA50 regime gate)' -f (& $mfun 'P59/'))
('markers P60/: {0,3}   installed if 17 (equity sizing: live equity x risk multiplier, all 4 strategies)' -f (& $mfun 'P60/'))
} catch { ('tail error: {0}' -f $_) }
'@
        Add-Content -Path $cp -Value $tail -Encoding UTF8
        Write-Host "[P53-60] check_packs.ps1: v8 tail appended"
    } else {
        Write-Host "[P53-60] check_packs.ps1: v8 tail already present (skip)"
    }
} else {
    Write-Host "[P53-60] WARN: check_packs.ps1 not found - v8 tail skipped"
}

# ---------- 4) verify markers ----------
$expected = @{ 'P53/' = 32; 'P54/' = 16; 'P55/' = 1; 'P56/' = 5; 'P57/' = 4; 'P58/' = 2; 'P59/' = 7; 'P60/' = 17 }
$fail = 0
foreach ($k in $expected.Keys) {
    $c = (Get-ChildItem .\src -Recurse -Filter *.rs | Select-String -Pattern $k -AllMatches | ForEach-Object { $_.Matches.Count } | Measure-Object -Sum).Sum
    if ([int]$c -eq $expected[$k]) {
        Write-Host ("[P53-60] markers {0,-5}: {1,3}  OK" -f $k, $c)
    } else {
        Write-Host ("[P53-60] markers {0,-5}: {1,3}  MISMATCH (expected {2})" -f $k, $c, $expected[$k])
        $fail = 1
    }
}
if ($fail -ne 0) { Write-Host '[P53-60] RESULT: FAIL - do not build; restore from backup'; exit 1 }
Write-Host '[P53-60] RESULT: OK'
Write-Host ''
Write-Host 'Next steps (deploy):'
Write-Host '  1) Get-Process scalping_agents -ErrorAction SilentlyContinue | Stop-Process -Force'
Write-Host '  2) Start-Sleep -Seconds 3'
Write-Host '  3) cargo test --lib          # 56 tests must pass'
Write-Host '  4) cargo build --release     # ~3.5 min'
Write-Host '  5) powershell -ExecutionPolicy Bypass -File .\deploy.ps1 -SkipBuild'
Write-Host '  6) powershell -ExecutionPolicy Bypass -File .\check_packs.ps1'
