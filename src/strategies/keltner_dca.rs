//! keltner_dca.rs — Veles-style Keltner Pullback + DCA (5 orders) + Donchian TP/SL.
//!
//! Логика (Veles Finance gold bot):
//!   1. Keltner Channel: EMA(20) ± 2×ATR(10)
//!   2. Entry LONG: close ≤ lower Keltner band И price > EMA200 (uptrend pullback)
//!   3. DCA: 5 ордеров, каждый -1% от предыдущего
//!      Order 1: 30% planned qty @ first Keltner touch
//!      Order 2: 25% @ -1.0% from order 1
//!      Order 3: 20% @ -2.0%
//!      Order 4: 15% @ -3.5%
//!      Order 5: 10% @ -5.0%
//!   4. TP: Donchian 20-period high (пробой верхней Turtle zone)
//!   5. SL: close ниже Donchian 20-period low
//!   6. LONG-only (золото имеет bullish bias — central bank buying, inflation hedge)
//!
//! Veles benchmark: +10% за 3 месяца на $300 = +40%/год, Max DD -9%
//! Наш target: +15-25%/год на $200 (без DCA alpha, с DCA — выше)
//!
//! DCA state machine:
//!   Flat → Order1 → Order2 → Order3 → Order4 → Order5 → InPosition → Flat
//!   Каждый DCA order — отдельный signal. can_dca()=true → обходит has_position check.

use super::{Signal, SignalFilter, Strategy};
use crate::utils::math::{calc_position_size_with_fees, floor_to_step, round_to_tick};
use crate::websocket::WebSocketMessage;
use anyhow::Result;
use std::collections::VecDeque;

const TICK_SIZE: f64 = 0.01;
const QTY_STEP: f64 = 0.001;

/// Keltner EMA period
const KELTNER_EMA_PERIOD: usize = 20;
/// Keltner ATR multiplier
const KELTNER_ATR_MULT: f64 = 2.0;
/// ATR period for Keltner
const KELTNER_ATR_PERIOD: usize = 10;

/// Donchian channel period (Turtle zone)
const DONCHIAN_PERIOD: usize = 20;

/// DCA configuration
const DCA_MAX_ORDERS: usize = 5;
const DCA_DROP_STEPS: [f64; 5] = [0.0, -0.01, -0.02, -0.035, -0.05]; // % from order 1
const DCA_QTY_FRACTIONS: [f64; 5] = [0.30, 0.25, 0.20, 0.15, 0.10]; // 30+25+20+15+10=100%

/// EMA200 filter — only LONG above EMA200
const EMA200_PERIOD: usize = 200;
const EMA200_ALPHA: f64 = 2.0 / (EMA200_PERIOD as f64 + 1.0);

/// Min net RR after fees
const MIN_NET_RR: f64 = 1.0;

#[derive(Debug, Clone, Copy, PartialEq)]
enum DcaState {
    Flat,           // No position, waiting for Keltner touch
    Averaging,      // In DCA mode, placing orders 2-5
    FullyInvested,  // All 5 orders placed, waiting for TP/SL
}

pub struct KeltnerDcaStrategy {
    symbol: String,
    balance: f64,
    risk_per_trade_pct: f64,
    fee_rate: f64,

    // Candle history
    highs: VecDeque<f64>,
    lows: VecDeque<f64>,
    closes: VecDeque<f64>,
    volumes: VecDeque<f64>,
    timestamps: VecDeque<i64>,
    max_history: usize,

    // ATR
    atrs: VecDeque<f64>,

    // EMA for Keltner
    keltner_ema: Option<f64>,
    keltner_alpha: f64,

    // EMA200 filter
    ema200: Option<f64>,

    // DCA state
    dca_state: DcaState,
    dca_order_idx: usize,       // 0=not started, 1-5=order placed
    dca_order1_price: f64,      // price of first DCA order
    dca_total_qty: f64,        // accumulated qty across all DCA orders
    dca_avg_entry: f64,         // weighted avg entry price
    dca_sl_price: f64,          // SL price (Donchian low at time of first order)
    dca_tp_price: f64,          // TP price (Donchian high at time of first order)

    // Cooldown filter
    filter: SignalFilter,
    last_kline_ts: i64,

    // ✅ P39/F2: BUY-сигнал, отправленный движку, но ещё не подтверждённый
    // (исход Entry не получен). (dca_idx, qty, entry) — для отката мутации.
    pending_buy: Option<(usize, f64, f64)>,

    // Counters
    n_candles: u64,
    diag_first_call_done: bool,
    n_signals: u64,
    n_blocked_no_keltner: u64,
    n_blocked_below_ema200: u64,
    n_blocked_rr: u64,
    n_blocked_cooldown: u64,
    n_dca_orders_placed: u64,
    n_tp_hits: u64,
    n_sl_hits: u64,
    // ✅ P23: slippage-модель по живому bookTicker (аудит: «спред не моделируется»)
    last_bid: f64,
    last_ask: f64,
    max_spread_pct: f64,
    funding_pct: f64, // ✅ P24: оценка funding за круг (аудит №12)
    n_spread_block: u64,
    // ✅ P60/EQ-6: режимный множитель риска (SURVIVAL 0.5 / GROWTH 1.0)
    risk_multiplier: f64,
}

impl KeltnerDcaStrategy {
    pub fn new(symbol: String, balance: f64, risk_pct: f64, fee_rate: f64) -> Self {
        Self {
            symbol, balance, risk_per_trade_pct: risk_pct, fee_rate,
            highs: VecDeque::with_capacity(300),
            lows: VecDeque::with_capacity(300),
            closes: VecDeque::with_capacity(300),
            volumes: VecDeque::with_capacity(300),
            timestamps: VecDeque::with_capacity(300),
            max_history: 300,
            atrs: VecDeque::with_capacity(50),
            keltner_ema: None,
            keltner_alpha: 2.0 / (KELTNER_EMA_PERIOD as f64 + 1.0),
            ema200: None,
            dca_state: DcaState::Flat,
            dca_order_idx: 0,
            dca_order1_price: 0.0,
            dca_total_qty: 0.0,
            dca_avg_entry: 0.0,
            dca_sl_price: 0.0,
            dca_tp_price: 0.0,
            filter: SignalFilter::new(300, 0.5),
            last_kline_ts: 0,
            pending_buy: None, // ✅ P39/F2
            n_candles: 0, n_signals: 0,
            diag_first_call_done: false,
            n_blocked_no_keltner: 0, n_blocked_below_ema200: 0,
            n_blocked_rr: 0, n_blocked_cooldown: 0,
            n_dca_orders_placed: 0, n_tp_hits: 0, n_sl_hits: 0,
            last_bid: 0.0, last_ask: 0.0,
            max_spread_pct: std::env::var("P23_MAX_SPREAD_PCT")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .map(|v| if v > 0.0 { v } else { f64::INFINITY })
                .unwrap_or(0.0010),
            // ✅ P24: funding за круг (аудит №12) — консервативно один интервал 8h,
            // доля от цены; дефолт 0.0001 = 0.01%/8h; мусор/<=0 в env -> 0.0 (выключено)
            funding_pct: std::env::var("P24_FUNDING_PCT")
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| *v > 0.0)
                .unwrap_or(0.0001),
            n_spread_block: 0,
            // ✅ P60/EQ-6: старт в SURVIVAL (как RiskManager, ЭТАП C)
            risk_multiplier: 0.5,
        }
    }

    /// ✅ P23: slippage-guard — true = спред шире лимита, вход запрещён
    fn spread_too_wide(&mut self) -> bool {
        if self.last_bid > 0.0 && self.last_ask > self.last_bid {
            let mid = (self.last_bid + self.last_ask) * 0.5;
            if (self.last_ask - self.last_bid) / mid > self.max_spread_pct {
                self.n_spread_block += 1;
                return true;
            }
        }
        false
    }

    /// Update ATR (Wilder's method)
    fn update_atr(&mut self) {
        let len = self.closes.len();
        if len < 2 { return; }
        let high = self.highs[len - 1];
        let low = self.lows[len - 1];
        let prev_close = self.closes[len - 2];
        let tr = (high - low).max((high - prev_close).abs()).max((low - prev_close).abs());
        let atr = if let Some(&prev_atr) = self.atrs.back() {
            let n = KELTNER_ATR_PERIOD as f64;
            (prev_atr * (n - 1.0) + tr) / n
        } else {
            tr
        };
        self.atrs.push_back(atr);
        if self.atrs.len() > KELTNER_ATR_PERIOD * 3 { self.atrs.pop_front(); }
    }

    fn current_atr(&self) -> f64 {
        self.atrs.back().copied().unwrap_or(0.0)
    }

    /// Update Keltner EMA(20)
    fn update_keltner_ema(&mut self, close: f64) {
        if let Some(prev) = self.keltner_ema {
            self.keltner_ema = Some(close * self.keltner_alpha + prev * (1.0 - self.keltner_alpha));
        } else if self.closes.len() >= KELTNER_EMA_PERIOD {
            let sum: f64 = self.closes.iter().take(KELTNER_EMA_PERIOD).sum();
            self.keltner_ema = Some(sum / KELTNER_EMA_PERIOD as f64);
        }
    }

    /// Update EMA200
    fn update_ema200(&mut self, close: f64) {
        if let Some(prev) = self.ema200 {
            self.ema200 = Some(close * EMA200_ALPHA + prev * (1.0 - EMA200_ALPHA));
        } else if self.closes.len() >= EMA200_PERIOD {
            let sum: f64 = self.closes.iter().take(EMA200_PERIOD).sum();
            self.ema200 = Some(sum / EMA200_PERIOD as f64);
        }
    }

    /// Keltner lower band = EMA(20) - mult × ATR(10)
    fn keltner_lower(&self) -> Option<f64> {
        let ema = self.keltner_ema?;
        let atr = self.current_atr();
        if atr <= 0.0 { return None; }
        Some(ema - KELTNER_ATR_MULT * atr)
    }

    /// Donchian high (N-period) — для TP
    fn donchian_high(&self) -> Option<f64> {
        if self.highs.len() < DONCHIAN_PERIOD { return None; }
        let start = self.highs.len() - DONCHIAN_PERIOD;
        self.highs.iter().skip(start).fold(None, |acc, &h| {
            Some(acc.map_or(h, |m: f64| m.max(h)))
        })
    }

    /// Donchian low (N-period) — для SL
    fn donchian_low(&self) -> Option<f64> {
        if self.lows.len() < DONCHIAN_PERIOD { return None; }
        let start = self.lows.len() - DONCHIAN_PERIOD;
        self.lows.iter().skip(start).fold(None, |acc, &l| {
            Some(acc.map_or(l, |m: f64| m.min(l)))
        })
    }

    /// Check if price touched/below Keltner lower band
    fn keltner_touch(&self, close: f64) -> bool {
        if let Some(lower) = self.keltner_lower() {
            return close <= lower;
        }
        false
    }

    /// Check if price is above EMA200 (uptrend filter)
    fn is_above_ema200(&self, close: f64) -> bool {
        match self.ema200 {
            Some(ema) => close > ema,
            None => false,
        }
    }

    /// Check if DCA order should be placed at current price
    fn should_place_dca_order(&self, close: f64) -> Option<usize> {
        match self.dca_state {
            DcaState::Flat => {
                // First order: Keltner touch + above EMA200
                if self.keltner_touch(close) && self.is_above_ema200(close) {
                    return Some(0); // index 0 = first order
                }
                None
            }
            DcaState::Averaging => {
                // Subsequent orders: price dropped DCA_DROP_STEPS[idx]% from order 1
                let next_idx = self.dca_order_idx; // 0-based: next order index
                if next_idx >= DCA_MAX_ORDERS { return None; }
                let drop_pct = DCA_DROP_STEPS[next_idx];
                let target_price = self.dca_order1_price * (1.0 + drop_pct);
                if close <= target_price {
                    return Some(next_idx);
                }
                None
            }
            DcaState::FullyInvested => None,
        }
    }

    /// Calculate position size for a DCA order
    fn calc_dca_qty(&self, entry: f64, sl: f64, fraction: f64) -> f64 {
        // Full position size based on risk
        let full_qty = calc_position_size_with_fees(
            self.balance, entry, sl,
            // ✅ P60/EQ-6: риск × режимный множитель (SURVIVAL/GROWTH)
            self.risk_per_trade_pct * self.risk_multiplier, self.fee_rate * 2.0,
        );
        // This DCA order gets 'fraction' of full size
        let dca_qty = full_qty * fraction;
        floor_to_step(dca_qty, QTY_STEP)
    }

    /// Check TP/SL for the ladder
    /// ✅ P39/F3: стратегические выходы живут и при недостроенной лестнице.
    /// Раньше check_exit возвращал None до FullyInvested, а dca_sl_price /
    /// dca_tp_price записывались, но нигде не читались (аудит F3/H13) —
    /// у лестницы не было собственного стопа до полной сборки.
    fn check_exit(&self, close: f64) -> Option<&'static str> {
        if self.dca_state == DcaState::Flat {
            return None;
        }
        // ✅ P39/F3: жёсткий стратегический стоп — SL первой транши.
        // Дегенерат sl >= entry (round_to_tick) отсечён — иначе мгновенный
        // ложный выход на первой же свече.
        if self.dca_sl_price > 0.0
            && self.dca_order1_price > 0.0
            && self.dca_sl_price < self.dca_order1_price
            && close <= self.dca_sl_price
        {
            return Some("StopLoss");
        }
        // ✅ P39/F3: цена вернулась к TP, пока лестница недостроена —
        // фиксируем прибыль по накопленным траншам, глубже не добираем.
        if self.dca_state == DcaState::Averaging
            && self.dca_tp_price > 0.0
            && self.dca_order1_price > 0.0
            && self.dca_tp_price > self.dca_order1_price
            && close >= self.dca_tp_price
        {
            return Some("TakeProfit");
        }
        if self.dca_state != DcaState::FullyInvested {
            return None;
        }
        // TP: close >= Donchian high
        if let Some(dh) = self.donchian_high() {
            if close >= dh {
                return Some("TakeProfit");
            }
        }
        // SL: close <= Donchian low
        if let Some(dl) = self.donchian_low() {
            if close <= dl {
                return Some("StopLoss");
            }
        }
        None
    }

    /// Reset DCA state after position close
    fn reset_dca(&mut self) {
        self.dca_state = DcaState::Flat;
        self.dca_order_idx = 0;
        self.dca_order1_price = 0.0;
        self.dca_total_qty = 0.0;
        self.dca_avg_entry = 0.0;
        self.dca_sl_price = 0.0;
        self.dca_tp_price = 0.0;
    }
}

impl Strategy for KeltnerDcaStrategy {
    fn name(&self) -> &str { "keltner_dca" }

    // ✅ V3.4: backfill греет индикаторы (Keltner EMA/ATR/EMA200/Donchian),
    // иначе ema200 на 4h-свечах ждал бы 200 живых закрытий = 33 дня.
    fn wants_backfill(&self) -> bool { true }

    /// 🆕 DCA: returns true — allows multiple entries on same symbol
    fn can_dca(&self) -> bool { true }

    fn notify_position_open(&mut self) {
        // ✅ P39/F2: транша подтверждена биржей — откат не нужен
        self.pending_buy = None;
        // Position opened — update DCA state
        if self.dca_state == DcaState::Flat {
            self.dca_state = DcaState::Averaging;
        }
        self.dca_order_idx += 1;
        self.n_dca_orders_placed += 1;
        if self.dca_order_idx >= DCA_MAX_ORDERS {
            self.dca_state = DcaState::FullyInvested;
        }
    }

    // ✅ P39/F2: откат оптимистичной мутации лестницы при неудаче входа.
    // process() увеличивает dca_total_qty/avg_entry ДО исполнения; без отката
    // при Missed/гейтах (KILL, suspend, риск) счётчики росли без ордеров
    // (аудит N2/F2), а выходной SELL нёс завышенный объём (H2).
    fn on_entry_failed(&mut self, _reason: &str) {
        if let Some((idx, qty, entry)) = self.pending_buy.take() {
            if idx == 0 {
                // первая транша не подтвердилась — лестница не начиналась
                self.reset_dca();
            } else {
                self.dca_total_qty -= qty;
                if self.dca_total_qty <= f64::EPSILON {
                    self.reset_dca();
                } else {
                    self.dca_avg_entry = (self.dca_avg_entry * (self.dca_total_qty + qty)
                        - entry * qty)
                        / self.dca_total_qty;
                }
            }
        }
    }

    // ✅ P39/F2: судьба ордера неизвестна — консервативный откат
    // (периодическая сверка восстановит фактическое состояние).
    fn on_position_unknown(&mut self) {
        self.on_entry_failed("position unknown");
    }

    // P16: sizing base follows live equity
    fn set_live_equity(&mut self, eq: f64) { self.balance = eq; }

    // ✅ P60/EQ-6: режимный множитель из RiskManager (ЭТАП C)
    fn set_risk_multiplier(&mut self, mult: f64) {
        if mult.is_finite() && mult > 0.0 {
            self.risk_multiplier = mult;
        }
    }

    fn notify_position_closed(&mut self, _pnl: f64) {
        self.reset_dca();
        self.filter.on_position_closed(self.last_kline_ts);
    }

    fn in_position(&self) -> bool {
        // In DCA mode: NOT in_position (allows more entries via can_dca bypass)
        // Only "in position" when fully invested
        match self.dca_state {
            DcaState::Flat => false,
            DcaState::Averaging => false, // ← KEY: allows DCA orders past has_position
            DcaState::FullyInvested => true,
        }
    }

    fn process(&mut self, msg: &WebSocketMessage) -> Result<Option<Signal>> {
        // ✅ P23: ловим живой спред из bookTicker (guard входа + стоимость спреда в RR)
        if let WebSocketMessage::BookTicker(b) = msg {
            if b.symbol.eq_ignore_ascii_case(&self.symbol)
                && b.best_bid > 0.0
                && b.best_ask > b.best_bid
            {
                self.last_bid = b.best_bid;
                self.last_ask = b.best_ask;
            }
            return Ok(None);
        }
        let kline = match msg { WebSocketMessage::Kline(k) => k, _ => return Ok(None) };

        // ✅ DIAG v3.4.1: одна запись о первой свече, дошедшей до стратегии
        if !self.diag_first_call_done {
            self.diag_first_call_done = true;
            tracing::info!(
                "🔍 KeltnerDCA DIAG: 1-я свеча дошла до стратегии | self.sym={} | msg.sym={} | is_closed={} | is_backfill={} | ts={}",
                self.symbol, kline.symbol, kline.is_closed, kline.is_backfill, kline.timestamp
            );
        }

        if !kline.symbol.eq_ignore_ascii_case(&self.symbol) { return Ok(None); }
        if !kline.is_closed { return Ok(None); }
        if kline.timestamp == self.last_kline_ts { return Ok(None); }

        self.last_kline_ts = kline.timestamp;
        self.n_candles += 1;

        // Accumulate history
        self.highs.push_back(kline.high);
        self.lows.push_back(kline.low);
        self.closes.push_back(kline.close);
        self.volumes.push_back(kline.volume);
        self.timestamps.push_back(kline.timestamp);
        while self.highs.len() > self.max_history {
            self.highs.pop_front(); self.lows.pop_front();
            self.closes.pop_front(); self.volumes.pop_front();
            self.timestamps.pop_front();
        }

        // Update indicators
        self.update_atr();
        self.update_keltner_ema(kline.close);
        self.update_ema200(kline.close);

        // ✅ V3.4: backfill — только прогрев индикаторов. Входы/выходы
        // (EXIT CHECK, DCA-ордера) по историческим ценам запрещены.
        if kline.is_backfill { return Ok(None); }

        // === EXIT CHECK (if fully invested) ===
        if let Some(reason) = self.check_exit(kline.close) {
            if !self.filter.can_signal(kline.close, kline.timestamp) {
                self.n_blocked_cooldown += 1;
                return Ok(None);
            }
            self.filter.on_signal(kline.close, kline.timestamp);
            self.n_signals += 1;
            if reason == "TakeProfit" { self.n_tp_hits += 1; }
            else { self.n_sl_hits += 1; }

            let side = "SELL"; // LONG-only → exit is always SELL
            let entry = round_to_tick(kline.close, TICK_SIZE);
            tracing::info!(
                "🎯 KeltnerDCA EXIT {} | reason={} | avg_entry={:.4} close={:.4} qty={:.6}",
                side, reason, self.dca_avg_entry, entry, self.dca_total_qty,
            );
            return Ok(Some(Signal {
                symbol: kline.symbol.clone(),
                side: side.to_string(),
                price: entry,
                quantity: self.dca_total_qty,
                strategy_name: "Exit_KeltnerDca".to_string(),
                leverage: 1.0,
                stop_loss: None,
                take_profit: None,
                atr_hint: self.current_atr(),
                entry_spread: 0.0,
            }));
        }

        // === ENTRY / DCA CHECK ===
        let close = kline.close;

        // Check if we should place a DCA order
        let dca_idx = match self.should_place_dca_order(close) {
            Some(idx) => idx,
            None => {
                if self.dca_state == DcaState::Flat {
                    // Check why we didn't enter
                    if !self.keltner_touch(close) {
                        self.n_blocked_no_keltner += 1;
                    } else if !self.is_above_ema200(close) {
                        self.n_blocked_below_ema200 += 1;
                    }
                }
                return Ok(None);
            }
        };

        // Donchian levels for SL/TP
        let don_low = match self.donchian_low() {
            Some(dl) => dl,
            None => return Ok(None),
        };
        let don_high = match self.donchian_high() {
            Some(dh) => dh,
            None => return Ok(None),
        };

        // Entry price for this DCA order
        let entry = round_to_tick(close, TICK_SIZE);
        let sl = round_to_tick(don_low, TICK_SIZE);
        let tp = round_to_tick(don_high, TICK_SIZE);

        // ✅ P23: slippage-guard — спред шире лимита = вход запрещён
        if self.spread_too_wide() {
            return Ok(None);
        }
        // Net RR check
        let fee_cost = 2.0 * self.fee_rate * entry;
        // ✅ P23: полная стоимость спреда за круг (вход по ask, выход по bid).
        // До первого bookTicker спред неизвестен — модель как раньше.
        let spread_cost = if self.last_bid > 0.0 && self.last_ask > self.last_bid {
            self.last_ask - self.last_bid
        } else {
            0.0
        };
        let gross_profit = tp - entry - spread_cost;
        let gross_loss = entry - sl + spread_cost;
        // ✅ P24: funding за круг (аудит №12) — консервативно один интервал 8h
        let funding_cost = entry * self.funding_pct;
        let net_profit = gross_profit - fee_cost - funding_cost;
        let net_loss = gross_loss + fee_cost + funding_cost;
        if net_loss <= 0.0 || net_profit <= 0.0 {
            self.n_blocked_rr += 1;
            return Ok(None);
        }
        let eff_rr = net_profit / net_loss;
        if eff_rr < MIN_NET_RR {
            self.n_blocked_rr += 1;
            return Ok(None);
        }

        // Position size for this DCA order
        let fraction = DCA_QTY_FRACTIONS[dca_idx];
        let qty = self.calc_dca_qty(entry, sl, fraction);
        if qty <= 0.0 { return Ok(None); }

        // Update DCA state
        if dca_idx == 0 {
            // First order — record reference price and SL/TP
            self.dca_order1_price = entry;
            self.dca_sl_price = sl;
            self.dca_tp_price = tp;
        }

        // Update accumulated position
        let new_total_qty = self.dca_total_qty + qty;
        let new_avg_entry = if self.dca_total_qty > 0.0 {
            (self.dca_avg_entry * self.dca_total_qty + entry * qty) / new_total_qty
        } else {
            entry
        };
        self.dca_total_qty = new_total_qty;
        self.dca_avg_entry = new_avg_entry;

        self.filter.on_signal(entry, kline.timestamp);
        self.n_signals += 1;
        // ✅ P39/F2: запоминаем траншу до подтверждения — при сбое
        // on_entry_failed откатит dca_total_qty/avg_entry (сейчас мутация
        // остаётся навсегда, выходной SELL нёс завышенный объём — H2).
        self.pending_buy = Some((dca_idx, qty, entry));

        tracing::info!(
            "🎯 KeltnerDCA BUY order {}/{} | entry={:.4} qty={:.6} | avg_entry={:.4} total_qty={:.6} | SL={:.4} TP={:.4} netRR={:.2}",
            dca_idx + 1, DCA_MAX_ORDERS, entry, qty,
            self.dca_avg_entry, self.dca_total_qty,
            self.dca_sl_price, self.dca_tp_price, eff_rr,
        );

        Ok(Some(Signal {
            symbol: kline.symbol.clone(),
            side: "BUY".to_string(),
            price: entry,
            quantity: qty,
            strategy_name: self.name().to_string(),
            leverage: 1.0,
            stop_loss: Some(sl),
            take_profit: Some(tp),
            atr_hint: self.current_atr(),
            entry_spread: 0.0,
        }))
    }

    fn diagnostics(&self) -> Option<String> {
        Some(format!(
            "KeltnerDCA(candles) diag | candles={} signals={} dca_orders={} | no_keltner={} below_ema200={} rr_block={} spr_block={} cooldown={} | TP={} SL={} | state={:?} dca_idx={} avg_entry={} total_qty={} | keltner_ema={} ema200={} atr={}",
            self.n_candles, self.n_signals, self.n_dca_orders_placed,
            self.n_blocked_no_keltner, self.n_blocked_below_ema200,
            self.n_blocked_rr, self.n_spread_block, self.n_blocked_cooldown,
            self.n_tp_hits, self.n_sl_hits,
            self.dca_state, self.dca_order_idx,
            if self.dca_avg_entry > 0.0 { format!("{:.4}", self.dca_avg_entry) } else { "n/a".to_string() },
            if self.dca_total_qty > 0.0 { format!("{:.6}", self.dca_total_qty) } else { "0".to_string() },
            self.keltner_ema.map(|v| format!("{:.4}", v)).unwrap_or_else(|| "n/a".to_string()),
            self.ema200.map(|v| format!("{:.4}", v)).unwrap_or_else(|| "n/a".to_string()),
            self.current_atr(),
        ))
    }
}
