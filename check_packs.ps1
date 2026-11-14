# check_packs.ps1 (v3) - which fix packs are installed in this repo?
# v3: +P41 row (P2-tail: F11/S12/E14/W5-W9/UI1-UI7/D13-D16/D15/K6, 104 markers),
#     fixed stale thresholds: P35 "installed if 16" (P38 replaced 2 stub markers 2c),
#     P38 threshold is now 9: pre-P41 it was 10, but P41/E14 removed the dead
#     maker-exit branch in execution_engine.rs which contained one "P38/2c"
#     marker ("close on FACT"). The twin "P39/N3 + P38/2c" in main.rs remains.
# Usage:  powershell -ExecutionPolicy Bypass -File .\check_packs.ps1
#         (run from the repo root, or pass -RepoPath)
param(
    [string]$RepoPath = "."
)

$src = Join-Path $RepoPath 'src'
if (-not (Test-Path $src)) {
    Write-Host "ERROR: no src\ folder under '$RepoPath'"
    exit 1
}

function Count-Marker([string]$pattern) {
    $hits = Get-ChildItem -Recurse -Path $src -Filter *.rs -File |
        Select-String -SimpleMatch $pattern
    return ($hits | Measure-Object).Count
}

Write-Host "=== PACK CHECK: $RepoPath ==="

$m35 = Count-Marker 'P35/'
$m36 = Count-Marker 'P36/'
$m37 = Count-Marker 'P37/'
$m38 = Count-Marker 'P38/'
$m39 = Count-Marker 'P39/'
$m40 = Count-Marker 'P40/'
$m41 = Count-Marker 'P41/'

Write-Host ("markers P35/: {0,3}   installed if 16  (code fixes S1/H1/...; 2 stubs 2c replaced by P38)" -f $m35)
Write-Host ("markers P36/: {0,3}   installed if 12  (client truth: C1a/C1c/D1/E5b/F5)" -f $m36)
Write-Host ("markers P37/: {0,3}   installed if  7  (config hardening: K2/K3/K4)" -f $m37)
Write-Host ("markers P38/: {0,3}   installed if  9  (exit-partial; was 10 pre-P41: 1 marker lived in the dead maker branch removed by P41/E14)" -f $m38)
Write-Host ("markers P39/: {0,3}   installed if 19  (keltner ladder: H12/E7/F2/F3/N3)" -f $m39)
Write-Host ("markers P40/: {0,3}   installed if 10  (secret masking: K1)" -f $m40)
Write-Host ("markers P41/: {0,3}   installed if 104 (P2-tail: F11/S12/E14/W5-W9/UI1-UI7/D13-D16/D15/K6)" -f $m41)

foreach ($p in 'P35','P36','P37','P38','P39','P40','P41') {
    $baks = Get-ChildItem -Recurse -Path $RepoPath -Filter ("*.bak-" + $p) -File
    Write-Host ("backups .bak-{0}: {1}" -f $p, ($baks | Measure-Object).Count)
}

$mv3 = Join-Path $RepoPath 'src\strategies\momentum_v3.rs'
if (Test-Path $mv3) { Write-Host 'momentum_v3.rs: still present (P38 not run or skipped removal)' }
else { Write-Host 'momentum_v3.rs: removed (P38 signature)' }

# P41: wfa binaries excluded from release via required-features
$cargo = Join-Path $RepoPath 'Cargo.toml'
if (Test-Path $cargo) {
    $c = Get-Content $cargo -Raw
    if ($c -match 'required-features = \["wfa"\]') {
        Write-Host 'wfa bins gated (P41: required-features = ["wfa"])'
    } else {
        Write-Host 'wfa bins NOT gated (Cargo.toml without P41 [[bin]] sections)'
    }
}

$exe = Join-Path $RepoPath 'target\release\scalping_agents.exe'
if (Test-Path $exe) {
    $i = Get-Item $exe
    Write-Host ("exe built: {0}  (LastWriteTime - when the current binary was compiled)" -f $i.LastWriteTime)
} else {
    Write-Host 'exe not found (target\release\scalping_agents.exe)'
}

Write-Host ''
Write-Host 'NOTE: marker counts reflect SOURCE files. The running EXE includes a pack'
Write-Host 'only if it was REBUILT after that pack was installed (check exe time).'
Write-Host '=== done ==='

# ===================== appended by P42-P44 (v4 tail) =====================
try {
$c42 = (Get-ChildItem .\src -Recurse -Filter *.rs | Select-String -Pattern 'P42/' -AllMatches | ForEach-Object { $_.Matches.Count } | Measure-Object -Sum).Sum
('markers P42/: {0,3}   installed if 3  (K6 account-level base; dead resolve_order_fate removed)' -f $c42)
$c43 = (Select-String -Path .\src\webui\dashboard.html -Pattern 'P43/' -AllMatches | ForEach-Object { $_.Matches.Count } | Measure-Object -Sum).Sum
('markers P43/: {0,3}   installed if 2  (UI5 residual: data-bot delegation, no inline onclick)' -f $c43)
$c44 = (Select-String -Path .\Cargo.toml -Pattern 'P44/' -AllMatches | ForEach-Object { $_.Matches.Count } | Measure-Object -Sum).Sum
('markers P44/: {0,3}   installed if 1  (wfa research cmd fixed to --profile wfa)' -f $c44)
} catch { ('tail error: {0}' -f $_) }

# ===================== appended by P45-P47 (v5 tail) =====================
try {
$c45 = (Get-ChildItem .\src -Recurse -Filter *.rs | Select-String -Pattern 'P42/' -AllMatches | ForEach-Object { $_.Matches.Count } | Measure-Object -Sum).Sum
('markers P45/P42/: {0,3}   installed if 3  (K6 account-base code + dead resolve helper removed; fixes startup unknown-field crash)' -f $c45)
$c46rs = (Get-ChildItem .\src -Recurse -Filter *.rs | Select-String -Pattern 'P46/' -AllMatches | ForEach-Object { $_.Matches.Count } | Measure-Object -Sum).Sum
$c46html = (Select-String -Path .\src\webui\dashboard.html -Pattern 'P46/' -AllMatches | ForEach-Object { $_.Matches.Count } | Measure-Object -Sum).Sum
('markers P46/: {0,3}   installed if 2  (UI8 robust mtf-row lookup; W10 config-read warn)' -f (($c46rs) + ($c46html)))
$c47 = if (Test-Path .\check_startup.ps1) { 1 } else { 0 }
('markers P47/: {0,3}   installed if 1  (check_startup.ps1 startup probe)' -f $c47)
} catch { ('tail error: {0}' -f $_) }

# ===================== appended by P48-P49 (v6 tail, repaired by P51) =====================
try {
$c48 = (Select-String -Path .\cleanup_logs.ps1 -Pattern 'P48/' -AllMatches | Measure-Object).Count
('markers P48/: {0,3}   installed if 1  (log retention tool: cleanup_logs.ps1)' -f $c48)
$c49 = (Select-String -Path .\check_k6.ps1 -Pattern 'P49/' -AllMatches | Measure-Object).Count
('markers P49/: {0,3}   installed if 1  (K6 preflight tool: check_k6.ps1)' -f $c49)
} catch { ('tail error: {0}' -f $_) }

# ===================== appended by P51 (v7 tail) =====================
try {
$c50cfg = (Select-String -Path .\config.toml -Pattern '\[P50\]' -AllMatches | Measure-Object).Count
$c50k6 = (Select-String -Path .\check_k6.ps1 -Pattern '\[P50\]' -AllMatches | Measure-Object).Count
('markers P50/: {0,3}   installed if 7  (config.toml repaired: 4 bots, daily x0.8 sum=40, mcp=4; check_k6 v2)' -f ($c50cfg + $c50k6))
$c51 = if (Test-Path .\check_run.ps1) { 1 } else { 0 }
('markers P51/: {0,3}   installed if 1  (check_run.ps1 runtime verification tool)' -f $c51)
} catch { ('tail error: {0}' -f $_) }

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


# ===================== appended by P61 (v9 tail) =====================
try {
$c61d = 0
if (Test-Path .\deploy.ps1) { $c61d = (Select-String -Path .\deploy.ps1 -Pattern 'P61/' -AllMatches | ForEach-Object { $_.Matches.Count } | Measure-Object -Sum).Sum }
if ($null -eq $c61d) { $c61d = 0 }
$c61b = 0
if (Test-Path .\build.ps1) { $c61b = (Select-String -Path .\build.ps1 -Pattern 'P61/' -AllMatches | ForEach-Object { $_.Matches.Count } | Measure-Object -Sum).Sum }
if ($null -eq $c61b) { $c61b = 0 }
('markers P61/: {0,3}   installed if 5  (deploy auto-stop + unlock-wait; build.ps1 one-command update wrapper)' -f ($c61d + $c61b))
} catch { ('tail error: {0}' -f $_) }


# ===================== appended by P62 (v10 tail) =====================
try {
$c62 = (Get-ChildItem .\src -Recurse -Filter *.rs | Select-String -Pattern 'P62/' -AllMatches | ForEach-Object { $_.Matches.Count } | Measure-Object -Sum).Sum
if ($null -eq $c62) { $c62 = 0 }
('markers P62/: {0,3}   installed if 18 (telegram hub v2: instant entry/close alerts via journal funnel, coalescing, rate-limit; day-loss 50/80/100; P15 warnings 50/80; breaker trip; supervisor restarts; KILL watch)' -f $c62)
} catch { ('tail error: {0}' -f $_) }


# ===================== appended by P63 (v11 tail) =====================
try {
$c63 = (Get-ChildItem .\src -Recurse -Filter *.rs | Select-String -Pattern 'P63/' -AllMatches | ForEach-Object { $_.Matches.Count } | Measure-Object -Sum).Sum
if ($null -eq $c63) { $c63 = 0 }
$t63 = 0
if (Test-Path .\Cargo.toml) { $t63 = ([regex]::Matches([System.IO.File]::ReadAllText((Join-Path (Get-Location) 'Cargo.toml')), 'P63/')).Count }
('markers P63/: {0,3}   installed if 14 (tg transport hardening: TG_PROXY socks5/http for RF-blocking, TG_ENABLED kill-switch, retry 3s/15s x3, circuit breaker 10-60min with outage digest, auth-dead 401/403/404; reqwest+socks)' -f ($c63 + $t63))
} catch { ('tail error: {0}' -f $_) }
