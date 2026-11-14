# =====================================================================
# install_p62.ps1 - P62: Telegram alert hub v2
#
# What it does (idempotent, safe to re-run):
#   1) Copies 5 patched sources (backup each into backups\<date>\<path>.bak-P62):
#        src\utils\telegram.rs             - hub v2: shared client + queue worker,
#                                            rate-limit 1.15s, retry, coalescing 60s,
#                                            notify_once, token never in logs;
#                                            poller v2 = suspensions + KILL watch
#                                            (journal-CLOSE polling removed - closes
#                                            are now INSTANT, no duplicates)
#        src\state\trade_journal.rs        - instant close alert at record_close
#                                            (single funnel of ALL close paths)
#        src\main.rs                       - entry alert, supervisor restart alerts,
#                                            process start/shutdown alerts, C4
#                                            SL/TP-restored + orphan alerts
#        src\risk\mod.rs                   - circuit breaker trip alert; per-bot
#                                            day-loss thresholds 50/80/100%
#        src\risk\global_risk_manager.rs   - P15 portfolio warnings 50/80% before
#                                            HARD HALT (once per UTC day per level)
#   2) Appends the v10 tail to check_packs.ps1.
#   3) Checks .env for TG_BOT_TOKEN / TG_CHAT_ID (presence only - values are
#      NEVER printed). Without them the module stays a silent no-op.
#
# 9 new unit tests (p62_tests) -> cargo test --lib must show 65 passed.
#
# Usage:  powershell -ExecutionPolicy Bypass -File .\P62_pack\install_p62.ps1
# After:  powershell -ExecutionPolicy Bypass -File .\build.ps1
# =====================================================================
$ErrorActionPreference = 'Stop'

# --- repo root detection (works from pack folder or from repo root) ---
$here = $PSScriptRoot
if (Test-Path (Join-Path $here 'src')) {
    $root = $here
} elseif (Test-Path (Join-Path (Split-Path $here -Parent) 'src')) {
    $root = Split-Path $here -Parent
} else {
    Write-Host '[P62] ERROR: repo root not found (no src\ next to or above this pack folder)'
    exit 1
}
Write-Host ("[P62] root = {0}" -f $root)

$dateTag = Get-Date -Format 'yyyy-MM-dd'
$bakDir = Join-Path $root ("backups\" + $dateTag)
New-Item -ItemType Directory -Force -Path $bakDir | Out-Null

$targets = @(
    'src\utils\telegram.rs',
    'src\state\trade_journal.rs',
    'src\main.rs',
    'src\risk\mod.rs',
    'src\risk\global_risk_manager.rs'
)

$copied = 0
$skipped = 0
foreach ($rel in $targets) {
    $dst = Join-Path $root $rel
    $srcFile = Join-Path $here (Join-Path 'patched' $rel)
    if (-not (Test-Path $srcFile)) {
        Write-Host ("[P62] ERROR: pack file missing: {0} - the pack folder must stay intact" -f $rel)
        exit 1
    }
    if ((Test-Path $dst) -and ([System.IO.File]::ReadAllText($dst)).Contains('P62/')) {
        Write-Host ("[P62] {0} : already patched - skip" -f $rel)
        $skipped++
        continue
    }
    if (Test-Path $dst) {
        $bakPath = Join-Path $bakDir ($rel + '.bak-P62')
        New-Item -ItemType Directory -Force -Path (Split-Path $bakPath -Parent) | Out-Null
        Copy-Item $dst $bakPath -Force
        Write-Host ("[P62] backup: {0} -> backups\{1}\{2}.bak-P62" -f $rel, $dateTag, $rel)
    }
    New-Item -ItemType Directory -Force -Path (Split-Path $dst -Parent) | Out-Null
    Copy-Item $srcFile $dst -Force
    Write-Host ("[P62] copied: {0}" -f $rel)
    $copied++
}
Write-Host ("[P62] files: {0} copied, {1} skipped (already patched)" -f $copied, $skipped)

# --- check_packs.ps1: append v10 tail ---
$cpPath = Join-Path $root 'check_packs.ps1'
$tailTpl = Join-Path $here 'check_packs_v10_tail.txt'
if (Test-Path $cpPath) {
    $cpRaw = [System.IO.File]::ReadAllText($cpPath)
    if ($cpRaw.Contains('appended by P62')) {
        Write-Host '[P62] check_packs.ps1: v10 tail already present - skip'
    } elseif (-not (Test-Path $tailTpl)) {
        Write-Host '[P62] WARN: check_packs_v10_tail.txt missing - check_packs not updated'
    } else {
        $cpBak = Join-Path $bakDir 'check_packs.ps1.bak-P62'
        Copy-Item $cpPath $cpBak -Force
        Write-Host ("[P62] backup: check_packs.ps1 -> backups\{0}\check_packs.ps1.bak-P62" -f $dateTag)
        $tail = [System.IO.File]::ReadAllText($tailTpl)
        [System.IO.File]::AppendAllText($cpPath, ("`r`n" + $tail), (New-Object System.Text.UTF8Encoding($false)))
        Write-Host '[P62] check_packs.ps1: v10 tail appended'
    }
} else {
    Write-Host '[P62] WARN: check_packs.ps1 not found - skipped'
}

# --- .env hint (presence only, values never printed) ---
$envFile = Join-Path $root '.env'
$hasToken = $false
$hasChat = $false
if (Test-Path $envFile) {
    $envTxt = [System.IO.File]::ReadAllText($envFile)
    $hasToken = ($envTxt -match '(?m)^\s*TG_BOT_TOKEN\s*=')
    $hasChat = ($envTxt -match '(?m)^\s*TG_CHAT_ID\s*=')
}
if ($hasToken -and $hasChat) {
    Write-Host '[P62] .env: TG_BOT_TOKEN + TG_CHAT_ID present - alerts will be ON after restart'
} else {
    Write-Host '[P62] NOTE: alerts are env-gated (silent no-op without them). Add to .env:'
    if (-not $hasToken) { Write-Host '        TG_BOT_TOKEN=<token from @BotFather>' }
    if (-not $hasChat) { Write-Host '        TG_CHAT_ID=<your chat id>   # group ids are negative' }
    Write-Host '      (bot reads .env at startup via dotenvy; restart applies it)'
}

# --- marker count + result ---
$total = 0
foreach ($rel in $targets) {
    $dst = Join-Path $root $rel
    if (Test-Path $dst) { $total += ([regex]::Matches([System.IO.File]::ReadAllText($dst), 'P62/')).Count }
}
Write-Host ("[P62] markers P62/: {0}  (expected 18: tg=3, journal=1, main=9, risk/mod=2, global_risk=3)" -f $total)
if ($total -lt 18) {
    Write-Host '[P62] RESULT: FAIL - restore from backups\*.bak-P62 if needed'
    exit 1
}
Write-Host '[P62] RESULT: OK'
Write-Host ''
Write-Host 'Next:'
Write-Host '  1) powershell -ExecutionPolicy Bypass -File .\build.ps1    # stop -> test (65) -> build -> deploy'
Write-Host '  2) powershell -ExecutionPolicy Bypass -File .\check_packs.ps1  # P62 row must show 18'
Write-Host '  3) watch the first Telegram messages: Process started, per-bot starts'
