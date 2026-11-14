# AUDIT BASELINE — scalping_agents (P68, 2026-11-13)

Аудит выполнен в изолированной песочнице на пользовательском снапшоте
`src_2026-11-12.zip` (загружен 2026-11-12 09:05, pre-P64). Прямого доступа к
рабочей машине нет; канал доставки изменений — паки P67/P68. Файлы, изменённые
после снапшота (main.rs, app_error.rs, wfa_pipeline.rs, threshold_grid.rs,
wfa_grid.ps1, Cargo.toml — P64–P67), в песочнице отсутствуют и проверяются
только по отчётам пользователя (маркеры P64=51 | P65=6 | P66=22, 80/80 тестов,
release-сборка, exit-контракт P65). Торговое ядро P64–P67 не затрагивали.

## 1. Подтверждённая архитектура (по коду снапшота)

```
config.toml (боты) ──► main.rs: 1 TimeSync, 1 FilterRegistry(exchangeInfo, ретраи),
                       WebSocketHub(isolated/common), 1 GlobalRiskManager(P15 daily stop)
                       супервизор бота: вечный цикл рестартов (backoff 5с→300с→1800с)
run_bot_inner:
  BinanceFuturesClient ─► ResilientClient(retry+timeout+CB+strict, только get_positions)
  StateManager(SQLite WAL, positions/…) + TradeJournal + RiskManager(per-bot)
  UserStream(listenKey, ORDER_TRADE_UPDATE→broadcast) при futures&&!simulate
  kline_poller(REST, backfill 250) ─► hub ─► цикл select!:
      shutdown │ ws(свечи→exit-check→сигнал→risk.gates→execute) │ 5с position-check │ 30с лог/персист
  periodic reconciliation (300с, двойное подтверждение, purge/adopt + synthetic SL 2%)
ExecutionEngine: maker(GTX)/market входы, ensure_terminal/resolve_by_client_id,
  slippage guard 0.3% с встречным market, RE-ANCHOR SL/TP к факту,
  STOP_MARKET+TP_MARKET (Algo API → fallback классика), pre-entry reconcile, IP-ban gate
```

## 2. Стратегии (фабрика create_strategy → config.strategy.name)

| bot | стратегия | файл | вход/выход (подтверждено кодом) | таймфрейм |
|---|---|---|---|---|
| tao_momentum | `momentum` | strategies/momentum.rs | momentum+OFI+volume+EMA200+Supertrend+MFI+ROC+regime=Trending; SL/TP от ATR, TP cap 2% | 1h |
| btc_bounce | `bounce_trader` | bounce_trader.rs | RSI 30/70 + фрактальные уровни (5-бар подтверждение), SL за уровнем | 1h |
| inj_liquidity | `liquidity_grab` | liquidity_grab.rs | SMC: sweep pool → MSS → retest; LIMIT maker вход | 1h |
| gold_dca | `keltner_dca` | keltner_dca.rs | Keltner(20,2×ATR10) touch + EMA200, DCA 5 ордеров, Donchian TP/SL | по конфигу |

\* Актуальный config.toml с 4 ботами в снапшоте ОТСУТСТВУЕТ (есть только
config_tao_momentum_v3.toml и bots.toml bot_1) — активные параметры
четырёх ботов из песочницы НЕПРОВЕРЯЕМЫ; см. §5.

`momentum_v3.rs` (1020 строк) **не включён в strategies/mod.rs — мёртвый файл**;
активный momentum — momentum.rs («v3 FULL», все 6 патчей Фазы A внутри).

## 3. Данные и исследования (реально доступные)

| источник | объём | QA (P68, scripts/data_qa_audit.py) |
|---|---|---|
| TAOUSDT-1h-450d | 10799 бар / 449.9д | dups=0 gaps=0 ohlc_bad=0 |
| TAOUSDT-1h-full | 22526 бар / 938.5д | gaps=0, zero_vol=1 |
| ETHUSDT / BTCUSDT / SOLUSDT 1h | ~10770 бар / ~450д | gaps=1, ok |
| INJUSDT-1h | 3000 бар / 125д | чисто, но мало для WFA |

Исследования с артефактами: (а) P66 threshold grid TAO 450d (отчёт пользователя,
2026-11-13): 0/125 ok-строк, per-trade expectancy ≈ −0.16R при vol 1.1–1.5;
(б) P33 WFA momentum (exe, 6 прогонов): **n=0 сделок** на всех наборах —
research-параметры (min_atr=0.25 и пр.) несовместимы с live (min_atr=0.05);
(в) P68 spectrum_lab (этот аудит): range_fade и donchian_breakout — FAIL по
OOS-гейтам на TAO/ETH/BTC (см. STRATEGY_DISCOVERY).

## 4. Базовые проверки

- Сборка/тесты основного крейта в песочнице НЕ выполнялись (P64–P67 файлы
  отсутствуют). Последний подтверждённый пользователем результат: 80/80
  тестов, release, `cargo check --features wfa` (2026-11-12/13).
- spectrum_lab (std-only) собран и прогнан здесь: rustc 1.100.0, BUILD_OK,
  реальный прогон на 4 датасетах (reference_runs/ в паке P68).
- QA данных: scripts/data_qa_audit.py — все датасеты чистые.

## 5. Что НЕПРОВЕРЯЕМО из песочницы (предоставить для закрытия)

1. Текущие main.rs, app_error.rs, wfa_pipeline.rs, threshold_grid.rs,
   wfa_grid.ps1, Cargo.toml, supervisor.ps1, wfa_pipeline.ps1 (версии P67).
2. Рабочий config.toml с 4 ботами + .env-структура (БЕЗ секретов).
3. SQLite trade_journal за период (для корреляции доходностей стратегий —
   сейчас портфельный анализ только качественный).
4. Логи бота за 1–2 недели (частота reconcile-срывов, срабатывания защит).

## 6. Риски запуска

- Тестнет флаковый: positionRisk периодически 5xx (компенсировано
  ResilientClient), Algo API тестнета исторически требовал параметры в нижнем
  регистре — расхождение с официальными доками (CHANGELOG_AUDIT, A-04).
- `simulate=false` с валидными ключами = реальные ордера на тестнет-счёте;
  для offline-анализа использовать только research-бины.

## 7. Опасные команды, которые НЕ выполнялись

- Запросы к боевой Binance/Bybit API с торговыми правами.
- `cargo run --bin scalping_agents` (торговый процесс) в песочнице.
- Скачивание/запуск непроверенных бинарников; удаление бэкапов/данных.
