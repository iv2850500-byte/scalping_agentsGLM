# =====================================================================
# install_p61.ps1 - P61: deploy auto-stop + build.ps1 one-command wrapper
#
# What it does (idempotent, safe to re-run):
#   1) deploy.ps1     : inserts the P61/1 auto-stop block (kill bot by name
#                       or exe path, wait until target\release\scalping_agents.exe
#                       is unlocked, max 15 s, fail-closed) BEFORE the first
#                       statement (anchor: the "[deploy] root" line), so the
#                       Stop -> Build -> Deploy order is enforced automatically.
#   2) build.ps1      : creates a one-command update wrapper
#                       stop -> cargo test --lib -> cargo build --release -> deploy -SkipBuild
#                       with fail-fast on tests/build errors.
#   3) check_packs.ps1: appends the v9 tail (P61 marker row).
#   4) Syntax-checks every touched file; aborts with FAIL on any parse error.
#
# P61 touches only .ps1 files - NO Rust rebuild is required for the running bot.
#
# Usage:  powershell -ExecutionPolicy Bypass -File .\P61_pack\install_p61.ps1
# Backups: backups\<yyyy-MM-dd>\<file>.bak-P61
# =====================================================================
$ErrorActionPreference = 'Stop'

# --- repo root detection (works from pack folder or from repo root) ---
$here = $PSScriptRoot
if (Test-Path (Join-Path $here 'src')) {
    $root = $here
} elseif (Test-Path (Join-Path (Split-Path $here -Parent) 'src')) {
    $root = Split-Path $here -Parent
} else {
    Write-Host '[P61] ERROR: repo root not found (no src\ next to or above this pack folder)'
    exit 1
}
Write-Host ("[P61] root = {0}" -f $root)

$dateTag = Get-Date -Format 'yyyy-MM-dd'
$bakDir = Join-Path $root ("backups\" + $dateTag)
New-Item -ItemType Directory -Force -Path $bakDir | Out-Null

function Backup-P61File([string]$path) {
    if (Test-Path $path) {
        $leaf = Split-Path $path -Leaf
        Copy-Item $path (Join-Path $bakDir ($leaf + '.bak-P61')) -Force
        Write-Host ("[P61] backup: {0} -> backups\{1}\{2}.bak-P61" -f $leaf, $dateTag, $leaf)
    }
}

function Normalize-Crlf([string]$s) {
    return (($s -replace "`r`n", "`n") -replace "`n", "`r`n")
}

# --- templates shipped next to this installer ---
$blockTpl = Join-Path $here 'deploy_p61_block.ps1.txt'
$buildTpl = Join-Path $here 'build_p61.ps1.template'
$tailTpl  = Join-Path $here 'check_packs_v9_tail.txt'
foreach ($t in @($blockTpl, $buildTpl, $tailTpl)) {
    if (-not (Test-Path $t)) {
        Write-Host ("[P61] ERROR: missing template {0} - the pack folder must stay intact" -f (Split-Path $t -Leaf))
        exit 1
    }
}
$block       = Normalize-Crlf ([System.IO.File]::ReadAllText($blockTpl))
$buildScript = Normalize-Crlf ([System.IO.File]::ReadAllText($buildTpl))
$tail        = Normalize-Crlf ([System.IO.File]::ReadAllText($tailTpl))

# ---------------------------------------------------------------------
# 1) deploy.ps1: insert auto-stop block before the "[deploy] root" line
# ---------------------------------------------------------------------
$deployPath = Join-Path $root 'deploy.ps1'
if (-not (Test-Path $deployPath)) {
    Write-Host '[P61] ERROR: deploy.ps1 not found in repo root'
    exit 1
}
$deployRaw = [System.IO.File]::ReadAllText($deployPath)
if ($deployRaw.Contains('P61/')) {
    Write-Host '[P61] deploy.ps1: P61 block already present - skip'
} else {
    $lines = [System.IO.File]::ReadAllLines($deployPath)
    $anchor = -1
    for ($i = 0; $i -lt $lines.Count; $i++) {
        if ($lines[$i].Contains('[deploy] root')) { $anchor = $i; break }
    }
    if ($anchor -lt 0) {
        Write-Host '[P61] ERROR: anchor line "[deploy] root" not found in deploy.ps1 - nothing patched'
        exit 1
    }
    Backup-P61File $deployPath
    $before = @()
    if ($anchor -gt 0) { $before = @($lines[0..($anchor - 1)]) }
    $after = @($lines[$anchor..($lines.Count - 1)])
    $newLines = @($before + $block + $after)
    $bytes = [System.IO.File]::ReadAllBytes($deployPath)
    $hasBom = ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF)
    [System.IO.File]::WriteAllText($deployPath, (($newLines -join "`r`n") + "`r`n"), (New-Object System.Text.UTF8Encoding($hasBom)))
    Write-Host ("[P61] deploy.ps1: auto-stop block inserted before line {0} (anchor: root line)" -f ($anchor + 1))
}

# ---------------------------------------------------------------------
# 2) build.ps1: one-command update wrapper
# ---------------------------------------------------------------------
$buildPath = Join-Path $root 'build.ps1'
if ((Test-Path $buildPath) -and ([System.IO.File]::ReadAllText($buildPath)).Contains('P61/')) {
    Write-Host '[P61] build.ps1: already installed - skip'
} else {
    Backup-P61File $buildPath
    [System.IO.File]::WriteAllText($buildPath, $buildScript, (New-Object System.Text.UTF8Encoding($false)))
    Write-Host '[P61] build.ps1: created (stop -> test -> build -> deploy, fail-fast)'
}

# ---------------------------------------------------------------------
# 3) check_packs.ps1: append v9 tail
# ---------------------------------------------------------------------
$cpPath = Join-Path $root 'check_packs.ps1'
if (Test-Path $cpPath) {
    $cpRaw = [System.IO.File]::ReadAllText($cpPath)
    if ($cpRaw.Contains('appended by P61')) {
        Write-Host '[P61] check_packs.ps1: v9 tail already present - skip'
    } else {
        Backup-P61File $cpPath
        [System.IO.File]::AppendAllText($cpPath, ("`r`n" + $tail), (New-Object System.Text.UTF8Encoding($false)))
        Write-Host '[P61] check_packs.ps1: v9 tail appended'
    }
} else {
    Write-Host '[P61] WARN: check_packs.ps1 not found - skipped'
}

# ---------------------------------------------------------------------
# 4) syntax check of every touched file (fail-closed)
# ---------------------------------------------------------------------
$bad = 0
foreach ($f in @($deployPath, $buildPath, $cpPath)) {
    if (Test-Path $f) {
        $errs = $null
        [void][System.Management.Automation.Language.Parser]::ParseFile($f, [ref]$null, [ref]$errs)
        if ($errs -and @($errs).Count -gt 0) {
            $bad++
            Write-Host ("[P61] SYNTAX ERROR in {0} (line {1}): {2}" -f (Split-Path $f -Leaf), $errs[0].Extent.StartLineNumber, $errs[0].Message)
        }
    }
}

# ---------------------------------------------------------------------
# 5) marker count + result
# ---------------------------------------------------------------------
$m61 = 0
foreach ($f in @($deployPath, $buildPath)) {
    if (Test-Path $f) { $m61 += ([regex]::Matches([System.IO.File]::ReadAllText($f), 'P61/')).Count }
}
Write-Host ("[P61] markers P61/: {0}  (expected 5: 3 in deploy.ps1 + 2 in build.ps1)" -f $m61)
if ($bad -gt 0 -or $m61 -lt 5) {
    Write-Host '[P61] RESULT: FAIL - restore touched files from backups\*.bak-P61 if needed'
    exit 1
}
Write-Host '[P61] RESULT: OK'
Write-Host ''
Write-Host 'Next: powershell -ExecutionPolicy Bypass -File .\build.ps1'
Write-Host '      (one command: stop bot -> test -> build -> deploy; ~4.5 min bot downtime on testnet)'
Write-Host 'Note: P61 touched only .ps1 files - the currently running bot already has P53-P60, no rebuild needed.'
