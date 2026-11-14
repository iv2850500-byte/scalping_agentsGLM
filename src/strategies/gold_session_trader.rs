//! gold_session_trader.rs — торговля золотом (XAUUSDT) по London session.
//!
//! Логика:
//!   1. Time filter: торгуем ТОЛЬКО свечи закрытые в 10:00 и 14:00 UTC
//!      10:00 = London morning (институциональный приток)
//!      14:00 = pre-US open (макро-данные США)
//!   2. Marubozu detector: body > 70% of range (чистый импульс, без теней)
//!   3. Volume spike: volume > 2× среднего за 20 свечей
//!   4. Direction: в сторону body (бычья Marubozu → LONG, медвежья → SHORT)
//!   5. SL: low/high свечи Marubozu (очень короткий)
//!   6. TP: 3× SL distance (RR = 3.0)
//!   7. Maker entry (limit ордер)
//!
//! Математика (XAUUSDT 1h):
//!   ATR(14) ≈ 0.30% per 1h
//!   Marubozu SL ≈ 0.20-0.40% (короткий, без теней)
//!   TP = 3× SL ≈ 0.60-1.20%
//!   Round-trip fee: maker 0.02% + taker SL 0.05% = 0.07%
//!   Net RR = (0.90 - 0.07) / (0.30 + 0.07) = 0.83/0.37 = 2.24
//!   Breakeven WR = 1/(1+2.24) = 30.9%
//!   Целевая WR: 40-50% (институциональный momentum = высокая точность)
//!   2 trades/day × 250 days = 500 trades/year
//!   EV = +0.18 USDT/trade × 500 = +90 USDT/год на $200 = +45% годовых

use super::{Signal, SignalFilter, Strategy};
use crate::utils::math::{calc_position_size_with_fees, floor_to_step, round_to_tick};
use crate::websocket::WebSocketMessage;
use anyhow::Result;
use std::collections::VecDeque;

const TICK_SIZE: f64 = 0.00000001;
const QTY_STEP: f64 = 0.00000001;

/// Часы UTC для торговли (London session key times)
const TRADE_HOURS_UTC: &[u32] = &[10, 14];

/// Минимальное отношение body к range для Marubozu (70%)
const MARUBOZU_BODY_RATIO: f64 = 0.70;

/// Минимальный volume spike (2× среднего)
const VOLUME_SPIKE_MULT: f64 = 2.0;

/// Окно для среднего volume (20 свечей)
const VOLUME_WINDOW: usize = 20;

/// RR ratio (TP = 3× SL)
const RR_RATIO: f64 = 3.0;

/// Минимальный Net RR после fees
const MIN_NET_RR: f64 = 1.5;

/// Минимальная длина body свечи (в % от цены) для валидности сигнала
const MIN_BODY_PCT: f64 = 0.15;

pub struct GoldSessionStrategy {
    symbol: String,
    balance: f64,
    risk_per_trade_pct: f64,
    fee_rate: f64,

    // История свечей
    highs: VecDeque<f64>,
    lows: VecDeque<f64>,
    opens: VecDeque<f64>,
    closes: VecDeque<f64>,
    volumes: VecDeque<f64>,
    timestamps: VecDeque<i64>,
    max_history: usize,

    // FSM
    filter: SignalFilter,
    last_kline_ts: i64,

    // Счётчики
    n_candles: u64,
    n_signals: u64,
    n_blocked_not_trade_hour: u64,
    n_blocked_no_marubozu: u64,
    n_blocked_low_volume: u64,
    n_blocked_net_rr: u64,
    n_blocked_small_body: u64,
}

impl GoldSessionStrategy {
    pub fn new(symbol: String, balance: f64, risk_pct: f64, fee_rate: f64) -> Self {
        Self {
            symbol, balance, risk_per_trade_pct: risk_pct, fee_rate,
            highs: VecDeque::with_capacity(200), lows: VecDeque::with_capacity(200),
            opens: VecDeque::with_capacity(200), closes: VecDeque::with_capacity(200),
            volumes: VecDeque::with_capacity(200), timestamps: VecDeque::with_capacity(200),
            max_history: 200,
            filter: SignalFilter::new(300, 0.10), // 5 min cooldown
            last_kline_ts: 0,
            n_candles: 0, n_signals: 0,
            n_blocked_not_trade_hour: 0, n_blocked_no_marubozu: 0,
            n_blocked_low_volume: 0, n_blocked_net_rr: 0, n_blocked_small_body: 0,
        }
    }

    /// Проверка: закрылась ли свеча в торговый час (10:00 или 14:00 UTC)
    fn is_trade_hour(&self, ts_ms: i64) -> bool {
        let secs_of_day = ((ts_ms / 1000) % 86_400) as u32;
        let hour = secs_of_day / 3600;
        TRADE_HOURS_UTC.contains(&hour)
    }

    /// Marubozu detector: body > 70% of range
    fn is_marubozu(&self, open: f64, high: f64, low: f64, close: f64) -> bool {
        let range = high - low;
        if range <= 0.0 { return false; }
        let body = (close - open).abs();
        let body_ratio = body / range;
        body_ratio >= MARUBOZU_BODY_RATIO
    }

    /// Volume spike: текущий volume > VOLUME_SPIKE_MULT × среднего
    fn is_volume_spike(&self) -> bool {
        if self.volumes.len() < VOLUME_WINDOW + 1 { return false; }
        let current = *self.volumes.back().unwrap();
        let sum: f64 = self.volumes.iter().rev().skip(1).take(VOLUME_WINDOW).sum();
        let avg = sum / VOLUME_WINDOW as f64;
        if avg <= 0.0 { return false; }
        current >= avg * VOLUME_SPIKE_MULT
    }

    /// Расчёт net RR после fees
    fn net_rr(&self, entry: f64, sl: f64, tp: f64, is_long: bool) -> Option<f64> {
        let fee_cost = 2.0 * self.fee_rate * entry;
        let (gross_profit, gross_loss) = if is_long {
            (tp - entry, entry - sl)
        } else {
            (entry - tp, sl - entry)
        };
        let net_profit = gross_profit - fee_cost;
        let net_loss = gross_loss + fee_cost;
        if net_loss <= 0.0 || net_profit <= 0.0 { return None; }
        Some(net_profit / net_loss)
    }

    /// Средний volume для диагностики
    fn avg_volume(&self) -> f64 {
        if self.volumes.len() < VOLUME_WINDOW { return 0.0; }
        let sum: f64 = self.volumes.iter().rev().take(VOLUME_WINDOW).sum();
        sum / VOLUME_WINDOW as f64
    }
}

impl Strategy for GoldSessionStrategy {
    fn name(&self) -> &str { "GoldSession" }

    fn notify_position_open(&mut self) { self.filter.on_position_opened(); }
    fn notify_position_closed(&mut self, _pnl: f64) { self.filter.on_position_closed(self.last_kline_ts); }

    fn process(&mut self, msg: &WebSocketMessage) -> Result<Option<Signal>> {
        let kline = match msg { WebSocketMessage::Kline(k) => k, _ => return Ok(None) };
        if !kline.symbol.eq_ignore_ascii_case(&self.symbol) { return Ok(None); }
        if !kline.is_closed { return Ok(None); }
        if kline.timestamp == self.last_kline_ts { return Ok(None); }

        self.last_kline_ts = kline.timestamp;
        self.n_candles += 1;

        // 1. Накапливаем историю
        self.highs.push_back(kline.high);
        self.lows.push_back(kline.low);
        self.opens.push_back(kline.open);
        self.closes.push_back(kline.close);
        self.volumes.push_back(kline.volume);
        self.timestamps.push_back(kline.timestamp);
        while self.highs.len() > self.max_history {
            self.highs.pop_front(); self.lows.pop_front();
            self.opens.pop_front(); self.closes.pop_front();
            self.volumes.pop_front(); self.timestamps.pop_front();
        }

        // 2. FSM: не торгуем если в позиции или cooldown
        let price = kline.close;
        if !self.filter.can_signal(price, kline.timestamp) { return Ok(None); }

        // 3. Time filter: только 10:00 и 14:00 UTC
        if !self.is_trade_hour(kline.timestamp) {
            self.n_blocked_not_trade_hour += 1;
            return Ok(None);
        }

        // 4. Marubozu detector
        let open = kline.open;
        let high = kline.high;
        let low = kline.low;
        let close = kline.close;

        if !self.is_marubozu(open, high, low, close) {
            self.n_blocked_no_marubozu += 1;
            return Ok(None);
        }

        // 5. Body size check (минимум 0.15% от цены)
        let body_pct = (close - open).abs() / close * 100.0;
        if body_pct < MIN_BODY_PCT {
            self.n_blocked_small_body += 1;
            return Ok(None);
        }

        // 6. Volume spike
        if !self.is_volume_spike() {
            self.n_blocked_low_volume += 1;
            return Ok(None);
        }

        // 7. Direction: в сторону body
        let is_long = close > open;
        let (side, sl_price, tp_price) = if is_long {
            // Bullish Marubozu → LONG
            let sl = low; // SL на low свечи
            let sl_dist = (price - sl).abs().max(price * 0.002); // min 0.2%
            let sl = price - sl_dist;
            let tp = price + sl_dist * RR_RATIO;
            ("BUY", sl, tp)
        } else {
            // Bearish Marubozu → SHORT
            let sl = high; // SL на high свечи
            let sl_dist = (sl - price).abs().max(price * 0.002);
            let sl = price + sl_dist;
            let tp = price - sl_dist * RR_RATIO;
            ("SELL", sl, tp)
        };

        let entry = round_to_tick(price, TICK_SIZE);
        let sl = round_to_tick(sl_price, TICK_SIZE);
        let tp = round_to_tick(tp_price, TICK_SIZE);

        // 8. Net RR check
        let eff_rr = match self.net_rr(entry, sl, tp, is_long) {
            Some(rr) => rr,
            None => return Ok(None),
        };
        if eff_rr < MIN_NET_RR {
            self.n_blocked_net_rr += 1;
            return Ok(None);
        }

        // 9. Position sizing
        let raw_qty = calc_position_size_with_fees(
            self.balance, entry, sl, self.risk_per_trade_pct, self.fee_rate * 2.0,
        );
        let quantity = floor_to_step(raw_qty, QTY_STEP);
        if quantity <= 0.0 { return Ok(None); }

        // 10. Signal!
        self.filter.on_signal(entry, kline.timestamp);
        self.n_signals += 1;
        tracing::info!(
            "GOLD {} | body={:.3}% vol_spike={:.1}× entry={:.4} SL={:.4} TP={:.4} qty={:.6} netRR={:.2}",
            side, body_pct, kline.volume / self.avg_volume(), entry, sl, tp, quantity, eff_rr
        );

        Ok(Some(Signal {
            symbol: kline.symbol.clone(),
            side: side.to_string(),
            price: entry,
            quantity,
            strategy_name: self.name().to_string(),
            leverage: 1.0,
            stop_loss: Some(sl),
            take_profit: Some(tp),
            atr_hint: 0.0,
            entry_spread: 0.0,
        }))
    }

    fn diagnostics(&self) -> Option<String> {
        Some(format!(
            "GoldSession(candles) diag | candles={} signals={} | not_trade_hour={} no_marubozu={} small_body={} low_vol={} net_rr_block={} | avg_vol={:.0} | rr={} min_net_rr={} fee={:.4}%",
            self.n_candles, self.n_signals,
            self.n_blocked_not_trade_hour, self.n_blocked_no_marubozu,
            self.n_blocked_small_body, self.n_blocked_low_volume, self.n_blocked_net_rr,
            self.avg_volume(), RR_RATIO, MIN_NET_RR, self.fee_rate * 100.0,
        ))
    }

    fn in_position(&self) -> bool {
        // ✅ P21: Cooldown больше НЕ считается «в позиции».
        // Фильтр уходит в Cooldown в момент ВЫДАЧИ сигнала (on_signal) — это
        // анти-флап. Раньше in_position()=true на свежевыданном сигнале
        // мгновенно само-блокировал его в main.rs («позиция уже открыта»),
        // и стратегия не могла войти никогда. Anti-flap остаётся в can_signal().
        matches!(self.filter.state(), super::PositionState::InPosition)
    }
}
