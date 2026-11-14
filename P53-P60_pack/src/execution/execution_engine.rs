use crate::config::BotConfig;
use crate::exchange::ExchangeClient;
use crate::risk::GlobalRiskManager;
use crate::state::StateManager;
use crate::strategies::Signal;
use crate::exchange::symbol_filters::FilterRegistry;
use crate::exchange::traits::OrderResult;
use crate::exchange::user_stream::OrderEventsRx;
use anyhow::Result;
use rust_decimal::prelude::*;
use std::sync::Arc;

#[derive(Debug)]
pub enum ExecOutcome {
    Exit(f64),
    // ✅ P38/2c: полный выход исполнился ЧАСТИЧНО (maker-догон/маркет
    // заполнились не полностью). Остаток остаётся в state под управлением
    // (SL/TP на бирже не тронуты), PnL зафиксирован только по исполненному.
    ExitPartial(f64),
    ExitUnconfirmed,
    Entry { price: f64, qty: f64, order_id: u64 },
    Missed(String),
}

pub struct ExecutionEngine {
    config: BotConfig,
    state_manager: Arc<StateManager>,
    global_risk: Arc<GlobalRiskManager>,
    rest_client: Option<Arc<dyn ExchangeClient>>,
    simulate: bool,
    filters: Option<Arc<FilterRegistry>>,
    order_events: Option<OrderEventsRx>,
    last_close_attempt: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,

    // 🔧 ПАТЧ-СИРОТЫ: гейты входа по состоянию сети
    last_entry_net_fail: std::sync::Mutex<Option<std::time::Instant>>,
    net_fails: std::sync::Mutex<std::collections::VecDeque<std::time::Instant>>,
    entries_blocked_until: std::sync::Mutex<Option<std::time::Instant>>,

    // 🔧 АНТИ-БАН: throttle pre-entry reconcile (не чаще раза в 120с;
    // ✅ P38: комментарий приведён к фактическому значению ниже)
    last_reconcile_attempt: std::sync::Mutex<Option<std::time::Instant>>,
}

impl ExecutionEngine {
    pub fn new(
        config: &BotConfig,
        client: Arc<dyn ExchangeClient>,
        state_manager: Arc<StateManager>,
        global_risk: Arc<GlobalRiskManager>,
        filters: Option<Arc<FilterRegistry>>,
        order_events: Option<OrderEventsRx>,
    ) -> Self {
        Self {
            config: config.clone(),
            state_manager,
            global_risk,
            rest_client: Some(client),
            simulate: config.simulate,
            filters,
            order_events,
            last_close_attempt: std::sync::Mutex::new(std::collections::HashMap::new()),
            last_entry_net_fail: std::sync::Mutex::new(None),
            net_fails: std::sync::Mutex::new(std::collections::VecDeque::new()),
            entries_blocked_until: std::sync::Mutex::new(None),
            last_reconcile_attempt: std::sync::Mutex::new(None),
        }
    }

    // ============================================================
    // 🔧 ПАТЧ-СИРОТЫ: сетевые гейты входов.
    // ============================================================
    fn record_net_fail(&self) {
        let now = std::time::Instant::now();
        *self.last_entry_net_fail.lock().unwrap() = Some(now);

        let mut q = self.net_fails.lock().unwrap();
        q.push_back(now);
        while q.front().map_or(false, |t| now.duration_since(*t).as_secs() > 600) {
            q.pop_front();
        }
        if q.len() >= 5 {
            *self.entries_blocked_until.lock().unwrap() =
                Some(now + std::time::Duration::from_secs(900));
            tracing::error!(
                "[{}] 🌐 5 сетевых сбоев за 10 мин — ВХОДЫ ЗАБЛОКИРОВАНЫ на 15 мин",
                self.config.name
            );
        }
    }

    fn entries_allowed(&self) -> bool {
        self.entries_blocked_until
            .lock()
            .unwrap()
            .map_or(true, |t| std::time::Instant::now() >= t)
    }

    fn entry_backoff_active(&self) -> bool {
        self.last_entry_net_fail
            .lock()
            .unwrap()
            .map_or(false, |t| t.elapsed() < std::time::Duration::from_secs(180))
    }

    fn taker_fee(&self) -> f64 {
        if let Some(fee) = self.config.fee_rate {
            return fee.to_f64().unwrap_or(0.00075);
        }
        match self.config.market_type.as_str() {
            "futures" => 0.0005,
            _ => 0.00075,
        }
    }

    // ✅ P41/E14: maker_fee удалён (единственный потребитель был в мёртвой
    // maker-ветке SpreadCapture).

    fn apply_filters(&self, symbol: &str, price: f64, qty: f64) -> Option<(f64, f64)> {
        let f = self.filters.as_ref()?.get(symbol).ok()?;
        let p = Decimal::from_f64(price)?;
        let q = Decimal::from_f64(qty)?;
        Some((f.round_price(p).to_f64()?, f.floor_qty(q).to_f64()?))
    }

    // ============================================================
    // 🔧 ПАТЧ-СИРОТЫ: доводка ордера до терминального статуса.
    // ============================================================
    async fn ensure_terminal(
        &self,
        rest: &Arc<dyn ExchangeClient>,
        symbol: &str,
        order_id: u64,
        known_commission: f64,
    ) -> Result<OrderResult> {
        for attempt in 1..=5u32 {
            match rest.get_order_status(symbol, order_id).await {
                Ok(mut st) => {
                    if st.commission < known_commission {
                        st.commission = known_commission;
                    }
                    if matches!(st.status.as_str(), "FILLED" | "CANCELED" | "EXPIRED" | "REJECTED") {
                        return Ok(st);
                    }
                    tracing::warn!(
                        "[{}] ордер #{} ещё жив ({}) после таймаута — отменяю",
                        self.config.name, order_id, st.status
                    );
                    if let Err(ce) = rest.cancel_order(symbol, order_id).await {
                        tracing::warn!(
                            "[{}] отмена #{} (ensure_terminal) не удалась: {}",
                            self.config.name, order_id, ce
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "[{}] статус #{} недоступен ({}/5): {}",
                        self.config.name, order_id, attempt, e
                    );
                    self.record_net_fail();
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
        }

        let _ = rest.cancel_order(symbol, order_id).await;
        for attempt in 1..=5u32 {
            match rest.get_order_status(symbol, order_id).await {
                Ok(st) if matches!(st.status.as_str(), "FILLED" | "CANCELED" | "EXPIRED" | "REJECTED") => {
                    return Ok(st);
                }
                Ok(st) => {
                    tracing::warn!(
                        "[{}] ордер #{} всё ещё жив ({}) — отменяю повторно",
                        self.config.name, order_id, st.status
                    );
                    let _ = rest.cancel_order(symbol, order_id).await;
                }
                Err(e) => {
                    tracing::warn!(
                        "[{}] статус #{} недоступен (финал {}/5): {}",
                        self.config.name, order_id, attempt, e
                    );
                    self.record_net_fail();
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
        }

        Err(anyhow::anyhow!(
            "судьба ордера {} не выяснена после 10+ попыток — СВЕРЬ openOrders ВРУЧНУЮ",
            order_id
        ))
    }

    async fn resolve_by_client_id(
        &self,
        rest: &Arc<dyn ExchangeClient>,
        symbol: &str,
        client_id: &str,
    ) -> Option<OrderResult> {
        for attempt in 1..=6u32 {
            match rest.get_order_by_client_id(symbol, client_id).await {
                Ok(st) => {
                    if matches!(st.status.as_str(), "FILLED" | "CANCELED" | "EXPIRED" | "REJECTED") {
                        if st.executed_qty > 0.0 {
                            tracing::warn!(
                                "[{}] ⚠️ ордер client={} (id={}) исполнен ({:.5}) вопреки сбою POST — АДОПТИРУЮ как вход",
                                self.config.name, client_id, st.order_id, st.executed_qty
                            );
                            return Some(st);
                        }
                        tracing::info!(
                            "[{}] ордер client={} (id={}) мёртв ({}) — упущение подтверждено",
                            self.config.name, client_id, st.order_id, st.status
                        );
                        return None;
                    }
                    tracing::warn!(
                        "[{}] ордер client={} (id={}) жив ({}) — отменяю",
                        self.config.name, client_id, st.order_id, st.status
                    );
                    let _ = rest.cancel_order(symbol, st.order_id).await;
                }
                Err(e) => {
                    if e.to_string().contains("-2013") {
                        tracing::info!(
                            "[{}] ордер client={} не существует на бирже — POST не дошёл",
                            self.config.name, client_id
                        );
                        return None;
                    }
                    tracing::warn!(
                        "[{}] поиск client={} ({}/6): {}",
                        self.config.name, client_id, attempt, e
                    );
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
        }
        tracing::error!(
            "[{}] 🚨 CRITICAL: судьба client={} не выяснена за 6 попыток — \
             СВЕРЬ openOrders/positionRisk ВРУЧНУЮ",
            self.config.name, client_id
        );
        None
    }

    async fn place_and_wait<F, Fut>(
        &self,
        rest: &Arc<dyn ExchangeClient>,
        symbol: &str,
        place: F,
        timeout_ms: u64,
    ) -> Result<OrderResult>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<OrderResult>>,
    {
        let Some(rx_arc) = &self.order_events else {
            let order = place().await?;
            return self.wait_plain(rest, symbol, order.order_id, timeout_ms).await;
        };

        let mut rx = rx_arc.lock().await;
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms + 2000);

        let order = place().await?;

        let mut commission_sum: f64 = 0.0;
        loop {
            let Some(remain) = deadline.checked_duration_since(std::time::Instant::now()) else {
                drop(rx);
                return self
                    .ensure_terminal(rest, symbol, order.order_id, commission_sum)
                    .await;
            };
            match tokio::time::timeout(remain, rx.recv()).await {
                Ok(Ok(ev)) if ev.order_id == order.order_id => {
                    commission_sum += ev.commission;
                    if matches!(ev.status.as_str(), "FILLED" | "CANCELED" | "EXPIRED" | "REJECTED") {
                        let mut res = ev.to_order_result();
                        res.commission = commission_sum;
                        return Ok(res);
                    }
                    continue;
                }
                Ok(Ok(_)) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    drop(rx);
                    return self.wait_plain(rest, symbol, order.order_id, timeout_ms).await;
                }
                Err(_) => {
                    drop(rx);
                    return self
                        .ensure_terminal(rest, symbol, order.order_id, commission_sum)
                        .await;
                }
            }
        }
    }

    async fn wait_plain(
        &self,
        rest: &Arc<dyn ExchangeClient>,
        symbol: &str,
        order_id: u64,
        timeout_ms: u64,
    ) -> Result<OrderResult> {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        let mut consec_err = 0u32;
        loop {
            if std::time::Instant::now() >= deadline {
                return self.ensure_terminal(rest, symbol, order_id, 0.0).await;
            }
            let remain = deadline
                .checked_duration_since(std::time::Instant::now())
                .unwrap_or_default();
            let step = remain.min(std::time::Duration::from_millis(400));
            tokio::time::sleep(step).await;
            match rest.get_order_status(symbol, order_id).await {
                Ok(st) => {
                    consec_err = 0;
                    if matches!(st.status.as_str(), "FILLED" | "CANCELED" | "EXPIRED" | "REJECTED") {
                        return Ok(st);
                    }
                }
                Err(e) => {
                    consec_err += 1;
                    self.record_net_fail();
                    tracing::warn!(
                        "[{}] статус #{} недоступен ({}/10): {}",
                        self.config.name, order_id, consec_err, e
                    );
                    if consec_err >= 10 {
                        return self.ensure_terminal(rest, symbol, order_id, 0.0).await;
                    }
                }
            }
        }
    }

    // P42/D17: dead resolve helper removed - orphaned by P41/E14 maker cleanup (dead_code).

    // ✅ P41/E14: reduce_only=true для ВЫХОДОВ (place_market_order_reduce_only:
    // minNotional не действует на reduce-only — пыль закрывается ордером; при
    // дрейфе стейта выход ОТКЛОНИТСЯ (-2022), а не откроет противоположную
    // позицию). Для входов всегда false.
    async fn enter_market_confirmed(
        &self,
        rest: &Arc<dyn ExchangeClient>,
        signal: &Signal,
        qty: f64,
        reduce_only: bool,
    ) -> Result<OrderResult> {
        // ✅ P36/E5b: client id генерируется ДО POST и переживает сбой.
        // Раньше id рождался внутри замыкания и терялся: таймаут POST при
        // исполненном на бирже ордере = «СУДЬБА НЕИЗВЕСТНА», хотя ордер
        // находится по origClientOrderId одной ссылкой
        // (resolve_by_client_id, уже существовал для maker-пути).
        let client_id = self.new_client_id();
        let status = match self
            .place_and_wait(
                rest,
                &signal.symbol,
                || async {
                    if reduce_only {
                        rest.place_market_order_reduce_only(
                            &signal.symbol,
                            &signal.side,
                            qty,
                            Some(client_id.clone()),
                        )
                        .await
                    } else {
                        rest.place_market_order(
                            &signal.symbol,
                            &signal.side,
                            qty,
                            Some(client_id.clone()),
                        )
                        .await
                    }
                },
                3000,
            )
            .await
        {
            Ok(s) => s,
            Err(e) => {
                // ✅ P35/E5a: отказ БИРЖИ ≠ сетевой сбой. Ошибки place() от
                // Binance-клиента содержат "Binance" ("Binance Futures error: …",
                // "Binance Futures защитный ордер: …"); ResilientClient
                // делегирует place_* без retry и без переписывания текста.
                // Раньше ЛЮБОЙ отказ (-2019 margin, -1111 precision…) безусловно
                // заворачивался в СУДЬБА НЕИЗВЕСТНА, ветки contains("Binance")
                // в execute_real были мертвы, и счётчик сетевых сбоев рос от
                // отказов биржи → ложная блокировка входов на 15 мин.
                // Временная мера (подстрока); постоянная — типизированные
                // ошибки ExchangeReject/Transport (план P36).
                let m = e.to_string();
                if m.contains("Binance") {
                    return Err(e);
                }
                // ✅ P36/E5b: таймаут/сеть ≠ потеря ордера. POST мог дойти и
                // исполниться — разрешаем по client id: исполнен → АДОПТИРУЕМ
                // как вход (позиция зарегистрируется с фактическим объёмом),
                // мёртв/-2013 → честное «упущено» вместо СУДЬБА НЕИЗВЕСТНА.
                if let Some(st) = self
                    .resolve_by_client_id(rest, &signal.symbol, &client_id)
                    .await
                {
                    return Ok(st);
                }
                return Err(anyhow::anyhow!("СУДЬБА НЕИЗВЕСТНА: {}", m));
            }
        };

        if status.status == "FILLED" || status.executed_qty > 0.0 {
            if status.executed_qty < qty {
                tracing::warn!(
                    "[{}] market исполнен частично: {:.5} из {:.5}",
                    self.config.name, status.executed_qty, qty
                );
            }
            return Ok(status);
        }
        Err(anyhow::anyhow!(
            "market-ордер {} не подтверждён (статус {})",
            signal.symbol, status.status
        ))
    }

    async fn recover_commissions(
        &self,
        rest: &Arc<dyn ExchangeClient>,
        symbol: &str,
        order_ids: &[u64],
    ) {
        for oid in order_ids {
            match rest.get_order_commission(symbol, *oid).await {
                Ok(c) if c > 0.0 => {
                    self.state_manager.add_fees_paid(symbol, c);
                    tracing::info!(
                        "[{}] 🩹 докапчивание: комиссии #{} = {:.6} (из income)",
                        self.config.name, oid, c
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(
                        "[{}] докапчивание #{} недоступно: {}",
                        self.config.name, oid, e
                    );
                }
            }
        }
    }

    // ============================================================
    // 🔧 ЭТАП B: снять биржевые защитные ордера закрытой позиции.
    // ✅ P35/H1: ids принимаются ПАРАМЕТРОМ. Раньше функция читала
    // get_protective_ids ПОСЛЕ close_position, который уже удалил позицию
    // из DashMap → (None, None) → отмен не было → STOP/TP оставались
    // сиротами на бирже.
    // ✅ P35/E8b: «уже ок» — только для терминальных статусов; раньше ЛЮБОЙ
    // статус логировался как «уже ок», включая NEW/PARTIALLY_FILLED (ордер
    // жив!). Ok(false) (алго-ордер не найден ни в классике, ни в Algo API)
    // больше не логируется как «снят» — идёт в ту же проверку статуса.
    // FINISHED — терминальный статус алго-защит (Algo API).
    // ============================================================
    async fn remove_protective_orders(&self, symbol: &str, ids: (Option<String>, Option<String>)) {
        if let Some(rest) = &self.rest_client {
            for (tag, id_str) in [("SL", ids.0), ("TP", ids.1)] {
                if let Some(s) = id_str {
                    let oid: u64 = s.parse().unwrap_or(0);
                    if oid > 0 {
                        let outcome = rest.cancel_order(symbol, oid).await;
                        let why = match outcome {
                            Ok(true) => {
                                tracing::info!(
                                    "[{}] 🧹 защитный {} #{} снят",
                                    self.config.name, tag, oid
                                );
                                continue;
                            }
                            Ok(false) => "не найден при отмене (классика+Algo)".to_string(),
                            Err(e) => e.to_string(),
                        };
                        match rest.get_order_status(symbol, oid).await {
                            Ok(st) if matches!(
                                st.status.as_str(),
                                "CANCELED" | "EXPIRED" | "FILLED" | "REJECTED" | "FINISHED"
                            ) => tracing::warn!(
                                "[{}] защитный {} #{} уже {} — ок",
                                self.config.name, tag, oid, st.status
                            ),
                            Ok(st) => tracing::error!(
                                "[{}] 🚨 защитный {} #{} ЖИВ (статус {}), отмена не удалась: {}",
                                self.config.name, tag, oid, st.status, why
                            ),
                            Err(_) => tracing::error!(
                                "[{}] 🚨 не снять защитный {} #{}: {}",
                                self.config.name, tag, oid, why
                            ),
                        }
                    }
                }
            }
        }
    }

    // ============================================================
    // 🔧 ЭТАП B: поставить защитные ордера после входа.
    // ============================================================
    async fn place_protective_orders(
        &self,
        rest: &Arc<dyn ExchangeClient>,
        signal: &Signal,
        entry_order_id: u64,
    ) {
        let close_side = if signal.side == "BUY" { "SELL" } else { "BUY" };

        // P18: protective triggers must be multiples of tickSize, otherwise
        // -1111 rejects them (TAOUSDT 2026-10-18: SL/TP rejected 3x, position
        // 110+ sec unprotected). Decimal round to nearest tick, no float dust.
        let p18_tick = |v: f64| -> f64 {
            match self.filters.as_ref().and_then(|r| r.get(&signal.symbol).ok()) {
                Some(f) => match Decimal::from_f64(v) {
                    Some(d) => f.round_price(d).to_f64().unwrap_or(v),
                    None => v,
                },
                None => v,
            }
        };
        let sl_f = signal.stop_loss.map(|v| p18_tick(v));
        let tp_f = signal.take_profit.map(|v| p18_tick(v));

        if sl_f != signal.stop_loss || tp_f != signal.take_profit {
            tracing::info!(
                "[{}] P18: protective triggers tick-aligned: sl {:?} -> {:?}, tp {:?} -> {:?}",
                self.config.name, signal.stop_loss, sl_f, signal.take_profit, tp_f
            );
        }

        if let Some(sl) = sl_f {
            let mut placed = false;
            for attempt in 1..=2u32 {
                                // 🔧 Если shutdown уже закрыл позицию — защиты не нужны
                if self.state_manager.get_positions().is_empty() {
                    tracing::warn!(
                        "[{}] позиция уже закрыта (shutdown) — прерываю постановку защит",
                        self.config.name
                    );
                    return;
                }
                match rest
                    .place_protective_order(
                        &signal.symbol,
                        &close_side,
                        true,
                        sl,
                        Some(format!("pr_sl_{}", entry_order_id)),
                    )
                    .await
                {
                    Ok(o) => {
                        self.state_manager
                            .set_sl_order_id(&signal.symbol, o.order_id.to_string())
                            .await;
                        tracing::info!(
                            "[{}] 🛡 биржевой STOP_MARKET #{} @ {:.8} поставлен",
                            self.config.name, o.order_id, sl
                        );
                        placed = true;
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[{}] 🛡 STOP_MARKET попытка {}/2 не удалась: {}",
                            self.config.name, attempt, e
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(
                            1000 * attempt as u64,
                        ))
                        .await;
                    }
                }
            }
            if !placed {
                tracing::error!(
                    "[{}] 🚨 позиция БЕЗ биржевого стопа — работает только софт-защита",
                    self.config.name
                );
            }
        }

        if let Some(tp) = tp_f {
            let mut placed = false;
            for attempt in 1..=2u32 {
                                // 🔧 Если shutdown уже закрыл позицию — защиты не нужны
                if self.state_manager.get_positions().is_empty() {
                    tracing::warn!(
                        "[{}] позиция уже закрыта (shutdown) — прерываю постановку защит",
                        self.config.name
                    );
                    return;
                }
                match rest
                    .place_protective_order(
                        &signal.symbol,
                        &close_side,
                        false,
                        tp,
                        Some(format!("pr_tp_{}", entry_order_id)),
                    )
                    .await
                {
                    Ok(o) => {
                        self.state_manager
                            .set_tp_order_id(&signal.symbol, o.order_id.to_string())
                            .await;
                        tracing::info!(
                            "[{}] 🛡 биржевой TAKE_PROFIT_MARKET #{} @ {:.8} поставлен",
                            self.config.name, o.order_id, tp
                        );
                        placed = true;
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[{}] 🛡 TP_MARKET попытка {}/2 не удалась: {}",
                            self.config.name, attempt, e
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(
                            1000 * attempt as u64,
                        ))
                        .await;
                    }
                }
            }
            if !placed {
                tracing::error!(
                    "[{}] 🚨 позиция БЕЗ биржевого TP — работает только софт-TP",
                    self.config.name
                );
            }
        }
    }

    // ✅ P41/E14: exit_allows_maker удалён вместе с мёртвым maker-выходом
    // (origin "SpreadCapture*" недостижим — стратегии нет в фабрике с P15).

    async fn close_position_real(
        &self,
        rest: &Arc<dyn ExchangeClient>,
        signal: &Signal,
    ) -> Option<f64> {
        // anti-hammer
        // ✅ P35/X2: Instant (монотонные часы) вместо SystemTime. Шаг
        // системных часов назад раньше делал now-t отрицательным (< 5000) →
        // ВСЕ выходы блокировались, пока настенные часы не догонят
        // сохранённую метку.
        // ✅ P41/E14: throttle 5с больше НЕ действует на аварийные выходы
        // (Exit_StopLoss/Exit_Panic/Exit_Shutdown_Market) — гасить стоп/панику
        // из-за кулдауна = держать рискованную позицию дольше, чем нужно.
        let is_emergency_exit = matches!(
            signal.strategy_name.as_str(),
            "Exit_StopLoss" | "Exit_Panic" | "Exit_Shutdown_Market"
        );
        if !is_emergency_exit {
            let key = signal.symbol.clone();
            let mut map = self.last_close_attempt.lock().unwrap();
            if let Some(t) = map.get(&key) {
                if t.elapsed() < std::time::Duration::from_secs(5) {
                    tracing::debug!(
                        "[{}] закрытие {}: throttle — менее 5с с прошлой попытки",
                        self.config.name, key
                    );
                    return None;
                }
            }
            map.insert(key, std::time::Instant::now());
        }

        // ✅ P41/E14: fail-open для выходов сохранён СОЗНАТЕЛЬНО (пропуск
        // закрытия опаснее неотфильтрованного объёма), но теперь громкий:
        // тихий unwrap_or маскировал отсутствие фильтров символа.
        // Отдельно: qty=0 после floor больше не уходит в «ПЫЛЬ»-гашение —
        // обработчики ниже закрывают исходным объёмом или отказываются явно.
        if signal.quantity <= 0.0 {
            tracing::warn!(
                "[{}] P41/E14: выход {} с quantity={} <= 0 — нечего закрывать",
                self.config.name, signal.symbol, signal.quantity
            );
            return None;
        }
        let qty = match self.apply_filters(&signal.symbol, signal.price, signal.quantity) {
            Some((_, q)) if q > 0.0 => q,
            Some((_, q)) if q <= 0.0 => {
                tracing::warn!(
                    "[{}] P41/E14: объём {} стал 0 после floor — закрываю исходным {}",
                    self.config.name, signal.quantity, signal.quantity
                );
                signal.quantity
            }
            Some((_, q)) => q,
            None => {
                tracing::warn!(
                    "[{}] P41/E14: нет фильтров для {} — выход исходным объёмом {} (fail-open выхода)",
                    self.config.name, signal.symbol, signal.quantity
                );
                signal.quantity
            }
        };

        // ✅ P41/E14: ветка «ПЫЛЬ» удалена. Она гасила позицию В STATE БЕЗ
        // ордера при qty*price < minNotional, а биржа при reduceOnly=true
        // minNotional НЕ проверяет (-4164: «unless you choose reduce only»).
        // Старая ветка давала фантом: state забывал позицию, биржа держала её.
        // Пыль теперь закрывается обычным reduce-only маркетом ниже.

        // ✅ P41/E14: maker-выход SpreadCapture удалён как мёртвая ветка (~200 строк):
        // стратегия SpreadCapture отсутствует в фабрике с P15, origin
        // "SpreadCapture"/"SpreadCapture_MOMENTUM" в позициях появиться не может,
        // значит use_maker_exit был всегда false и путь был недостижим.
        // Выходы теперь всегда идут reduce-only маркетом (блок ниже).

        // ✅ SL/Timeout — reduce-only маркетом, без GTX (P41/E14: пыль и
        // дрейф-защита, см. enter_market_confirmed)
        match self.enter_market_confirmed(rest, signal, qty, true).await {
            Ok(s) => {
                // ✅ P38/2c (полная семантика): фактический fill выхода может
                // быть меньше запрошенного — закрываем в state по ФАКТУ,
                // остаток остаётся под управлением (SL/TP не тронуты).
                let filled = if s.executed_qty > 0.0 { s.executed_qty.min(qty) } else { qty };
                if s.commission > 0.0 {
                    self.state_manager
                        .add_fees_paid(&signal.symbol, s.commission);
                }
                let exit_price = if s.price > 0.0 { s.price } else { signal.price };
                let pnl = self
                    .state_manager
                    .close_position(&signal.symbol, exit_price, filled)
                    .await;
                if s.commission <= 0.0 {
                    self.recover_commissions(rest, &signal.symbol, &[s.order_id]).await;
                }
                Some(pnl)
            }
            Err(e) => {
                tracing::error!(
                    "[{}] ❌ закрытие {} не подтверждено: {} — позиция остаётся, повтор (throttled)",
                    self.config.name, signal.symbol, e
                );
                None
            }
        }
    }

    fn new_client_id(&self) -> String {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("sa{}x{}", ms, n)
    }

    pub async fn execute(&self, signal: Signal) -> Result<ExecOutcome> {
        if signal.strategy_name.starts_with("Exit_") {
            // ============================================================
            // 🔧 ЭТАП B: синхронизация биржевого стопа (подтяжка).
            // ============================================================
            if signal.strategy_name == "Exit_UpdateSL" {
                // ✅ P35/E6a: в simulate биржевых защит нет — не отправляем
                // реальные cancel/place. Раньше ветка шла мимо simulate-гейта:
                // после real-прогона и рестарта с simulate=true и той же БД
                // позиции восстанавливались со sl_order_id, и ветка ставила
                // РЕАЛЬНЫЕ ордера (сироты на тестнете/бирже).
                if self.simulate {
                    return Ok(ExecOutcome::Missed("Exit_UpdateSL пропущен (simulate)".into()));
                }
                if let Some(rest) = &self.rest_client {
                    let (sl_id, _tp_id) = self.state_manager.get_protective_ids(&signal.symbol);
                    // P18: trail trigger must be a tick multiple, otherwise -1111
                    // kills BOTH stops: the old one is already cancelled here.
                    let new_sl = match self
                        .filters
                        .as_ref()
                        .and_then(|r| r.get(&signal.symbol).ok())
                        .and_then(|f| {
                            Decimal::from_f64(signal.price)
                                .map(|d| f.round_price(d).to_f64().unwrap_or(signal.price))
                        })
                    {
                        Some(v) if v > 0.0 => v,
                        _ => signal.price,
                    };
                    if (new_sl - signal.price).abs() > f64::EPSILON {
                        tracing::info!(
                            "[{}] P18: UpdateSL trigger {} -> {} (tickSize)",
                            self.config.name, signal.price, new_sl
                        );
                    }
                    if let Some(old_id_str) = sl_id {
                        let old_id: u64 = old_id_str.parse().unwrap_or(0);
                        if old_id > 0 {
                            let _ = rest.cancel_order(&signal.symbol, old_id).await;
                        }
                    }
                    let close_side = signal.side.clone();
                    match rest
                        .place_protective_order(
                            &signal.symbol,
                            &close_side,
                            true,
                            new_sl,
                            Some(format!(
                                "pr_sl_t{}",
                                chrono::Utc::now().timestamp_millis()
                            )),
                        )
                        .await
                    {
                        Ok(o) => {
                            self.state_manager
                                .set_sl_order_id(&signal.symbol, o.order_id.to_string())
                                .await;
                            tracing::info!(
                                "[{}] 🎗 биржевой стоп подтянут: #{} → {:.8}",
                                self.config.name, o.order_id, new_sl
                            );
                        }
                        Err(e) => {
                            tracing::error!(
                                "[{}] 🚨 подтяжка стопа не удалась ({}): повторная попытка",
                                self.config.name, e
                            );
                            if let Ok(o) = rest
                                .place_protective_order(
                                    &signal.symbol, &close_side, true, new_sl, None,
                                )
                                .await
                            {
                                self.state_manager
                                    .set_sl_order_id(&signal.symbol, o.order_id.to_string())
                                    .await;
                                tracing::warn!(
                                    "[{}] 🛡 стоп поставлен со 2-й попытки #{}",
                                    self.config.name, o.order_id
                                );
                            } else {
                                tracing::error!(
                                    "[{}] 🚨🚨 позиция без биржевого стопа после подтяжки — \
                                     работает софт-стоп; СВЕРИТЬ openOrders",
                                    self.config.name
                                );
                            }
                        }
                    }
                }
                return Ok(ExecOutcome::Missed("Exit_UpdateSL выполнен".into()));
            }

            if self.simulate {
                let pnl = self
                    .state_manager
                    .close_position(&signal.symbol, signal.price, signal.quantity)
                    .await;
                if !signal.strategy_name.starts_with("Exit_Partial") {
                    self.global_risk
                        .unregister_position(&self.config.name, &signal.symbol);
                }
                return Ok(ExecOutcome::Exit(pnl));
            }
            if let Some(rest) = &self.rest_client {
                // ✅ P35/H1: ids защит снимаем ДО close_position_real —
                // закрытие стирает позицию из state, после чего ids уже
                // не прочитать (сироты STOP/TP на бирже).
                let prot_ids = self.state_manager.get_protective_ids(&signal.symbol);
                let is_partial = signal.strategy_name.starts_with("Exit_Partial");
                return match self.close_position_real(rest, &signal).await {
                    Some(pnl) => {
                        // ✅ P35/H3: защиты снимаем только если позиция закрыта
                        // ЦЕЛИКОМ. Раньше remove_protective_orders звался
                        // безусловно: при частичном закрытии (maker-остаток,
                        // Exit_Partial) он срывал SL/TP у ЖИВОГО остатка.
                        let residual = self.state_manager.has_position(&signal.symbol);
                        if !is_partial && !residual {
                            self.remove_protective_orders(&signal.symbol, prot_ids).await;
                            self.global_risk
                                .unregister_position(&self.config.name, &signal.symbol);
                        }
                        // ✅ P38/2c: полный выход исполнился ЧАСТИЧНО — новый
                        // исход: main не включает кулдаун и не считает позицию
                        // закрытой, остаток продолжает управляться.
                        if !is_partial && residual {
                            Ok(ExecOutcome::ExitPartial(pnl))
                        } else {
                            Ok(ExecOutcome::Exit(pnl))
                        }
                    }
                    None => Ok(ExecOutcome::ExitUnconfirmed),
                };
            }
            return Ok(ExecOutcome::Exit(0.0));
        }

        if self.simulate {
            self.execute_simulated(signal).await
        } else {
            self.execute_real(signal).await
        }
    }

    async fn execute_simulated(&self, signal: Signal) -> Result<ExecOutcome> {
        // ✅ P56/S11-4: паритет с execute_real — симулятор тоже отбраковывает
        // сигналы с рельсами не на той стороне (иначе sim-прогон расходится
        // с реалом на «мгновенных» стопах).
        if let Err(why) = crate::utils::math::validate_rails(
            &signal.side,
            signal.price,
            signal.stop_loss,
            signal.take_profit,
        ) {
            tracing::error!("[{}] 🚨 {} (sim) — вход ОТКЛОНЁН", self.config.name, why);
            return Ok(ExecOutcome::Missed(why));
        }
        let sl_str = signal
            .stop_loss
            .map(|v| format!("{:.8}", v))
            .unwrap_or("—".into());
        let tp_str = signal
            .take_profit
            .map(|v| format!("{:.8}", v))
            .unwrap_or("—".into());

        let risk = signal.quantity
            * (signal.price - signal.stop_loss.unwrap_or(signal.price)).abs();
        let expected_profit = signal.quantity
            * (signal.take_profit.unwrap_or(signal.price) - signal.price).abs();

        let fee_rate = self.taker_fee();
        let expected_fees = signal.quantity * signal.price * fee_rate * 2.0;
        let net_profit = expected_profit - expected_fees;

        if expected_profit <= expected_fees {
            tracing::warn!(
                "⚠️ [СИМУЛЯЦИЯ] {}: движение {:.6} ≤ комиссий {:.6} — вход отклонён",
                signal.symbol, expected_profit, expected_fees
            );
            return Ok(ExecOutcome::Missed("sim: движение ≤ комиссий".into()));
        }

        let rr = if risk > 0.0 { expected_profit / risk } else { 0.0 };
        let net_rr = if risk > 0.0 {
            net_profit / (risk + expected_fees)
        } else {
            0.0
        };

        tracing::info!("════════════════════════════════════════════════════════");
        tracing::info!(
            "📝 [{}] [СИМУЛЯЦИЯ] {} {:.5} {} @ {:.8}",
            self.config.name, signal.side, signal.quantity, signal.symbol, signal.price
        );
        tracing::info!("   🛑 SL: {} | 🎯 TP: {}", sl_str, tp_str);
        tracing::info!(
            "   ⚠️ Риск: {:.4} USDT | 💰 брутто: {:.4} | fee({:.4}%): {:.4} | нетто: {:.4}",
            risk, expected_profit, fee_rate * 100.0, expected_fees, net_profit
        );
        tracing::info!("   📊 gross R:R = {:.2} | net R:R = {:.2}", rr, net_rr);
        tracing::info!("════════════════════════════════════════════════════════");

        // AUDIT-P3: без SL регистрируем консервативные 2% от номинала (не 0).
        let sl_ref = signal.stop_loss.unwrap_or(signal.price * 0.98);
        let risk_usdt = signal.quantity * (signal.price - sl_ref).abs();

        if self.state_manager.has_position(&signal.symbol) {
            // ✅ P39/H12: лестница в симуляторе — та же семантика дозакупки
            self.state_manager
                .add_dca_order(
                    &signal.symbol,
                    &signal.side,
                    signal.price,
                    signal.quantity,
                    signal.stop_loss,
                    signal.take_profit,
                    signal.strategy_name.clone(),
                    // AUDIT-P7: per-unit risk трейлинга — как в P3: нет SL = 2% (не 0).
                    (signal.price - signal.stop_loss.unwrap_or(signal.price * 0.98)).abs(),
                    signal.atr_hint,
                    signal.entry_spread
                )
                .await;
            let total_qty = self
                .state_manager
                .get_positions()
                .iter()
                .find(|p| p.symbol == signal.symbol)
                .map(|p| p.quantity)
                .unwrap_or(signal.quantity);
            self.global_risk
                .register_position(
                    &self.config.name,
                    &signal.symbol,
                    total_qty
                        * (signal.price - signal.stop_loss.unwrap_or(signal.price * 0.98)).abs(),
                );
        } else {
            self.global_risk
                .register_position(&self.config.name, &signal.symbol, risk_usdt);
            self.state_manager
                .add_position_with_risk(
                    signal.symbol.clone(),
                    signal.side.clone(),
                    signal.price,
                    signal.quantity,
                    signal.stop_loss,
                    signal.take_profit,
                    signal.strategy_name.clone(),
                    // AUDIT-P7: per-unit risk трейлинга — как в P3: нет SL = 2% (не 0).
                    (signal.price - signal.stop_loss.unwrap_or(signal.price * 0.98)).abs(),
                    signal.atr_hint,
                    signal.entry_spread
                )
                .await;
        }

        Ok(ExecOutcome::Entry { price: signal.price, qty: signal.quantity, order_id: 0 })
    }

    async fn execute_real(&self, signal: Signal) -> Result<ExecOutcome> {
        // ============================================================
        // 🔧 ПАТЧ-СИРОТЫ: гейты входа.
        // ============================================================
        if !self.entries_allowed() {
            return Ok(ExecOutcome::Missed(
                "gate: сетевые сбои — входы заблокированы на 15 мин".into(),
            ));
        }
        // ✅ P54/HALT-4: ЖЁСТКИЙ HALT портфеля — оборона в глубину. Main loop
        // тоже гейтится (P15), но движок — последняя инстанция перед POST:
        // KILL-file / дневной стоп портфеля / fail-closed персиста.
        if let Err(reason) = self.global_risk.entries_allowed() {
            return Ok(ExecOutcome::Missed(format!("HARD HALT: {}", reason)));
        }
        if self.entry_backoff_active() {
            return Ok(ExecOutcome::Missed(
                "backoff 60с после сетевого сбоя входа".into(),
            ));
        }

        // ✅ P41/E14: паритет с execute_simulated — вход с движением ≤
        // комиссий отклоняем ДО отправки на биржу (в симе проверка была,
        // в реале нет — живое поведение расходилось с моделью).
        {
            let fee_rate = self.taker_fee();
            let expected_profit = signal.quantity
                * (signal.take_profit.unwrap_or(signal.price) - signal.price).abs();
            let expected_fees = signal.quantity * signal.price * fee_rate * 2.0;
            if expected_profit <= expected_fees {
                tracing::warn!(
                    "[{}] движение {:.6} ≤ комиссий {:.6} — вход отклонён (P41/E14)",
                    self.config.name, expected_profit, expected_fees
                );
                return Ok(ExecOutcome::Missed("движение ≤ комиссий".into()));
            }
        }

        // ✅ P56/S11-3: рельсы защитных уровней — ДО ордера. Стоп не на той
        // стороне / не конечный / равен входу = мгновенный ложный SL/TP.
        if let Err(why) = crate::utils::math::validate_rails(
            &signal.side,
            signal.price,
            signal.stop_loss,
            signal.take_profit,
        ) {
            tracing::error!(
                "[{}] 🚨 {} | {} {:.5} @ {:.8} SL={:?} TP={:?} — вход ОТКЛОНЁН",
                self.config.name, why, signal.side, signal.quantity, signal.price,
                signal.stop_loss, signal.take_profit
            );
            return Ok(ExecOutcome::Missed(why));
        }

        let (safe_price, safe_qty) = match self
            .apply_filters(&signal.symbol, signal.price, signal.quantity)
        {
            Some(v) => v,
            None => {
                tracing::error!(
                    "[{}] нет фильтров для {} — ордер отклонён (fail-safe)",
                    self.config.name, signal.symbol
                );
                return Ok(ExecOutcome::Missed("нет фильтров символа".into()));
            }
        };

        if safe_qty <= 0.0 {
            tracing::error!("Объём стал 0 после округления: {}", signal.quantity);
            return Ok(ExecOutcome::Missed("qty=0 после фильтров".into()));
        }

        if let Some(f) = self.filters.as_ref().and_then(|r| r.get(&signal.symbol).ok()) {
            let notional = safe_price * safe_qty;
            let min_n = f.min_notional.to_f64().unwrap_or(0.0);
            if notional < min_n {
                tracing::warn!(
                    "[{}] нотационал {:.4} < min {} — вход отклонён",
                    self.config.name, notional, min_n
                );
                return Ok(ExecOutcome::Missed(format!("notional {:.4} < min {}", notional, min_n)));
            }
        }

        let rest = self
            .rest_client
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("REST not init"))?;

        // ✅ P39/H12: флаг дозакупки фиксируем ДО ордера. Лестница keltner_dca
        // (can_dca=true) шлёт BUY при уже открытой позиции — это очередная
        // транша, а не новый вход; путь определяем заранее.
        let was_dca = self.state_manager.has_position(&signal.symbol);

        // ============================================================
        // 🔧 АНТИ-БАН: pre-entry reconcile не чаще раза в 120с (✅ P38).
        // При пачке сигналов (как 19:03–19:05) не штормим биржу
        // проверками позиции — это и накопило -1003 бан.
        // ============================================================
        {
            let mut last = self.last_reconcile_attempt.lock().unwrap();
            if let Some(t) = *last {
                if t.elapsed() < std::time::Duration::from_secs(120) {
                    return Ok(ExecOutcome::Missed(
                        "reconcile-кэш: слишком часто — вход пропущен".into(),
                    ));
                }
            }
            *last = Some(std::time::Instant::now());
        }

        // ============================================================
        // 🔧 ПАТЧ-СИРОТЫ: сверка биржа/состояние ПЕРЕД входом.
        // IP-ban (-1003) → блокировка входов на 60 мин (жёсткий вариант —
        // без хрупкого парсинга времени бана).
        // ✅ P36/C1a: авторитетный вызов (без fallback на локальный стейт) —
        // иначе ветка Err ниже недостижима: resilient-обёртка на тестнете
        // подменяет сбой локальными позициями, и IP-ban гейт мёртв.
        // ============================================================
        match rest.get_positions_authoritative().await {
            Ok(all) => {
                if let Some(p) = all
                    .iter()
                    .find(|p| p.symbol.eq_ignore_ascii_case(&signal.symbol))
                {
                    if p.qty.abs() > 0.0 && !self.state_manager.has_position(&signal.symbol) {
                        tracing::error!(
                            "[{}] 🚨 RECONCILE-PRE-ENTRY: на бирже позиция {:.5} {}, бот flat — \
                             вход ЗАПРЕЩЁН (сиротская/чужая позиция)",
                            self.config.name, p.qty.abs(), p.symbol
                        );
                        return Ok(ExecOutcome::Missed(
                            "reconcile: на бирже позиция, ботом не управляемая".into(),
                        ));
                    }
                }
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("-1003") || msg.contains("banned") {
                    // 🔧 IP-BAN: биржа запретила REST — входы вниз на 60 минут
                    *self.entries_blocked_until.lock().unwrap() = Some(
                        std::time::Instant::now() + std::time::Duration::from_secs(3600),
                    );
                    tracing::error!(
                        "[{}] 🚨 IP BANNED биржей (-1003) — входы заблокированы на 60 мин. \
                         НЕ рестартить раньше: каждый запрос продлевает бан",
                        self.config.name
                    );
                } else {
                    tracing::warn!(
                        "[{}] pre-entry reconcile недоступен ({}) — вход пропущен",
                        self.config.name, msg
                    );
                }
                self.record_net_fail();
                return Ok(ExecOutcome::Missed("reconcile недоступен/бан".into()));
            }
        }

        // ============================================================
        // 🔧 ПИВОТ B-MARKET: маршрутизация входа.
        // ============================================================
        // ✅ P41/E14: is_passive (SpreadCapture maker-вход) удалён — ветка
        // была мёртвой: такой стратегии нет в фабрике с P15.
        let is_momentum = signal.strategy_name.ends_with("_MOMENTUM");

        let fill: Option<OrderResult> = if is_momentum {
            match self.enter_market_confirmed(rest, &signal, safe_qty, false).await {
                Ok(s) => Some(s),
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("СУДЬБА НЕИЗВЕСТНА") {
                        tracing::error!(
                            "[{}] 🚨 momentum market-вход не подтверждён (сеть): {} — \
                             следит periodic reconciliation + pre-entry reconcile",
                            self.config.name, msg
                        );
                        self.record_net_fail();
                    } else if msg.contains("Binance") {
                        tracing::warn!(
                            "[{}] momentum market-вход отклонён биржей: {}",
                            self.config.name, msg
                        );
                    } else {
                        tracing::warn!(
                            "[{}] momentum market-вход не состоялся: {}",
                            self.config.name, msg
                        );
                    }
                    None
                }
            }
        } else {
            match self.enter_market_confirmed(rest, &signal, safe_qty, false).await {
                Ok(s) => Some(s),
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("СУДЬБА НЕИЗВЕСТНА") {
                        tracing::error!(
                            "[{}] 🚨 market-вход не подтверждён (сеть): {} — \
                             следит периодический reconciliation + pre-entry reconcile",
                            self.config.name, msg
                        );
                        self.record_net_fail();
                    } else if msg.contains("Binance") {
                        tracing::warn!(
                            "[{}] market-вход отклонён биржей: {}",
                            self.config.name, msg
                        );
                    } else {
                        tracing::warn!(
                            "[{}] market-вход не состоялся: {}",
                            self.config.name, msg
                        );
                    }
                    None
                }
            }
        };

        let Some(order) = fill else {
            return Ok(ExecOutcome::Missed("вход не исполнен".into()));
        };

        if order.executed_qty <= 0.0 && order.status != "FILLED" {
            return Ok(ExecOutcome::Missed(format!(
                "ордер {} без исполнения (статус {})",
                order.order_id, order.status
            )));
        }

        let fill_price = if order.price > 0.0 { order.price } else { safe_price };
        let mut fill_qty = if order.executed_qty > 0.0 { order.executed_qty } else { safe_qty };   // ← mut
        // ✅ P41/E14: fee больше не выбирается по мёртвой maker-ветке
        // (использовался только в логе); вход всегда taker.
        let fee = self.taker_fee();

        tracing::info!(
            "✅ ВХОД ИСПОЛНЕН [{}] {} {:.5} {} @ {:.8} (сигнал {:.8}) | fee≈{:.4}% | id={} | факт-комиссия={:.6}",
            self.config.name,
            signal.side,
            fill_qty,
            signal.symbol,
            fill_price,
            safe_price,
            fee * 100.0,
            order.order_id,
            order.commission
        );

        // ============================================================
        // 🔧 SLIPPAGE GUARD: слиппедж > 0.3% = геометрия сделки испорчена
        // (риск/ед раздуло, TP/SL не на тех дистанциях — кейс 06:01:
        //  SELL по 0.00428010 при TP 0.00429960 ВЫШЕ входа → ложный TP).
        // Немедленный встречный MARKET тем же объёмом, затем
        // positionRisk-сверка: flat → выход; остаток → ведём дальше.
        // ============================================================
        let slip = if signal.price > 0.0 {
            (fill_price - signal.price).abs() / signal.price
        } else {
            0.0
        };
        if slip > 0.003 {
            let rev_side = if signal.side == "BUY" { "SELL" } else { "BUY" };
            tracing::error!(
                "[{}] 🚨 SLIPPAGE {:.2}% > 0.3% (сигнал {:.8} → исполнено {:.8}) — \
                 откат встречным market {} {}",
                self.config.name, slip * 100.0, signal.price, fill_price,
                rev_side, fill_qty
            );

            let mut rev = signal.clone();
            rev.side = rev_side.to_string();
            rev.price = fill_price;
            rev.quantity = fill_qty;
            rev.strategy_name = format!("{}_SLIP_CANCEL", signal.strategy_name);

            // Встречный market: падение = позиция остаётся (ведём софт-менеджментом)
            // ✅ P41/E14: reduce-only — откат гасит только что открытую позицию
            // и не может случайно удвоить её при дрейфе.
            if let Err(e) = self.enter_market_confirmed(rest, &rev, fill_qty, true).await {
                tracing::error!(
                    "[{}] 🚨🚨 откат НЕ ПОДТВЕРЖДЁН ({}): позиция {} {} остаётся на бирже — \
                     ведёт софт-менеджмент + периодический reconciliation",
                    self.config.name, e, signal.side, fill_qty
                );
            }

            // Фактический остаток у биржи (встречный мог исполниться частично)
            // ✅ P36/C1a: авторитетный вызов — fallback вернул бы локальный
            // (ещё пустой) стейт → ложное «позиции нет» при живой позиции.
            let actual = match rest.get_positions_authoritative().await {
                Ok(all) => all
                    .iter()
                    .find(|p| p.symbol.eq_ignore_ascii_case(&signal.symbol))
                    .map(|p| p.qty.abs())
                    .unwrap_or(0.0),
                Err(_) => fill_qty, // не смогли спросить — консервативно считаем всё живым
            };

            if actual <= 0.0 {
                tracing::warn!(
                    "[{}] ✅ откат завершён — позиции нет, комиссии списаны биржей",
                    self.config.name
                );
                return Ok(ExecOutcome::Missed(format!(
                    "SLIPPAGE {:.2}% — вход откачен встречным market",
                    slip * 100.0
                )));
            }

            tracing::warn!(
                "[{}] остаток после отката: {:.5} — регистрирую с фактическим объёмом",
                self.config.name, actual
            );
            fill_qty = actual;
        }

        // ============================================================
        // 🔧 RE-ANCHOR: TP/SL пересчитываются от ФАКТИЧЕСКОЙ цены
        // исполнения (оффсеты сигнала переносятся на fill). Кейс 06:01
        // без этого: TP шорта оказался ВЫШЕ фактического входа →
        // мгновенный ложный TakeProfit.
        // ============================================================
        let mut signal = signal;
        // ✅ P35/X7: RE-ANCHOR только при ОБОИХ уровнях (SL и TP). При None
        // offset считался 0 через unwrap_or(sig_entry) → SL или TP становился
        // равен fill_price → мгновенный ложный StopLoss/TakeProfit на первом
        // же тике (hit-проверка допускает равенство: price <= sl).
        if is_momentum && signal.price > 0.0 && fill_price > 0.0 {
            if let (Some(_), Some(_)) = (signal.stop_loss, signal.take_profit) {
                let sig_entry = signal.price;
                let sl_off = ((sig_entry - signal.stop_loss.unwrap_or(sig_entry)).abs()) / sig_entry;
                let tp_off = ((signal.take_profit.unwrap_or(sig_entry) - sig_entry).abs()) / sig_entry;
                let is_long = signal.side == "BUY";
                let (new_sl, new_tp) = if is_long {
                    (fill_price * (1.0 - sl_off), fill_price * (1.0 + tp_off))
                } else {
                    (fill_price * (1.0 + sl_off), fill_price * (1.0 - tp_off))
                };
                tracing::warn!(
                    "[{}] 📏 RE-ANCHOR: fill {:.8} vs сигнал {:.8} (смещение {:+.3}%) | SL → {:.8}, TP → {:.8}",
                    self.config.name, fill_price, sig_entry,
                    (fill_price - sig_entry) / sig_entry * 100.0, new_sl, new_tp
                );
                signal.stop_loss = Some(new_sl);
                signal.take_profit = Some(new_tp);
            }
        }

        // AUDIT-P3: без SL — консервативные 2% от номинала fill (не 0).
        let sl_ref = signal.stop_loss.unwrap_or(fill_price * 0.98);
        let risk_usdt = fill_qty * (fill_price - sl_ref).abs();

        let initial_risk = (fill_price - signal.stop_loss.unwrap_or(fill_price * 0.98)).abs(); // AUDIT-P3: нет SL = 2% (не 0)
        if was_dca {
            // ✅ P39/H12+E7: транша лестницы — дозакупка в позицию.
            // Раньше add_position_with_risk ЗАМЕНЯЛ позицию последней траншей
            // (учёт вёл только её объём), а прежние SL/TP оставались на бирже
            // сиротами (E7). Теперь: объём суммируется, avg пересчитывается,
            // старые защиты сняты — новые ставятся ниже по SL/TP сигнала.
            let prot_ids = self.state_manager.get_protective_ids(&signal.symbol);
            self.remove_protective_orders(&signal.symbol, prot_ids).await;
            self.state_manager
                .add_dca_order(
                    &signal.symbol,
                    &signal.side,
                    fill_price,
                    fill_qty,
                    signal.stop_loss,
                    signal.take_profit,
                    signal.strategy_name.clone(),
                    initial_risk,
                    signal.atr_hint,
                    signal.entry_spread,
                )
                .await;
            // ✅ P39/H12: единый лимит риска лестницы — суммарный объём ×
            // риск/ед по актуальному SL (консервативно для ранних траншей).
            let total_qty = self
                .state_manager
                .get_positions()
                .iter()
                .find(|p| p.symbol == signal.symbol)
                .map(|p| p.quantity)
                .unwrap_or(fill_qty);
            self.global_risk
                .register_position(&self.config.name, &signal.symbol, total_qty * initial_risk);
        } else {
            self.global_risk
                .register_position(&self.config.name, &signal.symbol, risk_usdt);
            self.state_manager
                .add_position_with_risk(
                    signal.symbol.clone(),
                    signal.side.clone(),
                    fill_price,
                    fill_qty,
                    signal.stop_loss,
                    signal.take_profit,
                    signal.strategy_name.clone(),
                    initial_risk,
                    signal.atr_hint,
                    signal.entry_spread,
                )
                .await;
        }

        if order.commission > 0.0 {
            self.state_manager
                .add_fees_paid(&signal.symbol, order.commission);
        } else {
            self.recover_commissions(rest, &signal.symbol, &[order.order_id])
                .await;
        }

        // ============================================================
        // 🔧 ЭТАП B: биржевые защитные ордера (STOP_MARKET + TP_MARKET)
        // — на РЕ-АНКЕРЕННЫХ ценах.
        // ============================================================
        self.place_protective_orders(rest, &signal, order.order_id).await;

        Ok(ExecOutcome::Entry { price: fill_price, qty: fill_qty, order_id: order.order_id })
    }
}