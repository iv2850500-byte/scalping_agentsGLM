pub mod bounce_trader;
pub mod market_regime;
pub mod orderbook_analysis;
pub mod momentum;
// PATCH P15: asian_breakout removed (simpler duplicate of keltner_dca, dead code)
pub mod keltner_dca;
pub mod liquidity_grab;
pub mod gold_session_trader;


use crate::config::BotConfig;
use crate::websocket::WebSocketMessage;
use anyhow::Result;
use rust_decimal::prelude::ToPrimitive;
use std::sync::{Arc, RwLock};

pub use orderbook_analysis::OrderBookAnalyzer;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PositionState {
    Flat,
    InPosition,
    Cooldown,
}

pub struct SignalFilter {
    state: PositionState,
    last_signal_at: Option<i64>,
    cooldown_ms: i64,
    last_signal_price: f64,
    price_delta_threshold: f64,
}

impl SignalFilter {
    pub fn new(cooldown_secs: u64, price_delta_pct: f64) -> Self {
        Self {
            state: PositionState::Flat,
            last_signal_at: None,
            cooldown_ms: cooldown_secs as i64 * 1000,
            last_signal_price: 0.0,
            price_delta_threshold: price_delta_pct / 100.0,
        }
    }

    pub fn can_signal(&mut self, current_price: f64, now_ms: i64) -> bool {
        match self.state {
            PositionState::InPosition => false,
            PositionState::Flat => true,
            PositionState::Cooldown => {
                let Some(t) = self.last_signal_at else {
                    self.state = PositionState::Flat;
                    return true;
                };
                let price_moved = if self.last_signal_price > 0.0 {
                    (current_price - self.last_signal_price).abs() / self.last_signal_price
                } else { 0.0 };
                if price_moved > self.price_delta_threshold {
                    self.state = PositionState::Flat;
                    return true;
                }
                if now_ms.saturating_sub(t) >= self.cooldown_ms {
                    self.state = PositionState::Flat;
                    return true;
                }
                false
            }
        }
    }

    pub fn on_signal(&mut self, price: f64, now_ms: i64) {
        self.state = PositionState::Cooldown;
        self.last_signal_at = Some(now_ms);
        self.last_signal_price = price;
    }

    pub fn on_position_opened(&mut self) {
        self.state = PositionState::InPosition;
    }

    pub fn on_position_closed(&mut self, now_ms: i64) {
        self.state = PositionState::Cooldown;
        self.last_signal_at = Some(now_ms);
        // ✅ P36/F5: сброс якоря цены сигнала. Раньше last_signal_price
        // оставался от исходного сигнала: после закрытия позиции движение
        // цены >price_delta_threshold от СТАРОГО уровня немедленно снимало
        // кулдаун (обход паузы «после сделки»). 0.0 → в кулдауне работает
        // только таймер. Зеркало в wfa_momentum.rs — тем же паком.
        self.last_signal_price = 0.0;
    }

    pub fn state(&self) -> PositionState { self.state }
}

pub trait Strategy: Send + Sync {
    fn name(&self) -> &str;
    fn process(&mut self, msg: &WebSocketMessage) -> Result<Option<Signal>>;

    fn notify_position_open(&mut self) {}
    fn notify_position_closed(&mut self, _pnl: f64) {}

    fn in_position(&self) -> bool { false }

    fn diagnostics(&self) -> Option<String> { None }

    fn set_risk_multiplier(&mut self, _mult: f64) {}

    fn on_entry_failed(&mut self, _reason: &str) {}

    fn on_position_unknown(&mut self) {}

    // P16: push live equity into the strategy (sizing base)
    fn set_live_equity(&mut self, _eq: f64) {}

    fn can_dca(&self) -> bool { false }

    /// ✅ П3: хочет ли стратегия получать backfill-свечи (историю от
    /// REST-поллера, 250 шт при старте) для мгновенного прогрева индикаторов.
    /// ПО УМОЛЧАНИЮ false — защита: стратегия без поддержки is_backfill
    /// может выдать СИГНАЛ по старой цене (исполнится по текущему рынку).
    /// Переопределяет только BounceTrader (сигналы по backfill запрещены
    /// внутри его process()).
    fn wants_backfill(&self) -> bool { false }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Signal {
    pub symbol: String,
    pub side: String,
    pub price: f64,
    pub quantity: f64,
    pub strategy_name: String,
    pub leverage: f64,
    pub stop_loss: Option<f64>,
    pub take_profit: Option<f64>,
    pub atr_hint: f64,
    pub entry_spread: f64,
}

#[derive(Debug, Clone, Default)]
pub struct OrderBookState {
    pub best_bid: f64,
    pub best_ask: f64,
}

pub fn create_strategy(
    config: &BotConfig,
    global_symbol: &str,
    _order_book_state: Arc<RwLock<OrderBookState>>,
    _orderbook_analyzer: OrderBookAnalyzer,
) -> Result<Box<dyn Strategy>> {
    let balance = config.initial_balance.to_f64().unwrap_or(1000.0);
    let risk_pct = config.risk_per_trade_pct.to_f64().unwrap_or(0.5);

    let is_futures = config.market_type == "futures";
    let default_fee = if is_futures { 0.0005 } else { 0.00075 };
    let fee_rate = config
        .fee_rate
        .and_then(|d| d.to_f64())
        .unwrap_or(default_fee);

    let min_net_rr = config.min_net_rr.to_f64().unwrap_or(1.2);
    let min_ev_pct = config.min_ev_pct.to_f64().unwrap_or(0.0);

    let get = |key: &str, default: f64| -> f64 {
        config
            .strategy
            .params
            .get(key)
            .and_then(|d| d.to_f64())
            .unwrap_or(default)
    };

    match config.strategy.name.as_str() {
        "momentum" => Ok(Box::new(
            momentum::MomentumStrategy::new(
                global_symbol.to_string(),
                balance,
                risk_pct,
            )
            .with_windows(
                get("lookback", 20.0) as usize,
                get("atr_period", 14.0) as usize,
                get("ofi_window", 5.0) as usize,
            )
            .with_fee(fee_rate, min_net_rr, min_ev_pct)
            .with_tp_sl(
                get("tp_atr_mult", 2.5),
                get("min_tp_pct", 0.0025),
                get("rr_ratio", 2.5),
            )
            .with_thresholds(
                get("min_atr_threshold", 0.05),
                get("ofi_threshold", 0.15),
                get("volume_anomaly_threshold", 1.5),
            )
            .with_limits(
                get("max_trades_per_day", 10.0) as u32,
                get("min_time_between_trades_ms", 300_000.0) as i64,
                get("trading_start_utc", 7.0) as u32,
                get("trading_end_utc", 22.0) as u32,
            )
            .with_regime(
                get("regime_window", 100.0) as usize,
                get("regime_volatile_pct", 0.5),
                get("regime_trending_pct", 0.1),
            ),
        )),

        "gold_session" => Ok(Box::new(
            gold_session_trader::GoldSessionStrategy::new(
                global_symbol.to_string(),
                balance,
                risk_pct,
                fee_rate,
            ),
        )),

        "liquidity_grab" => Ok(Box::new(
            liquidity_grab::LiquidityGrabStrategy::new(
                global_symbol.to_string(),
                balance,
                risk_pct,
                fee_rate,
            )
            .with_rr(2.5, 1.5),
        )),

        // ✅ P41/clean (устаревшая ссылка BUG#3 снята)-фикс: все пороги BounceTrader настраиваются через
        // [bots.<name>.strategy.params] в config.toml (ниже — дефолты):
        //   rr_ratio = 2.0, rsi_oversold = 30, rsi_overbought = 70,
        //   trend_threshold_pct = 1.5, trend_window = 12,
        //   support_search_pct = 1.2  (в ПРОЦЕНТАХ, радиус поиска уровня)
        "bounce_trader" => Ok(Box::new(
            bounce_trader::BounceTraderStrategy::new(
                global_symbol.to_string(),
                balance,
                risk_pct,
                fee_rate,
            )
            .with_rr(get("rr_ratio", 2.0), min_net_rr)
            .with_rsi(get("rsi_oversold", 30.0), get("rsi_overbought", 70.0))
            .with_trend_filter(
                get("trend_threshold_pct", 1.5),
                get("trend_window", 12.0) as usize,
            )
            .with_search_pct(get("support_search_pct", 1.2) / 100.0),
        )),

        // PATCH P15: asian_breakout branch removed

        "keltner_dca" => Ok(Box::new(
            keltner_dca::KeltnerDcaStrategy::new(
                global_symbol.to_string(),
                balance,
                risk_pct,
                fee_rate,
            )
        )),

        other => Err(anyhow::anyhow!("Unknown strategy: {}", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_flat_allows_signal() {
        let mut f = SignalFilter::new(10, 0.15);
        assert!(f.can_signal(50000.0, 1_000_000));
    }

    #[test]
    fn test_filter_cooldown_blocks_without_price_move() {
        let mut f = SignalFilter::new(10, 0.15);
        f.on_signal(50000.0, 1_000_000);
        assert!(!f.can_signal(50000.0, 1_000_000));
        assert!(!f.can_signal(50025.0, 1_000_000 + 5_000));
    }

    #[test]
    fn test_filter_cooldown_expires_by_data_time() {
        let mut f = SignalFilter::new(10, 0.15);
        f.on_signal(50000.0, 1_000_000);
        assert!(f.can_signal(50000.0, 1_000_000 + 10_000));
    }

    #[test]
    fn test_filter_bypass_on_price_delta() {
        let mut f = SignalFilter::new(10, 0.15);
        f.on_signal(50000.0, 1_000_000);
        assert!(f.can_signal(50000.0 * 1.002, 1_000_000));
    }

    #[test]
    fn test_filter_in_position_blocks() {
        let mut f = SignalFilter::new(10, 0.15);
        f.on_position_opened();
        assert!(!f.can_signal(50000.0, 1_000_000));
        assert!(!f.can_signal(60000.0, 1_000_000));
    }

    #[test]
    fn test_filter_after_position_closed() {
        let mut f = SignalFilter::new(10, 0.15);
        f.on_position_opened();
        f.on_position_closed(2_000_000);
        assert!(!f.can_signal(50000.0, 2_000_000));
        assert!(f.can_signal(50000.0, 2_000_000 + 10_000));
    }
}