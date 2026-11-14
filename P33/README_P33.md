# P33 — WFA momentum: ГОТОВЫЙ Windows-экзешник (без компиляции)

Дата: 2026-11-09. Пак P31 переехал в standalone-экзешник: `wfa_momentum.exe`
считает тот же WFA momentum-бэктест на тех же CSV и выдаёт те же числа 1:1.
Компилировать на твоей машине больше НИЧЕГО не нужно.

## (а) Зачем этот пак: у тебя rustc падает

P31 и P32 ты собирать не смог — на твоей машине rustc падает при сборке
`wfa_momentum`:

1. Сначала (P31, профиль с LTO): падение процесса rustc с кодом **0xc0000005**
   (access violation) на этапе LTO.
2. Затем (P32, профиль БЕЗ LTO, твой `p32_fail.log`, 2679 строк, cargo build
   --profile wfa --bin wfa_momentum): линковка уходит в LLVM и умирает по
   памяти — дословно из лога:

   ```
   rustc-LLVM ERROR: out of memory
   Allocation failed
   error: could not compile `scalping_agents` (bin "wfa_momentum")
   ```

   с кодом возврата `0xc0000409` на шаге линковки wfa_momentum (виноват не
   код P31/P33 — в P32 линковался весь lib-крейт `scalping_agents` с tokio,
   sqlx, reqwest, ring и пр., и rustc-LLVM не хватило памяти).

Поэтому P33 = тот же исходник WFA, собранный здесь в готовый `.exe`:
внешние зависимости = ноль (только std), память на сборку — не твоя
проблема. Числа те же, что в P31 (эталон совпадает 1:1, проверено).

## (б) Как запускать

В PowerShell:

```powershell
Expand-Archive -Path .\Downloads\P33_wfa_standalone_exe_20261109.zip -DestinationPath . -Force
cd .\P33
.\run_wfa_p33.ps1
```

Если Windows блокирует скрипт политикой выполнения (ExecutionPolicy), так:

```powershell
powershell -ExecutionPolicy Bypass -File .\run_wfa_p33.ps1
```

Скрипт сам прогонит все 6 эталонных прогонов, каждый сохранит в
`run_<имя>.log` (Tee), в конце напечатает сводную таблицу «получено vs
ожидается» и вернёт exit-код 0 (всё совпало) или 1 (расхождение). После
прогонов рядом появятся каталоги `run_taofull/`, `run_tao450/` и т.д. с
`summary.json`, `per_fold.csv`, `oos_trades_production.csv` — как в P31.

Запуск руками (если хочется конкретный прогон) — из каталога `P33`:

```powershell
.\wfa_momentum.exe --data .\data\TAOUSDT-1h-full.csv --out run_taofull
.\wfa_momentum.exe --data .\data\TAOUSDT-1h-450d.csv --out run_tao450
.\wfa_momentum.exe --data .\data\TAOUSDT-1h-450d.csv --fee 0.0006 --spread 0.0003 --out run_tao450_stress
.\wfa_momentum.exe --data .\data\BTCUSDT-1h-full.csv --out run_btc450
.\wfa_momentum.exe --data .\data\ETHUSDT-1h-full.csv --out run_eth450
.\wfa_momentum.exe --data .\data\SOLUSDT-1h-full.csv --out run_sol450
```

Это те же аргументы, что в командах P31 (там было `cargo run --release --bin
wfa_momentum -- ...`), только вместо cargo — готовый exe. Полный список
флагов: `.\wfa_momentum.exe --help`.

## (в) Эталонные числа (P31, обязаны совпасть 1:1)

PROD-конфиг v3 (rr=1.5) молчит на всех символах (prod-OOS n=0 — так и должно
быть, см. README_P31 «Главная находка»). Содержательная картина — плато
сеточных конфигов по OOS целиком:

| Прогон | Конфиг | n | WR | R | PF |
|---|---|---|---|---|---|
| run_taofull (2.5г, 28 фолдов) | rr2.0 | 159 | 31.4% | **-29.68** | 0.76 |
| run_taofull | rr2.5 | 162 | 24.1% | **-51.86** | 0.64 |
| run_tao450 (11 фолдов) | rr2.0 | 76 | 32.9% | -10.89 | 0.81 |
| run_tao450 | rr2.5 | 80 | 26.2% | -19.51 | 0.72 |
| run_tao450_stress (двойные издержки) | rr2.0 | 76 | 32.9%* | -13.17 | 0.78 |
| run_tao450_stress | rr2.5 | 80 | 26.2% | -22.51 | 0.68 |
| run_btc450 | rr2.0 | 74 | 28.4% | -20.62 | 0.66 |
| run_btc450 | rr2.5 | 76 | 22.4% | -28.85 | 0.58 |
| run_eth450 | rr2.0 | 54 | 25.9% | -19.02 | 0.58 |
| run_eth450 | rr2.5 | 54 | 20.4% | -24.27 | 0.51 |
| run_sol450 | rr2.0 | 56 | 33.9% | -6.24 | 0.85 |
| run_sol450 | rr2.5 | 57 | 33.3% | +0.22 | 1.01 |

\* Единственное отличие от компактной таблицы README_P31: там для
tao450_stress rr2.0 указан WR 30.3%, а строка плато в эталонном логе P31
(`run_tao450_stress.log`) печатает WR=32.9% (n/R/PF совпадают: 76/-13.17/0.78).
Сравнение в run_wfa_p33.ps1 идёт по n/R/PF, поэтому на WR это не влияет.

Сверка: `run_wfa_p33.ps1` парсит именно эти две строки из каждого лога и
сравнивает n/R/PF с таблицей выше. Любое расхождение — exit 1.

## (г) SmartScreen / Defender

Если Windows SmartScreen предупреждает о неизвестном издателе:

- кнопка **«Подробнее» → «Выполнить в любом случае»**, или
- PowerShell: `Unblock-File .\wfa_momentum.exe` (снимает маркер Zone.Identifier,
  который вешает браузер при скачивании).

exe собран здесь кросс-тулчейном llvm-mingw (clang 20.1.5 / LLVM lld +
mingw-w64 UCRT runtime), цель x86_64-pc-windows-gnu, rustc 1.99.1 — та же
версия компилятора, что у эталонных прогонов P31, без LTO: PE32+ console
x86-64, 1 315 328 байт. Компиляторный рантайм слинкован статически; динамические
импорты — только системные библиотеки Windows (KERNEL32.dll, ntdll.dll,
USERENV.dll, WS2_32.dll, bcryptprimitives.dll и UCRT api-ms-win-crt-*),
которые входят в Windows 10/11 (на 7/8.1 могло бы понадобиться UCRT-обновление,
но это не твой случай). Никуда не стучится: интернет не нужен, устанавливать
ничего не нужно, учёток/ключей не трогает. SHA256 exe:
`ec168e54a449ffe13f1653c52c68cbe3a85f35332d4c6ce02b39a9bc7c20e147`.

## (д) Приёмка

- Числа обязаны совпасть с эталоном 1:1. Расчёт детерминирован: f64/IEEE-754,
  bootstrap MT19937 с seed=42 — платформа и LTO на числа не влияют (в P31
  числа сверены с Python-референсом построчно, сделки 1:1).
- Валидация сборщика: Linux-native сборка ТОГО ЖЕ standalone-проекта (тот же
  wfa_momentum.rs, тот же rustc 1.99.1) дала 6 прогонов, у которых stdout-логи
  ПОБАЙТНО совпали с эталонными логами P31 (`run_taofull.log`, `run_tao450.log`,
  `run_tao450_stress.log`, `run_btc450.log`, `run_eth450.log`, `run_sol450.log`
  — diff пуст), а `summary.json` и `per_fold.csv` — побайтово идентичны
  эталонным артефактам P31 по всем 6 прогонам.
- Если у тебя числа разошлись — пришли все `run_*.log` (они лежат рядом со
  скриптом после прогона).

## (е) Что exe НЕ делает

Чистый offline-расчёт по CSV: читает указанный `--data` файл, пишет `--out`
каталог (summary.json / per_fold.csv / oos_trades_production.csv) и stdout.
Сеть, биржа, API-ключи, аккаунты — не трогает вообще (только std, никаких
сетевых крейтов). CSV-данные — те же, что в P31 (TAO full/450d) и P30b
(BTC/ETH/SOL full), провенанс см. VALIDATION_P31.txt и README_P31.

## Состав пакета

| Файл | Что это |
|---|---|
| wfa_momentum.exe | Готовый WFA-бинарник (Windows x64, PE32+, UCRT) |
| data/TAOUSDT-1h-full.csv | 22 526 баров, 2024-04-11 → 2026-11-06 UTC |
| data/TAOUSDT-1h-450d.csv | 10 799 баров, последние 450 дней TAO (точный хвост full) |
| data/BTCUSDT-1h-full.csv | 10 766 баров (данные P30b) |
| data/ETHUSDT-1h-full.csv | 10 776 баров (данные P30b) |
| data/SOLUSDT-1h-full.csv | 10 776 баров (данные P30b) |
| run_wfa_p33.ps1 | 6 эталонных прогонов + сверка чисел (ASCII-only) |
| README_P33.md | Этот файл |
| VALIDATION_P33.txt | Отчёт валидации сборки (хеши, прогоны, платформа) |
| SHA256SUMS.txt | Хеши всех файлов пакета |
