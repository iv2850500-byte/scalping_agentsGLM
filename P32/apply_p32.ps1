# P32 apply script: add a no-LTO cargo profile [profile.wfa] to Cargo.toml.
# Why: rustc.exe died with STATUS_ACCESS_VIOLATION (0xc0000005) 6/6 times while
# compiling bin wfa_momentum under [profile.release] (fat LTO, codegen-units=1).
# This is a known Windows/LLVM failure class, NOT a bug in the P31 code.
# The fix: a separate build profile without LTO for the research binary.
# [profile.release] is NOT touched, so the live bot build stays unchanged.
# Idempotent: safe to run multiple times.

$ErrorActionPreference = 'Stop'

# --- locate project root: cwd first, then parent of this script folder ---
$root = $null
foreach ($c in @((Get-Location).Path, (Split-Path -Parent $PSScriptRoot))) {
    if (Test-Path -LiteralPath (Join-Path $c 'Cargo.toml')) { $root = $c; break }
}
if (-not $root) {
    throw 'Cargo.toml not found. Run this from the project root (C:\Users\BASE_PC\scalping_agentsGLM).'
}
Set-Location -LiteralPath $root
Write-Host "[P32] project root: $root"

# --- sanity check: P31 files in place ---
$wfa = Join-Path $root 'src\bin\wfa_momentum.rs'
if (Test-Path -LiteralPath $wfa) {
    Write-Host '[P32] ok: src\bin\wfa_momentum.rs found (P31 installed).'
} else {
    Write-Warning '[P32] src\bin\wfa_momentum.rs NOT found. Install P31 first if you plan to run WFA.'
}

# --- patch Cargo.toml ---
$cargoToml = Join-Path $root 'Cargo.toml'
$tomlText = Get-Content -LiteralPath $cargoToml -Raw

if ($tomlText -match '(?m)^\s*\[profile\.wfa\]') {
    Write-Host '[P32] [profile.wfa] already present in Cargo.toml - nothing to patch.'
} else {
    $backup = "$cargoToml.bak_p32"
    Copy-Item -LiteralPath $cargoToml -Destination $backup -Force
    Write-Host '[P32] backup written: Cargo.toml.bak_p32'

    Add-Content -LiteralPath $cargoToml -Value ''
    Add-Content -LiteralPath $cargoToml -Value @'

# P32: no-LTO profile for research binaries (wfa_momentum).
# Fat LTO of [profile.release] crashes rustc.exe on this machine
# (STATUS_ACCESS_VIOLATION). WFA runtime speed is fine without LTO.
[profile.wfa]
inherits = "release"
lto = false
codegen-units = 16
'@
    Write-Host '[P32] appended [profile.wfa]: lto = false, codegen-units = 16.'
}

# --- verify patch landed ---
$check = Get-Content -LiteralPath $cargoToml -Raw
if ($check -notmatch '(?m)^\s*\[profile\.wfa\]') {
    throw '[P32] FAILED: [profile.wfa] not found after patch. Restore Cargo.toml.bak_p32 and patch manually (see README_P32.md).'
}

Write-Host ''
Write-Host '=== environment ==='
& cargo --version
& rustc --version
$os = Get-CimInstance Win32_OperatingSystem
$freeGb  = [math]::Round($os.FreePhysicalMemory / 1MB, 1)
$totalGb = [math]::Round($os.TotalVisibleMemorySize / 1MB, 1)
Write-Host ("RAM free/total GB: {0} / {1}" -f $freeGb, $totalGb)

Write-Host ''
Write-Host '[P32] done. Next step:  .\P32\run_wfa_p32.ps1'
Write-Host '[P32] note: the FIRST build under the new profile recompiles all dependencies once (several minutes). Normal.'
