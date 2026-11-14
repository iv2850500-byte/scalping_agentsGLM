use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait ExchangeClient: Send + Sync {
    async fn place_market_order(
        &self,
        symbol: &str,
        side: &str,
        quantity: f64,
        client_order_id: Option<String>,
    ) -> Result<OrderResult>;

    async fn place_limit_order(
        &self,
        symbol: &str,
        side: &str,
        quantity: f64,
        price: f64,
        client_order_id: Option<String>,
    ) -> Result<OrderResult>;

    /// 🔧 ЖЁСТКИЕ ЗАЩИТЫ (Этап B): постановка защитного ордера на бирже.
    /// stop=true  → STOP_MARKET (стоп-лосс)
    /// stop=false → TAKE_PROFIT_MARKET (тейк-профит)
    /// closePosition=true → закрывает ВСЮ позицию при триггере,
    /// quantity НЕ указывается (биржа отвергает связку).
    /// client_order_id: префикс "pr_" помечает ордер как защитный —
    /// sweep при shutdown не снимает их при живой позиции.
    async fn place_protective_order(
        &self,
        symbol: &str,
        side: &str,
        stop: bool,
        trigger_price: f64,
        client_order_id: Option<String>,
    ) -> Result<OrderResult>;

    /// ✅ К-11: true = отменён; false = ордера уже нет — это НЕ ошибка.
    async fn cancel_order(&self, symbol: &str, order_id: u64) -> Result<bool>;

    async fn get_order_status(&self, symbol: &str, order_id: u64) -> Result<OrderResult>;
    async fn get_order_by_client_id(&self, symbol: &str, orig_client_order_id: &str) -> Result<OrderResult>;
    async fn get_open_orders(&self, symbol: &str) -> Result<Vec<OrderResult>>;
    async fn get_balance(&self, asset: &str) -> Result<f64>;
    async fn get_order_book(&self, symbol: &str, limit: usize) -> Result<(Vec<(f64, f64)>, Vec<(f64, f64)>)>;
    async fn sync_time(&self) -> Result<()>;

    /// ✅ 4г: открытые позиции на бирже (для reconciliation).
    async fn get_positions(&self) -> Result<Vec<ExchangePosition>> {
        Ok(Vec::new())
    }

    /// ✅ P36/C1a: авторитетная сверка позиций — БЕЗ локального fallback.
    /// Дефолт = обычный get_positions (клиенты без resilient-декоратора).
    /// ResilientClient переопределяет: реальная биржа и честная ошибка
    /// ({"code":-1003}, HTML 200, 5xx) вместо молчаливой подмены локальным
    /// стейтом, который с ним всегда «сходится».
    async fn get_positions_authoritative(&self) -> Result<Vec<ExchangePosition>> {
        self.get_positions().await
    }

    /// ✅ P37/K3: выставить плечо и режим маржи на бирже (futures).
    /// Дефолт — no-op для бирж/обёрток без таких эндпоинтов.
    async fn apply_margin_settings(
        &self,
        _symbol: &str,
        _leverage: u32,
        _isolated: bool,
    ) -> Result<()> {
        Ok(())
    }

    /// ✅ ПРАВКА 2 (докапчивание): фактическая комиссия ордера из income.
    async fn get_order_commission(&self, _symbol: &str, _order_id: u64) -> Result<f64> {
        Ok(0.0)
    }

    /// ✅ P41/E14: market-ордер с reduceOnly=true — ВЫХОДЫ. Биржа пропускает
    /// minNotional для reduceOnly (-4164: «unless you choose reduce only») —
    /// пыль закрывается ордером, а не гашением в state (фантом устранён).
    /// Побочно: reduce-only не может ОТКРЫТЬ позицию при дрейфе стейта —
    /// неверный выход отклонится (-2022), а не перевернётся.
    /// Дефолт — обычный market (клиенты без reduceOnly-поддержки, поведение
    /// прежнее); binance_futures переопределяет.
    async fn place_market_order_reduce_only(
        &self,
        symbol: &str,
        side: &str,
        quantity: f64,
        client_order_id: Option<String>,
    ) -> Result<OrderResult> {
        self.place_market_order(symbol, side, quantity, client_order_id).await
    }

    // ============================================================
    // ✅ P53/C4-1: открытые АЛГО-ордера (защиты SL/TP на Algo API).
    // Периодическая сверка защит (C4): позиция жива, а SL/TP на бирже
    // отсутствует → восстановить; защиты без позиции → снять сирот.
    // Классический /fapi/v1/openOrders НЕ видит Algo-ордера — нужен
    // отдельный /fapi/v1/openAlgoOrders.
    // Дефолт — Err: клиенты без Algo API (spot, bybit, okx) честно
    // отключают C4-сверку (однократный warn в reconciler).
    // ============================================================
    async fn get_open_algo_orders(&self, _symbol: &str) -> Result<Vec<AlgoOrderInfo>> {
        Err(anyhow::anyhow!("openAlgoOrders не поддерживается этим клиентом"))
    }
}

/// ✅ P53/C4-1: защитный алго-ордер на бирже (срезка /fapi/v1/openAlgoOrders).
#[derive(Debug, Clone)]
pub struct AlgoOrderInfo {
    /// algoId (или orderId в классическом fallback) — годится для cancel_order.
    pub algo_id: u64,
    pub symbol: String,
    /// BUY / SELL — сторона ордера (для защиты = сторона ЗАКРЫТИЯ позиции).
    pub side: String,
    /// STOP_MARKET / TAKE_PROFIT_MARKET / прочее.
    pub algo_type: String,
    /// NEW / WORKING / FINISHED / CANCELED / ...
    pub status: String,
    /// Триггерная цена (stopPrice/triggerPrice); 0.0 если не распознана.
    pub trigger_price: f64,
    /// closePosition=true (наша семантика защит: закрыть ВСЮ позицию).
    pub close_position: bool,
}

impl AlgoOrderInfo {
    /// Это наш стоп-лосс (STOP_MARKET) на сторону закрытия?
    pub fn is_stop(&self, close_side: &str) -> bool {
        self.algo_type.eq_ignore_ascii_case("STOP_MARKET")
            && self.side.eq_ignore_ascii_case(close_side)
    }

    /// Это наш тейк-профит (TAKE_PROFIT_MARKET) на сторону закрытия?
    pub fn is_take_profit(&self, close_side: &str) -> bool {
        self.algo_type.eq_ignore_ascii_case("TAKE_PROFIT_MARKET")
            && self.side.eq_ignore_ascii_case(close_side)
    }

    /// Защита ещё жива (не исполнена и не снята).
    pub fn is_working(&self) -> bool {
        matches!(
            self.status.to_ascii_uppercase().as_str(),
            "NEW" | "WORKING" | "PENDING" | "PARTIALLY_FILLED"
        )
    }
}

#[derive(Debug, Clone)]
pub struct OrderResult {
    pub order_id: u64,
    pub symbol: String,
    pub side: String,
    pub price: f64,
    pub quantity: f64,
    pub executed_qty: f64,
    pub status: String,
    /// ✅ ДОРОГА А: фактическая комиссия исполнения (USDT, ≥ 0).
    pub commission: f64,
}

/// ✅ 4г: позиция на бирже для reconciliation.
#[derive(Debug, Clone)]
pub struct ExchangePosition {
    pub symbol: String,
    /// положительный = long, отрицательный = short
    pub qty: f64,
    pub entry_price: f64,
}