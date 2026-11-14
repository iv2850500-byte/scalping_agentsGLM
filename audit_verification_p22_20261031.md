# Верификационный отчёт аудита + фикс-пакет P22 — 31.10.2026

Документ-основание: «Ты — команда экспертов…» (аудит AVRORA, 5 блоков + roadmap 30/60/90).
Верификация выполнена по свежим исходникам `src/` (включая модули, которых аудит не видел:
`src/risk/*.rs`, `src/execution/*.rs`, `src/agents/`, `src/webui/`, `src/exchange/binance_futures.rs`,
`src/websocket/`).

## 1. Итог верификации ⚠️-пунктов аудита

| Пункт аудита | Вердикт | Доказательство в коде |
|---|---|---|
| Нет дневного лимита убытка портфеля | **СНЯТ — сделан (P15)** | `risk/global_risk_manager.rs:47-156`: `set_equity_base` + `load_persisted_day` (персист `data/p15_portfolio_day.txt`), гейт `entries_allowed()`, вызовы `add_closed_pnl` ×6 в main.rs (1143, 1248, 1281, 1469, 1506), арминг main.rs:271-277 |
| Нет kill-switch | **СНЯТ — сделан (P15+P11)** | `entries_allowed()` проверяет файл `KILL` (`global_risk_manager.rs:119`); helper `utils/killfile.rs` (P11). Файл KILL = «только выходы» |
| RECONCILE_STRICT=0 по умолчанию | **СНЯТ — сделан (P11/P15)** | `resilient_client.rs:115,126`: default STRICT, env-override; main.rs:686-697 — strict по non-testnet URL |
| Супервизор сдаётся после 3 попыток | **СНЯТ — сделан (AUDIT-P6/P15)** | main.rs:324-384: `MAX_ATTEMPTS=u32::MAX`, backoff 5/10с → 300с → 1800с (cap), сторож-баннер |
| webui несёт ключи | **СНЯТ** | `webui/mod.rs:20-33`: BotInfo — без Serialize; комментарий: «наружу через API не отдаются»; `/api/dashboard` (mod.rs:582-592) отдаёт name/symbol/strategy/market/simulate/interval/balance/positions/risk — ключей нет; POST-гейт WEBUI_TOKEN (AUDIT-P2, mod.rs:46-75) |
| Защитные ордера без reduceOnly/workingType | **СНЯТ — сделан (AUDIT-P8)** | `binance_futures.rs:230-237`: STOP_MARKET `workingType=MARK_PRICE&closePosition=true` (closePosition сильнее reduceOnly — не может открыть обратную позицию) |
| Неисполненный лимитник (GTX) | **СНЯТ** | `execution_engine.rs:346-470`: place_and_wait с таймаутом → cancel → перечитывание статуса → `resolve_order_fate` (частичные исполнения учтены, сетевые сбои — resolve_by_client_id) |
| Telegram-алерты | **СНЯТ частично (P11)** | `utils/telegram.rs`: закрытия сделок (trade_journal CLOSE, 30с-полл) + суспензии (risk_stats). Не было: UDS-lost, ExitUnconfirmed → **добавлено UDS в P22-C** |
| Усыновление с синтетическим SL 2%, не зная о живом биржевом стопе | **ПОДТВЕРЖДЁН** | main.rs:622-627 (старт) и 812-818 (период.): `EMERGENCY_SL_PCT=0.02`, биржевые ордера не читаются → **исправлено в P22-B** |
| Периодический reconciler молча пропускает тик при сбое get_positions | **ПОДТВЕРЖДЁН** | main.rs:778: `let Ok(all) = ... else { continue };` → **исправлено в P22-A** |
| Slippage/спред не моделируются | **ПОДТВЕРЖДЁН (отложено)** | `execution/slippage.rs` используется только в `bin/backtest.rs`; в боевом пути нет |
| Funding не входит в решение | **ПОДТВЕРЖДЁН (отложено)** | Funding только начисляется (`accrue_funding`, main.rs:700-758), в net_rr не входит |
| Сайзинг от статического initial_balance | **ПОДТВЕРЖДЁН частично** | main.rs:270: equity_base = Σ initial_balance; компромисс: на testnet баланс биржи фиктивен. P22-D даёт ручной `P15_EQUITY_OVERRIDE`; автосчитывание баланса — после перехода на реал |
| Fee-фолбэки расходятся | **ПОДТВЕРЖДЁН** | state_manager.rs:228-231 был 0.001, движок 0.0005 → **выровнено в P22-E** |
| Роутинг биржи по подстроке contains("bybit") | **ПОДТВЕРЖДЁН (отложено)** | main.rs:135,240,312,384,424 и др.; средняя критичность, эволюционный рефакторинг |

## 2. Состав P22

| # | Патч | Файлы |
|---|---|---|
| A | Не-тихий reconcile: warn + счётчик подряд + сообщение о восстановлении | main.rs |
| B | Перехват биржевого STOP_MARKET при усыновлении (roadmap #7): реальная цена стопа вместо синтетики 2%, sl_order_id фиксируется — без дубликатов стопа; в WARN-логе источник SL | main.rs, exchange/binance_futures.rs (parse stopPrice для STOP_MARKET) |
| C | UDS-алерты в Telegram: listenKeyExpired, потеря потока (≥6 неудач реконнекта, 1 раз на инцидент), восстановление | exchange/user_stream.rs |
| D | `P15_EQUITY_OVERRIDE` — ручной реальный капитал для дневного стопа | main.rs |
| E | Fee-фолбэк выровнен: futures 0.0005 / spot 0.00075 (как ExecutionEngine) | state/state_manager.rs |

Проверки сборки: все 9 замен — ровно 1 вхождение (CRLF); баланс скобок/скобочных пар сохранён;
CRLF не повреждён; diff-карта = только задуманные места; zip integrity OK.

**Пакет**: `scalping_fixpack_p22_20261031.zip`, 77 187 B,
SHA256 `ddc0d49208264570c107ca8377c0eaa2f78d6cc89624a47eeb27e61fabba44a5`

## 3. Что осталось из roadmap (честный список, не в P22)

1. Equity-сайзинг от реального баланса биржи (после решения о реале; на testnet баланс фиктивен).
2. Slippage-guard (bookTicker) + half-spread в net_rr.
3. Funding в net_rr для горизонтов ≥1 сутки.
4. Enum Exchange вместо contains("bybit"); выноски из main.rs; тесты денежной математики.
5. Walk-forward для bounce/liquidity/momentum (дни 31-60) — решает вердикты по стратегиям.
6. VPS/юрисдикция/ключи — вне кода, день 8 roadmap.
