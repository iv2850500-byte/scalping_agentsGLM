# =====================================================================
# hotfix_p63_msg.ps1 - P63 R2 hotfix: add missing "struct Msg" to
# src\utils\telegram.rs IN PLACE, without re-extracting the pack zip.
#
# Why: R1 telegram.rs on disk is missing the Msg struct definition
# (build error E0425 "cannot find type Msg"). Evidence from the last
# install log ("already patched - skip") shows the OLD pack ran again -
# the updated zip was not extracted over the old files. This hotfix
# patches the single missing piece directly. ASCII-only, idempotent,
# fail-closed (backup first, verify after).
#
# Usage:  powershell -ExecutionPolicy Bypass -File .\hotfix_p63_msg.ps1
# After:  powershell -ExecutionPolicy Bypass -File .\build.ps1
# =====================================================================
$ErrorActionPreference = 'Stop'

# --- repo root detection ---
$here = $PSScriptRoot
if (Test-Path (Join-Path $here 'src')) {
    $root = $here
} elseif (Test-Path (Join-Path (Split-Path $here -Parent) 'src')) {
    $root = Split-Path $here -Parent
} else {
    Write-Host '[HOTFIX] ERROR: repo root not found (no src\ next to or above this script)'
    exit 1
}
Write-Host ("[HOTFIX] root = {0}" -f $root)

$p = Join-Path $root 'src\utils\telegram.rs'
if (-not (Test-Path $p)) {
    Write-Host '[HOTFIX] ERROR: src\utils\telegram.rs not found'
    exit 1
}
$t = [System.IO.File]::ReadAllText($p)

# --- idempotency: already fixed? ---
if ($t.Contains('struct Msg')) {
    Write-Host '[HOTFIX] struct Msg already present - nothing to do'
    Write-Host '[HOTFIX] RESULT: OK'
    exit 0
}

# --- sanity: this must be the P63/R1 file with expected layout ---
if (-not $t.Contains('P63/')) {
    Write-Host '[HOTFIX] ERROR: telegram.rs has no P63/ markers - unexpected file version'
    exit 1
}
$anchor = 'static DROPPED: AtomicU64 = AtomicU64::new(0);'
if (-not $t.Contains($anchor)) {
    Write-Host '[HOTFIX] ERROR: anchor (static DROPPED) not found - unexpected file layout'
    exit 1
}

# --- backup (separate suffix: original .bak-P63 stays untouched) ---
$dateTag = Get-Date -Format 'yyyy-MM-dd'
$bakDir = Join-Path $root ("backups\" + $dateTag)
New-Item -ItemType Directory -Force -Path $bakDir | Out-Null
$bakPath = Join-Path $bakDir 'src\utils\telegram.rs.bak-P63R2'
New-Item -ItemType Directory -Force -Path (Split-Path $bakPath -Parent) | Out-Null
Copy-Item $p $bakPath -Force
Write-Host ("[HOTFIX] backup: src\utils\telegram.rs -> backups\{0}\src\utils\telegram.rs.bak-P63R2" -f $dateTag)

# --- insert struct Msg after the DROPPED anchor (same place as P63 R2) ---
$nl = if ($t.Contains("`r`n")) { "`r`n" } else { "`n" }
$insert = $anchor + $nl + $nl + '// hub queue message: coalescing key (optional) + ready text' + $nl + 'struct Msg {' + $nl + '    key: Option<String>,' + $nl + '    text: String,' + $nl + '}'
$t = $t.Replace($anchor, $insert)
[System.IO.File]::WriteAllText($p, $t, (New-Object System.Text.UTF8Encoding($false)))

# --- verify: struct present + brace balance intact ---
$t2 = [System.IO.File]::ReadAllText($p)
if (-not $t2.Contains('struct Msg')) {
    Write-Host '[HOTFIX] FAIL: insert did not apply - restore from the .bak-P63R2 backup'
    exit 1
}
$bal = 0
foreach ($ch in $t2.ToCharArray()) {
    if ($ch -eq '{') { $bal++ } elseif ($ch -eq '}') { $bal-- }
}
if ($bal -ne 0) {
    Write-Host ("[HOTFIX] FAIL: brace balance = {0} - restore from the .bak-P63R2 backup" -f $bal)
    exit 1
}
Write-Host '[HOTFIX] patched: struct Msg added (equivalent to P63 R2 content)'
Write-Host '[HOTFIX] RESULT: OK'
Write-Host ''
Write-Host 'Next: powershell -ExecutionPolicy Bypass -File .\build.ps1   # stop -> test (73) -> build -> deploy'
