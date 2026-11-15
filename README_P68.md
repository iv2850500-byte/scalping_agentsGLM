# README_P68 — аудит-пак (2026-11-13)

## Что это

Аддитивный пак аудита: документация + исследовательский конвейер пятого бота
(standalone) + QA-скрипт данных + эталонный прогон. Торговый код бота
НЕ затрагивается: ни один существующий файл не меняется, маркеры P64/P65/P66
остаются как были (P64=51 | P65=6 | P66=22).

## Установка

```powershell
Expand-Archive -Path .\Downloads\P68_audit_pack.zip -DestinationPath . -Force
powershell -ExecutionPolicy Bypass -File .\install_p68_audit.ps1
```

Инсталлер: копирует docs\ (6 файлов), создаёт research\spectrum_lab\,
кладёт scripts\data_qa_audit.py, проверяет компиляцию research-крейта
(`cargo check` в research\spectrum_lab — std-only, отдельная сборка,
памяти требует мало) и печатает сводку.

## Быстрый старт пятого бота (offline, безопасно)

```powershell
cd research\spectrum_lab
cargo build --release
.\target\release\spectrum_lab.exe --out ..\..\wfa_out\spectrum_run1 `
  --data ..\..\data\klines\TAOUSDT-1h-450d.csv `
  --data ..\..\data\klines\ETHUSDT-1h-full.csv `
  --data ..\..\data\klines\BTCUSDT-1h-full.csv
```

Эталонные результаты (этот же код + эти же данные, песочница аудита):
reference_runs\spectrum_report.txt — обе гипотезы (range_fade,
donchian_breakout) FAIL по OOS-гейтам; это зафиксированный отрицательный
результат, НЕ приглашение к перебору параметров.

## Состав

- docs\AUDIT_BASELINE.md — подтверждённая архитектура, данные, границы проверки
- docs\CHANGELOG_AUDIT.md — находки A-01…A-08 со статусами и путями исправления
- docs\STRATEGY_DISCOVERY.md — протокол и правила отбора гипотез
- docs\ENTRY_EXIT_RESEARCH.md — источники (с датами/статусами) и гипотезы
- docs\RUNBOOK_offline_paper_testnet.md — режимы запуска и критерии переходов
- docs\config.example.toml — пример конфигурации без секретов
- research\spectrum_lab\ — standalone крейт (src/main.rs, Cargo.toml)
- reference_runs\ — эталонный прогон P68
- scripts\data_qa_audit.py — QA CSV

## P69 (отдельный, опциональный)

P69_optional.zip — выравнивание Algo Order API с официальной документацией
(camelCase algoType/triggerPrice как дополнительная попытка в
place_protective_order). Ставить ТОЛЬКО вручную после чтения
P69_README.txt: требует последующей тестнет-проверки постановки защит.
Статус: ИСПРАВЛЕНО, НЕ ПРОВЕРЕНО.
