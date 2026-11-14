//! Resilient decorator поверх `ExchangeClient` — неблокирующий reconcile.
//!
//! ## Проблема
//! Тестнет Binance `/fapi/v2/positionRisk` нестабилен — на начале часа
//! отдаёт `error decoding response body` (HTML вместо JSON) или таймаутит.
//! Движок вызывает `get_positions()` ПЕРЕД входом в сделку → на любой
//! сбой API сделка пропускается → бот не входит в сигналы.
//!
//! ## Решение (Decorator, БЕЗ правок core-файлов)
//! 1. Оборачиваем реальный `BinanceFuturesClient` в `ResilientClient`.
//! 2. Все "торговые" методы (place_market_order, cancel_order, ...) —
//!    **напрямую** во внутренний клиент, БЕЗ retry (риск двойной
//!    отправки ордера).
//! 3. ТОЛЬКО `get_positions()` — retry с экспоненциальным backoff +
//!    жёсткий per-attempt таймаут + circuit breaker + fallback на
//!    локальный стейт (StateManager, который и есть источник правды
//!    для собственных позиций бота).
//! 4. Когда все retry упали — возвращаем локальные позиции из БД.
//!    Периодический reconciliation (5 мин в main.rs) ловит дрейф между
//!    локальным стейтом и биржей в фоне.
//!
//! ## Безопасность
//! - Env `RECONCILE_STRICT=1` → fallback возвращает `Err` (исходное
//!   поведение "пропустить сделку"). На mainnet лучше ставить STRICT=1.
//! - Circuit breaker: 3 провала подряд → 60с без запросов к бирже,
//!   сразу fallback (защита от rate-limit бана testnet).
//! - Per-attempt timeout 3с — внешний `tokio::time::timeout` гарантированно
//!   отменяет даже зависший `reqwest` (внутренний 10с таймаут клиента
//!   не успеет сработать).
//!
//! ## Архитектура
//! Decorator → `inner: Arc<dyn ExchangeClient>` (реальный клиент)
//!          → `fallback: Arc<dyn ReconcileFallback>` (локальная БД)
//!
//! Pattern: «Decorator + Circuit Breaker + Local Fallback» —
//! не требует правок traits.rs / binance_futures.rs / execution_engine.rs,
//! обновление core-крейта ничего не сломает.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Mutex; // ✅ P41/clean: std-Mutex сознательно (комментарий про отсутствие parking_lot устарел — крейт в Cargo.toml, здесь нужен именно std-аналог)

use super::traits::{AlgoOrderInfo, ExchangeClient, ExchangePosition, OrderResult}; // ✅ P53/C4-3: +AlgoOrderInfo
use crate::state::StateManager;

// =====================================================================
// Трейт fallback-провайдера — реализуется на чём угодно, что знает
// локальное состояние позиций бота.
// =====================================================================
pub trait ReconcileFallback: Send + Sync {
    /// Возвращает локальные (БД) позиции бота в виде биржевых.
    /// `qty` > 0 = LONG, < 0 = SHORT (как в `ExchangePosition`).
    fn local_positions(&self) -> Vec<ExchangePosition>;
}

/// `StateManager` — авторитетная локальная книга открытых позиций
/// (in-memory DashMap + SQLite). Это естественный fallback, когда биржа
/// недоступна: бот знает свои позиции лучше, чем тестнет в момент сбоя.
impl ReconcileFallback for StateManager {
    fn local_positions(&self) -> Vec<ExchangePosition> {
        self.get_positions()
            .iter()
            .map(|p| ExchangePosition {
                symbol: p.symbol.clone(),
                qty: match p.side {
                    crate::state::state_manager::PositionSide::Long => p.quantity,
                    crate::state::state_manager::PositionSide::Short => -p.quantity,
                },
                entry_price: p.entry_price,
            })
            .collect()
    }
}

// =====================================================================
// Circuit breaker state
// =====================================================================
#[derive(Clone, Copy, Default)]
struct CircuitState {
    consecutive_failures: u32,
    opened_at_ms: i64,
}

// =====================================================================
// Конфиг retry — откалиброван под сигнальный цикл (1h candle).
// Сигнал в xx:00:04, reconcile стартует ~xx:00:14.
// 3 попытки × 3с timeout + 300мс + 800мс backoff = ≤10.1с
// → вход до xx:00:25, задолго до следующей свечи (xx:01:00).
// =====================================================================
const PER_ATTEMPT_TIMEOUT_SECS: u64 = 3;
const BACKOFF_MS: &[u64] = &[300, 800]; // между 1→2 и 2→3 попыткой
const CIRCUIT_THRESHOLD: u32 = 3;        // 3 провала подряд → открыть
const CIRCUIT_COOLDOWN_MS: i64 = 60_000; // 60с fallback-режим

pub struct ResilientClient {
    inner: Arc<dyn ExchangeClient>,
    fallback: Arc<dyn ReconcileFallback>,
    circuit: Mutex<CircuitState>,
    /// Strict = на fallback возвращать Err (для mainnet).
    strict: bool,
}

impl ResilientClient {
    pub fn new(
        inner: Arc<dyn ExchangeClient>,
        fallback: Arc<dyn ReconcileFallback>,
        strict_default: bool, // ✅ P20: strict-by-URL из main.rs (testnet → false, prod → true)
    ) -> Self {
        // [P20] Приоритет: env RECONCILE_STRICT → strict_default (вычислен по URL).
        // main.rs уже передаёт правильный дефолт: !rest_url.contains("testnet").
        // ✅ ФИКС: раньше параметр игнорировался (_strict_default), resolve_strict()
        // всегда возвращал true → тестнет работал в strict-режиме, и КАЖДЫЙ сбой
        // positionRisk (63 раза за 11ч) = «вход пропущен». Теперь на тестнете
        // дефолт strict=0: сбой reconcile → fallback на локальный StateManager
        // (источник правды по собственным позициям бота) → вход выполняется.
        // На проде (fapi.binance.com) дефолт strict=1 — поведение не меняется.
        let strict = Self::resolve_strict(strict_default);
        tracing::info!(
            "RECONCILE_STRICT = {} (url-default = {}, env override wins)",
            strict, strict_default
        );
        Self {
            inner,
            fallback,
            circuit: Mutex::new(CircuitState::default()),
            strict,
        }
    }

    // [P11:strict → ✅ P20] env override приоритетнее; дефолт — по URL
    // (testnet → false, prod → true), а не жёсткий `true`.
    fn resolve_strict(url_default: bool) -> bool {
        match std::env::var("RECONCILE_STRICT") {
            Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => true,
            Ok(v) if v == "0" || v.eq_ignore_ascii_case("false") => false,
            _ => url_default, // ✅ P20: раньше здесь был безусловный `true`
        }
    }

    // --- Circuit breaker helpers ------------------------------------

    fn circuit_open(&self) -> bool {
        // ✅ P19c: mut — нужен для сброса opened_at_ms при истечении cooldown.
        let mut st = self.circuit.lock().unwrap_or_else(|e| e.into_inner());
        if st.consecutive_failures < CIRCUIT_THRESHOLD {
            return false;
        }
        let now = chrono::Utc::now().timestamp_millis();
        if now - st.opened_at_ms >= CIRCUIT_COOLDOWN_MS {
            // ✅ P19c: cooldown истёк → half-open. Явно сбрасываем opened_at_ms:
            // состояние «открыто, но окно прошло» больше не висит. Раньше
            // breaker срабатывал ровно ОДИН раз за жизнь процесса: после
            // первого открытия проверка `== 3` не срабатывала (счётчик 4, 5, …),
            // opened_at_ms протухал → circuit_open() всегда false → каждая
            // неудача стоила полных ~10с ретраев вместо мгновенного fallback.
            st.opened_at_ms = 0;
            tracing::info!(
                "🔄 reconcile circuit-breaker: cooldown {}с истёк → half-open, пробуем биржу",
                CIRCUIT_COOLDOWN_MS / 1000
            );
            return false;
        }
        true
    }

    fn record_success(&self) {
        let mut st = self.circuit.lock().unwrap_or_else(|e| e.into_inner());
        st.consecutive_failures = 0;
        st.opened_at_ms = 0;
    }

    fn record_failure(&self) {
        let mut st = self.circuit.lock().unwrap_or_else(|e| e.into_inner());
        st.consecutive_failures = st.consecutive_failures.saturating_add(1);
        // ✅ P19c: `>=` вместо `==` — после первого открытия счётчик только
        // растёт (3→4→5…), `==` больше не срабатывал и breaker не переоткрывался.
        if st.consecutive_failures >= CIRCUIT_THRESHOLD {
            st.opened_at_ms = chrono::Utc::now().timestamp_millis();
            tracing::warn!(
                "🔄 reconcile circuit-breaker РАЗОМКНУТ: {} провала подряд → \
                 {}с fallback-режим (без запросов к бирже)",
                CIRCUIT_THRESHOLD,
                CIRCUIT_COOLDOWN_MS / 1000
            );
        }
    }

    // --- Fallback decision ------------------------------------------

    fn fallback_result(&self) -> Result<Vec<ExchangePosition>> {
        if self.strict {
            return Err(anyhow::anyhow!(
                "reconcile недоступен (RECONCILE_STRICT=1) — вход пропущен по strict-режиму"
            ));
        }
        let local = self.fallback.local_positions();
        tracing::warn!(
            "🛟 reconcile fallback → взял {} локальных позиций из StateManager",
            local.len()
        );
        Ok(local)
    }
}

// =====================================================================
// Реализация ExchangeClient — Decorator.
// Все торговые методы делегируются как есть (БЕЗ retry!).
// Только get_positions получает устойчивую обвязку.
// =====================================================================
#[async_trait]
impl ExchangeClient for ResilientClient {
    async fn place_market_order(
        &self,
        symbol: &str,
        side: &str,
        quantity: f64,
        client_order_id: Option<String>,
    ) -> Result<OrderResult> {
        // ⚠️ НЕТ retry — повторная отправка = двойной ордер.
        self.inner
            .place_market_order(symbol, side, quantity, client_order_id)
            .await
    }

    async fn place_limit_order(
        &self,
        symbol: &str,
        side: &str,
        quantity: f64,
        price: f64,
        client_order_id: Option<String>,
    ) -> Result<OrderResult> {
        self.inner
            .place_limit_order(symbol, side, quantity, price, client_order_id)
            .await
    }

    // ✅ P41/E14: reduceOnly-выход должен проходить НАСКВОЗЬ к реальному
    // клиенту — дефолтная реализация трейта вызвала бы self.place_market_order
    // (без reduceOnly) и пыль/дрейф-защита потерялись бы в обёртке.
    async fn place_market_order_reduce_only(
        &self,
        symbol: &str,
        side: &str,
        quantity: f64,
        client_order_id: Option<String>,
    ) -> Result<OrderResult> {
        self.inner
            .place_market_order_reduce_only(symbol, side, quantity, client_order_id)
            .await
    }

    async fn cancel_order(&self, symbol: &str, order_id: u64) -> Result<bool> {
        self.inner.cancel_order(symbol, order_id).await
    }

    async fn get_order_status(&self, symbol: &str, order_id: u64) -> Result<OrderResult> {
        self.inner.get_order_status(symbol, order_id).await
    }

    async fn get_balance(&self, asset: &str) -> Result<f64> {
        self.inner.get_balance(asset).await
    }

    async fn get_order_book(
        &self,
        symbol: &str,
        limit: usize,
    ) -> Result<(Vec<(f64, f64)>, Vec<(f64, f64)>)> {
        self.inner.get_order_book(symbol, limit).await
    }

    async fn sync_time(&self) -> Result<()> {
        self.inner.sync_time().await
    }

    async fn get_order_commission(&self, symbol: &str, order_id: u64) -> Result<f64> {
        self.inner.get_order_commission(symbol, order_id).await
    }

    async fn place_protective_order(
        &self,
        symbol: &str,
        side: &str,
        stop: bool,
        trigger_price: f64,
        client_order_id: Option<String>,
    ) -> Result<OrderResult> {
        // ⚠️ НЕТ retry — повторная отправка = дубликат SL/TP ордера.
        self.inner
            .place_protective_order(symbol, side, stop, trigger_price, client_order_id)
            .await
    }

    async fn get_order_by_client_id(
        &self,
        symbol: &str,
        orig_client_order_id: &str,
    ) -> Result<OrderResult> {
        // Чтение по client_id — per-attempt таймаут, без retry.
        match tokio::time::timeout(
            Duration::from_secs(PER_ATTEMPT_TIMEOUT_SECS),
            self.inner.get_order_by_client_id(symbol, orig_client_order_id),
        )
        .await
        {
            Ok(inner) => inner,
            Err(_) => Err(anyhow::anyhow!(
                "get_order_by_client_id таймаут {}с",
                PER_ATTEMPT_TIMEOUT_SECS
            )),
        }
    }

    async fn get_open_orders(&self, symbol: &str) -> Result<Vec<OrderResult>> {
        // Список открытых ордеров по символу — per-attempt таймаут, без retry.
        match tokio::time::timeout(
            Duration::from_secs(PER_ATTEMPT_TIMEOUT_SECS),
            self.inner.get_open_orders(symbol),
        )
        .await
        {
            Ok(inner) => inner,
            Err(_) => Err(anyhow::anyhow!(
                "get_open_orders таймаут {}с",
                PER_ATTEMPT_TIMEOUT_SECS
            )),
        }
    }

    // ===============================================================
    // ✅ P53/C4-3: открытые алго-ордера (сверка защит C4) — чтение,
    // per-attempt таймаут, БЕЗ retry и БЕЗ fallback (честная ошибка:
    // недоступность списка защит должна быть видна reconciler'у).
    // ===============================================================
    async fn get_open_algo_orders(&self, symbol: &str) -> Result<Vec<AlgoOrderInfo>> {
        match tokio::time::timeout(
            Duration::from_secs(PER_ATTEMPT_TIMEOUT_SECS),
            self.inner.get_open_algo_orders(symbol),
        )
        .await
        {
            Ok(inner) => inner,
            Err(_) => Err(anyhow::anyhow!(
                "get_open_algo_orders таймаут {}с",
                PER_ATTEMPT_TIMEOUT_SECS
            )),
        }
    }

    // ===============================================================
    // ✨ Единственный метод с устойчивой обвязкой
    // ===============================================================
    async fn get_positions(&self) -> Result<Vec<ExchangePosition>> {
        // 1) Circuit открыт → в обход retry сразу локальный fallback.
        if self.circuit_open() {
            tracing::debug!(
                "reconcile: circuit открыт → сразу fallback (без запроса к бирже)"
            );
            return self.fallback_result();
        }

        let total_attempts = 1 + BACKOFF_MS.len(); // 3 попытки
        for attempt in 1..=total_attempts {
            // Жёсткий per-attempt таймаут — даже если reqwest внутри
            // завис на 10с, мы отменим future через PER_ATTEMPT_TIMEOUT_SECS.
            let fut = self.inner.get_positions();
            match tokio::time::timeout(
                Duration::from_secs(PER_ATTEMPT_TIMEOUT_SECS),
                fut,
            )
            .await
            {
                Ok(Ok(v)) => {
                    self.record_success();
                    return Ok(v);
                }
                Ok(Err(e)) => {
                    // API ответило, но с ошибкой (HTML вместо JSON,
                    // 5xx, banned, etc.) — retry.
                    tracing::warn!(
                        "reconcile попытка {}/{}: API ошибка: {}",
                        attempt, total_attempts, e
                    );
                }
                Err(_) => {
                    // Сетевой таймаут — future отменена.
                    tracing::warn!(
                        "reconcile попытка {}/{}: таймаут {}с",
                        attempt, total_attempts, PER_ATTEMPT_TIMEOUT_SECS
                    );
                }
            }
            if attempt < total_attempts {
                let d = BACKOFF_MS[attempt - 1];
                tokio::time::sleep(Duration::from_millis(d)).await;
            }
        }

        // 2) Все retry исчерпаны → фиксируем провал для circuit breaker
        //    и возвращаем локальный fallback.
        self.record_failure();
        tracing::warn!(
            "🛟 reconcile недоступен после {} попыток → fallback на StateManager{}",
            total_attempts,
            if self.strict { " (STRICT → реальная ошибка)" } else { "" }
        );
        self.fallback_result()
    }

    // ===============================================================
    // ✅ P36/C1a: авторитетный путь — та же сеть ретраев, что у get_positions,
    // но БЕЗ circuit breaker и БЕЗ fallback на локальный стейт. Периодическая
    // сверка (main.rs) и pre-entry reconcile (execution_engine) обязаны видеть
    // ЧЕСТНУЮ ошибку биржи: раньше сбой positionRisk на тестнете подменялся
    // локальными позициями StateManager, сверка всегда «сходилась»,
    // IP-ban гейт (-1003) в engine был мёртв (ветка Err недостижима).
    // ===============================================================
    async fn get_positions_authoritative(&self) -> Result<Vec<ExchangePosition>> {
        let total_attempts = 1 + BACKOFF_MS.len();
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 1..=total_attempts {
            let fut = self.inner.get_positions();
            match tokio::time::timeout(
                Duration::from_secs(PER_ATTEMPT_TIMEOUT_SECS),
                fut,
            )
            .await
            {
                Ok(Ok(v)) => return Ok(v),
                Ok(Err(e)) => {
                    tracing::warn!(
                        "reconcile(authoritative) попытка {}/{}: API ошибка: {}",
                        attempt, total_attempts, e
                    );
                    last_err = Some(e);
                }
                Err(_) => {
                    tracing::warn!(
                        "reconcile(authoritative) попытка {}/{}: таймаут {}с",
                        attempt, total_attempts, PER_ATTEMPT_TIMEOUT_SECS
                    );
                    last_err = Some(anyhow::anyhow!(
                        "get_positions (authoritative) таймаут {}с",
                        PER_ATTEMPT_TIMEOUT_SECS
                    ));
                }
            }
            if attempt < total_attempts {
                tokio::time::sleep(Duration::from_millis(BACKOFF_MS[attempt - 1])).await;
            }
        }
        Err(last_err.unwrap_or_else(|| {
            anyhow::anyhow!("get_positions (authoritative) недоступен")
        }))
    }

    async fn apply_margin_settings(
        &self,
        symbol: &str,
        leverage: u32,
        isolated: bool,
    ) -> Result<()> {
        // ✅ P37/K3: делегируем во внутренний клиент (без retry — POST не
        // идемпотентен в общем случае).
        self.inner
            .apply_margin_settings(symbol, leverage, isolated)
            .await
    }
}
