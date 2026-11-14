# check_run.ps1 (P51) - post-deploy runtime verification, read-only.
# The bot keeps bot.log open for writing, so plain ReadAllLines fails with
# 'file in use' - this tool opens the live file with FileShare ReadWrite+Delete.
# Checks: process alive; all 4 bots logged their start line; WARN/ERROR count
# since the current run began (auto-detected: 90s before the last tao_momentum
# start line; K6 fires during init, before it).
# Usage: powershell -ExecutionPolicy Bypass -File .\check_run.ps1
#        powershell -ExecutionPolicy Bypass -File .\check_run.ps1 -Since '2026-11-11T09:29:15'  (UTC)
param(
    [string]$Since = '',
    [string]$LogPath = ''
)
$ErrorActionPreference = 'Continue'
Set-Location 'C:\Users\BASE_PC\scalping_agentsGLM'
$ci = [Globalization.CultureInfo]::InvariantCulture
$bots = @('tao_momentum', 'btc_bounce', 'inj_liquidity', 'gold_dca')
$startedRx = '\u0411\u043E\u0442 \u0437\u0430\u043F\u0443\u0449\u0435\u043D'   # "Bot started" (ru), ascii-escaped codepoints

# -- 1) process --
$procs = @(Get-Process -Name 'scalping_agents' -ErrorAction SilentlyContinue)
$ok = $true
if ($procs.Count -gt 0) {
    foreach ($p in $procs) {
        $cpu = 0.0
        if ($p.CPU) { $cpu = [math]::Round($p.CPU, 1) }
        Write-Host ('PID {0}: alive, CPU={1}s, MEM={2}MB, started {3}' -f $p.Id, $cpu, [math]::Round($p.WorkingSet64/1MB, 0), $p.StartTime.ToString('yyyy-MM-dd HH:mm:ss', $ci))
    }
} else {
    Write-Host 'process scalping_agents: NOT RUNNING'
    $ok = $false
}

# -- 2) locate today log --
if ($LogPath -eq '') {
    $today = Get-Date -Format 'yyyy-MM-dd'
    foreach ($c in @(('.\logs\bot.log.' + $today), ('.\bot.log.' + $today))) {
        if (Test-Path $c) { $LogPath = (Get-Item $c).FullName; break }
    }
    if ($LogPath -eq '') {
        $g = Get-ChildItem . -Recurse -Filter ('bot.log.' + $today) -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1
        if ($g) { $LogPath = $g.FullName }
    }
}
if ($LogPath -eq '' -or -not (Test-Path $LogPath)) { Write-Host '[check_run] FAIL: log not found'; exit 1 }
Write-Host ('log: ' + $LogPath)

# -- 3) read live file (share-aware) --
try {
    $fs = New-Object IO.FileStream($LogPath, [IO.FileMode]::Open, [IO.FileAccess]::Read, ([IO.FileShare]'ReadWrite, Delete'))
    $sr = New-Object IO.StreamReader($fs)
    $all = $sr.ReadToEnd()
    $sr.Close()
    $fs.Close()
} catch {
    Write-Host ('[check_run] FAIL: cannot read log: ' + $_.Exception.Message)
    exit 1
}
$all = $all.Replace("`r", '')
$lines = @($all -split "`n")
Write-Host ('lines total: ' + $lines.Count)

# -- 4) run boundary: 90s before the last tao_momentum start line --
$cut = $Since
if ($cut -eq '') {
    $iStart = -1
    for ($i = $lines.Count - 1; $i -ge 0; $i--) {
        if ($lines[$i] -match ('\[tao_momentum\].*' + $startedRx)) { $iStart = $i; break }
    }
    if ($iStart -ge 0) {
        $t0 = [datetime]::ParseExact($lines[$iStart].Substring(0, 19), 'yyyy-MM-dd''T''HH:mm:ss', $ci).AddSeconds(-90)
        $cut = $t0.ToString('yyyy-MM-dd''T''HH:mm:ss', $ci)
    } else {
        $cut = '0000-01-01T00:00:00'
        Write-Host 'note: start line not found - scanning the whole file'
    }
}
Write-Host ('cut: ' + $cut + ' (UTC)')
$run = @($lines | Where-Object { $_.Length -ge 19 -and $_.StartsWith('2026-') -and [string]::CompareOrdinal($_.Substring(0, 19), $cut) -ge 0 })
Write-Host ('lines since cut: ' + $run.Count)

# -- 5) per-bot start lines --
foreach ($b in $bots) {
    $n = @($run | Where-Object { $_ -match ('\[' + $b + '\].*' + $startedRx) }).Count
    $t = @($run | Where-Object { $_ -match ('\[' + $b + '\]') }).Count
    if ($n -lt 1) { $ok = $false }
    Write-Host ('{0,-14} lines={1,-7} started={2}' -f $b, $t, $n)
}

# -- 6) WARN/ERROR since cut --
$w = @($run | Where-Object { $_ -match ' WARN | ERROR ' })
Write-Host ('WARN/ERROR since cut: ' + $w.Count)
if ($w.Count -gt 0) { $w | Select-Object -Last 10; $ok = $false }

# -- 7) inj strategy line --
$inj = @($run | Where-Object { $_ -match '\[inj_liquidity\].*(LiquidityGrab|liquidity_grab)' })
if ($inj.Count -gt 0) { $inj | Select-Object -Last 2 } else { Write-Host 'inj_liquidity: no strategy line yet' }

if ($ok) { Write-Host '[check_run] RESULT: OK (process alive, 4/4 started, no WARN/ERROR since cut)'; exit 0 }
Write-Host '[check_run] RESULT: ATTENTION - see lines above'
exit 2
