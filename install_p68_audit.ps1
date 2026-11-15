# install_p68_audit.ps1 — аддитивный пак аудита (без правок торгового кода)
# P68/ doc-marker. Запуск: powershell -ExecutionPolicy Bypass -File .\install_p68_audit.ps1
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $root

Write-Host "=== P68 audit pack install ===" -ForegroundColor Cyan
Write-Host "root: $root"

# 1) docs
New-Item -ItemType Directory -Force -Path "docs" | Out-Null
# [D2c] self-copy no-op (source==dest; pack already unpacked to root)
Write-Host "[ok] docs\ -> AUDIT_BASELINE / STRATEGY_DISCOVERY / ENTRY_EXIT_RESEARCH / CHANGELOG_AUDIT / RUNBOOK / config.example"

# 2) research crate
New-Item -ItemType Directory -Force -Path "research\spectrum_lab\src" | Out-Null
if (-not (Test-Path "research\spectrum_lab\Cargo.toml")) { Copy-Item "$root\research\spectrum_lab\Cargo.toml" "research\spectrum_lab\" -Force } else { Write-Host "[skip] already in place: Cargo.toml" } # [D2d] idempotent
if (-not (Test-Path "research\spectrum_lab\src\main.rs")) { Copy-Item "$root\research\spectrum_lab\src\main.rs" "research\spectrum_lab\src\" -Force } else { Write-Host "[skip] already in place: main.rs" } # [D2d] idempotent
Write-Host "[ok] research\spectrum_lab\ (standalone, std-only)"

# 3) QA script + reference run
New-Item -ItemType Directory -Force -Path "scripts" | Out-Null
if ((Test-Path "$root\scripts\data_qa_audit.py") -and -not (Test-Path "scripts\data_qa_audit.py")) { Copy-Item "$root\scripts\data_qa_audit.py" "scripts\" -Force } else { Write-Host "[skip] data_qa_audit.py (already in place or absent in pack)" } # [D2d] idempotent
if (Test-Path "$root\reference_runs") { New-Item -ItemType Directory -Force -Path "docs\reference_runs_P68" | Out-Null; Copy-Item "$root\reference_runs\*" "docs\reference_runs_P68\" -Force } # [D2d] flat copy, no nesting
Write-Host "[ok] scripts\data_qa_audit.py + docs\reference_runs_P68"

# 4) compile check research crate (standalone; НЕ трогает основной крейт)
Push-Location "research\spectrum_lab"
try {
    cargo check 2>&1 | Select-Object -Last 5
    if ($LASTEXITCODE -eq 0) { Write-Host "[ok] cargo check spectrum_lab: PASS" -ForegroundColor Green }
    else { Write-Host "[WARN] cargo check вернул $LASTEXITCODE — соберите вручную: rustc --edition 2021 -O src\main.rs -o spectrum_lab.exe" -ForegroundColor Yellow }
} finally { Pop-Location }

Write-Host ""
Write-Host "=== P68 DONE. Торговый код не изменён; маркеры P64/P65/P66 не тронуты ===" -ForegroundColor Cyan
Write-Host "Следующий шаг: см. README_P68.md раздел 'Быстрый старт' (offline, безопасно)."
