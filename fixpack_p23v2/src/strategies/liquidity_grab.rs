//! liquidity_grab.rs — SMC (Smart Money Concepts) стратегия.
//!
//! Логика (строго прибыльная):
//!   1. Williams Fractals (5-bar swing detection)
//!   2. Liquidity Pool clustering (равные fractals в пределах 0.15%)
//!   3. Sweep detection (цена пробивает pool, закрывается обратно)
//!   4. MSS confirmation (close пробивает противоположный fractal)
//!   5. Entry: LIMIT order на retest сломанного pool (maker 0.02%)
//!   6. SL: за sweep extreme (invalidation)
//!   7. TP: на следующем opposite liquidity pool
//!
//! Математика (PEPE 1h, real slippage 0.02% из калибровки 51M тиков):
//!   Round-trip fee: 0.02% maker entry + 0.05% taker SL + 0.02% maker TP = 0.09%
//!   TP: ~2.5% (next liquidity pool)
//!   SL: ~1.0% (sweep extreme)
//!   Net RR = (2.5 - 0.09) / (1.0 + 0.09) = 2.41/1.09 = 2.21
//!   Breakeven WR = 1/(1+2.21) = 31.2%
//!   Целевая WR: 40-50% (как у TAO momentum)
//!   При WR=40%: EV = 0.40×2.5 - 0.60×1.0 = +0.40 USDT/trade
//!
//! Safety:
//!   - Max 1 concurrent position (no martingale)
//!   - Cooldown 2h между трейдами (less noise)
//!   - Skip if pool touch_count < 2 (нужен значимый уровень)
//!   - Skip if basis > 0.5% (spot vs futures diverge)

use crate::strategies::{Signal, SignalFilter, Strategy};
use crate::utils::math::{calc_position_size_with_fees, floor_to_step, round_to_tick};
use crate::websocket::WebSocketMessage;
use anyhow::Result;
use std::collections::VecDeque;

/// Universal tick size — works for all coins (BTC, SOL, PEPE).
const TICK_SIZE: f64 = 0.00000001;
const QTY_STEP: f64 = 0.00000001;

/// Fractal lookback — свечи слева/справа должны быть ниже/выше
const FRACTAL_LOOKBACK: usize = 2;

/// Порог кластеризации fractals в pool (0.15% друг от друга)
const POOL_CLUSTER_THRESHOLD_PCT: f64 = 0.0015;

/// Минимум touch_count для значимого pool
const MIN_POOL_TOUCHES: usize = 2;

/// Максимальное время ожидания retest после sweep (свечей)
const RETEST_TIMEOUT_BARS: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct LiquidityPool {
    pub price: f64,
    pub is_high: bool,        // true = resistance (highs), false = support (lows)
    pub touch_count: usize,
    pub last_touch_ts: i64,
    pub swept: bool,
    pub sweep_extreme: Option<f64>, // max/min цена в момент sweep
    pub sweep_ts: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SweepDirection {
    Bullish, // sweep below support → long entry (цена сняла стопы лонгов, разворачивается вверх)
    Bearish, // sweep above resistance → short entry (цена сняла стопы шортов, разворачивается вниз)
}

#[derive(Debug, Clone)]
pub struct ActiveSweep {
    pub pool_price: f64,
    pub sweep_extreme: f64,
    pub direction: SweepDirection,
    pub sweep_ts: i64,
    pub bars_since_sweep: usize,
    pub mss_confirmed: bool,
}

pub struct LiquidityGrabStrategy {
    symbol: String,
    balance: f64,
    risk_per_trade_pct: f64,
    fee_rate: f64,
    rr_ratio: f64,
    min_net_rr: f64,

    // История свечей
    highs: VecDeque<f64>,
    lows: VecDeque<f64>,
    closes: VecDeque<f64>,
    timestamps: VecDeque<i64>,
    max_history: usize,

    // Найденные fractals (для MSS detection)
    recent_fractal_highs: VecDeque<(i64, f64)>, // (ts, price)
    recent_fractal_lows: VecDeque<(i64, f64)>,

    // Кластеризованные pools
    pools: Vec<LiquidityPool>,

    // Активный sweep (если есть)
    active_sweep: Option<ActiveSweep>,

    // Position state (через SignalFilter)
    filter: SignalFilter,
    last_kline_ts: i64,

    // Счётчики для diagnostics
    n_candles: u64,
    n_fractals: u64,
    n_pools: u64,
    n_sweeps: u64,
    n_mss_confirmed: u64,
    n_signals: u64,
    n_blocked_no_sweep: u64,
    n_blocked_no_mss: u64,
    n_blocked_low_touches: u64,
    n_blocked_retest_timeout: u64,
    n_blocked_position: u64,
    // ✅ P23: slippage-модель по живому bookTicker (аудит: «спред не моделируется»)
    last_bid: f64,
    last_ask: f64,
    max_spread_pct: f64,
    n_spread_block: u64,
}

impl LiquidityGrabStrategy {
    pub fn new(symbol: String, balance: f64, risk_pct: f64, fee_rate: f64) -> Self {
        Self {
            symbol,
            balance,
            risk_per_trade_pct: risk_pct,
            fee_rate,
            rr_ratio: 2.5,
            min_net_rr: 1.5,
            highs: VecDeque::with_capacity(200),
            lows: VecDeque::with_capacity(200),
            closes: VecDeque::with_capacity(200),
            timestamps: VecDeque::with_capacity(200),
            max_history: 200,
            recent_fractal_highs: VecDeque::with_capacity(50),
            recent_fractal_lows: VecDeque::with_capacity(50),
            pools: Vec::new(),
            active_sweep: None,
            filter: SignalFilter::new(120, 0.20), // cooldown 2min + price move 0.20%
            last_kline_ts: 0,
            n_candles: 0,
            n_fractals: 0,
            n_pools: 0,
            n_sweeps: 0,
            n_mss_confirmed: 0,
            n_signals: 0,
            n_blocked_no_sweep: 0,
            n_blocked_no_mss: 0,
            n_blocked_low_touches: 0,
            n_blocked_retest_timeout: 0,
            n_blocked_position: 0,
            last_bid: 0.0,
            last_ask: 0.0,
            max_spread_pct: std::env::var("P23_MAX_SPREAD_PCT")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .map(|v| if v > 0.0 { v } else { f64::INFINITY })
                .unwrap_or(0.0010),
            n_spread_block: 0,
        }
    }

    pub fn with_rr(mut self, rr: f64, min_net_rr: f64) -> Self {
        self.rr_ratio = rr;
        self.min_net_rr = min_net_rr;
        self
    }

    /// Детектор Williams Fractal.
    /// Свеча i — fractal high если highs[i] > всех FRACTAL_LOOKBACK свечей слева и справа.
    /// Подтверждение — нужно FRACTAL_LOOKBACK свечей ПОСЛЕ кандидата.
    fn detect_fractals(&mut self) {
        let n = self.highs.len();
        if n < FRACTAL_LOOKBACK * 2 + 1 {
            return;
        }

        // Проверяем свечу (n - 1 - FRACTAL_LOOKBACK) — у неё уже есть FRACTAL_LOOKBACK свечей после
        let candidate_idx = n - 1 - FRACTAL_LOOKBACK;
        if candidate_idx < FRACTAL_LOOKBACK {
            return;
        }

        let high_candidate = self.highs[candidate_idx];
        let low_candidate = self.lows[candidate_idx];
        let ts = self.timestamps[candidate_idx];

        // Fractal HIGH
        let is_fractal_high = (candidate_idx - FRACTAL_LOOKBACK..candidate_idx + FRACTAL_LOOKBACK + 1)
            .filter(|&i| i != candidate_idx)
            .all(|i| self.highs[candidate_idx] > self.highs[i]);

        // Fractal LOW
        let is_fractal_low = (candidate_idx - FRACTAL_LOOKBACK..candidate_idx + FRACTAL_LOOKBACK + 1)
            .filter(|&i| i != candidate_idx)
            .all(|i| self.lows[candidate_idx] < self.lows[i]);

        if is_fractal_high {
            self.recent_fractal_highs.push_back((ts, high_candidate));
            if self.recent_fractal_highs.len() > 50 {
                self.recent_fractal_highs.pop_front();
            }
            self.n_fractals += 1;
            self.update_pools(high_candidate, true, ts);
        }
        if is_fractal_low {
            self.recent_fractal_lows.push_back((ts, low_candidate));
            if self.recent_fractal_lows.len() > 50 {
                self.recent_fractal_lows.pop_front();
            }
            self.n_fractals += 1;
            self.update_pools(low_candidate, false, ts);
        }
    }

    /// Кластеризация fractals в liquidity pools.
    /// Близкие fractals (в пределах POOL_CLUSTER_THRESHOLD_PCT) — один pool.
    fn update_pools(&mut self, price: f64, is_high: bool, ts: i64) {
        let threshold = price * POOL_CLUSTER_THRESHOLD_PCT;
        let mut found = false;

        for pool in &mut self.pools {
            if pool.is_high == is_high && !pool.swept {
                if (pool.price - price).abs() < threshold {
                    // Усредняем цену pool с учётом touch_count
                    let total_price = pool.price * pool.touch_count as f64 + price;
                    let new_count = pool.touch_count + 1;
                    pool.price = total_price / new_count as f64;
                    pool.touch_count = new_count;
                    pool.last_touch_ts = ts;
                    found = true;
                    break;
                }
            }
        }

        if !found {
            self.pools.push(LiquidityPool {
                price,
                is_high,
                touch_count: 1,
                last_touch_ts: ts,
                swept: false,
                sweep_extreme: None,
                sweep_ts: None,
            });
            self.n_pools += 1;
        }

        // Удаляем старые pools (более 100 свечей назад)
        let cutoff_ts = self.timestamps.back().copied().unwrap_or(0) - 100 * 3_600_000;
        self.pools.retain(|p| p.last_touch_ts > cutoff_ts);

        // Cap pools count
        if self.pools.len() > 30 {
            self.pools.sort_by_key(|p| p.last_touch_ts);
            self.pools.drain(0..self.pools.len() - 30);
        }
    }

    /// Проверка sweep — цена пробила pool, но закрылась обратно.
    /// Возвращает true если случился sweep (поля в pool обновляются).
    fn check_sweep(&mut self, kline_high: f64, kline_low: f64, close: f64, ts: i64) -> Option<SweepDirection> {
        for pool in &mut self.pools {
            if pool.swept {
                continue;
            }
            if pool.touch_count < MIN_POOL_TOUCHES {
                continue;
            }

            if pool.is_high {
                // Resistance sweep: high пробил pool price, close вернулся ниже
                if kline_high > pool.price && close < pool.price {
                    pool.swept = true;
                    pool.sweep_extreme = Some(kline_high);
                    pool.sweep_ts = Some(ts);
                    self.n_sweeps += 1;
                    return Some(SweepDirection::Bearish);
                }
            } else {
                // Support sweep: low пробил pool price, close вернулся выше
                if kline_low < pool.price && close > pool.price {
                    pool.swept = true;
                    pool.sweep_extreme = Some(kline_low);
                    pool.sweep_ts = Some(ts);
                    self.n_sweeps += 1;
                    return Some(SweepDirection::Bullish);
                }
            }
        }
        None
    }

    /// Проверка MSS — close пробивает последний противоположный fractal.
    /// Bullish MSS: close > последний fractal_high (после bullish sweep)
    /// Bearish MSS: close < последний fractal_low (после bearish sweep)
    fn check_mss(&self, close: f64, direction: SweepDirection) -> bool {
        match direction {
            SweepDirection::Bullish => {
                // Нужен fractal_high ПОСЛЕ sweep moment (новая структура)
                if let Some(&(_, fh)) = self.recent_fractal_highs.back() {
                    if close > fh {
                        return true;
                    }
                }
                false
            }
            SweepDirection::Bearish => {
                if let Some(&(_, fl)) = self.recent_fractal_lows.back() {
                    if close < fl {
                        return true;
                    }
                }
                false
            }
        }
    }

    /// Поиск TP — следующий противоположный liquidity pool.
    fn find_target_pool(&self, direction: SweepDirection, entry: f64) -> Option<f64> {
        match direction {
            SweepDirection::Bullish => {
                // Long: ищем следующий resistance (unswept high pool выше entry)
                self.pools
                    .iter()
                    .filter(|p| p.is_high && !p.swept && p.price > entry)
                    .map(|p| p.price)
                    .next()
            }
            SweepDirection::Bearish => {
                // Short: ищем следующий support (unswept low pool ниже entry)
                self.pools
                    .iter()
                    .filter(|p| !p.is_high && !p.swept && p.price < entry)
                    .map(|p| p.price)
                    .next()
            }
        }
    }

    /// Расчёт net RR после fees.
    fn net_rr(&self, entry: f64, sl: f64, tp: f64, is_long: bool) -> Option<f64> {
        let fee_cost = 2.0 * self.fee_rate * entry;
        // ✅ P23: полная стоимость спреда за круг (вход по ask, выход по bid).
        // До первого bookTicker спред неизвестен — модель как раньше.
        let spread_cost = if self.last_bid > 0.0 && self.last_ask > self.last_bid {
            self.last_ask - self.last_bid
        } else {
            0.0
        };
        let (gross_profit, gross_loss) = if is_long {
            (tp - entry - spread_cost, entry - sl + spread_cost)
        } else {
            (entry - tp - spread_cost, sl - entry + spread_cost)
        };
        let net_profit = gross_profit - fee_cost;
        let net_loss = gross_loss + fee_cost;
        if net_loss <= 0.0 || net_profit <= 0.0 {
            return None;
        }
        Some(net_profit / net_loss)
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
}

impl Strategy for LiquidityGrabStrategy {
    fn name(&self) -> &str {
        "liquidity_grab"
    }

    // ✅ V3.4: backfill греет структуру (fractals + liquidity pools),
    // чтобы live-свечи сразу работали по готовым уровням.
    fn wants_backfill(&self) -> bool { true }

    fn process(&mut self, msg: &WebSocketMessage) -> Result<Option<Signal>> {
        // ✅ P23: ловим живой спред из bookTicker (guard входа + стоимость спреда в net_rr)
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
        let kline = match msg {
            WebSocketMessage::Kline(k) => k,
            _ => return Ok(None),
        };
        if !kline.symbol.eq_ignore_ascii_case(&self.symbol) {
            return Ok(None);
        }
        if !kline.is_closed {
            return Ok(None);
        }

        self.last_kline_ts = kline.timestamp;
        self.n_candles += 1;

        // 1. Накапливаем историю
        self.highs.push_back(kline.high);
        self.lows.push_back(kline.low);
        self.closes.push_back(kline.close);
        self.timestamps.push_back(kline.timestamp);
        while self.highs.len() > self.max_history {
            self.highs.pop_front();
            self.lows.pop_front();
            self.closes.pop_front();
            self.timestamps.pop_front();
        }

        // 2. Detect fractals
        self.detect_fractals();

        // ✅ V3.4: backfill — только прогрев структуры. Sweep/MSS/retest
        // и СИГНАЛЫ по историческим свечам запрещены: иначе бот мог бы
        // открыть позицию по цене недельной давности. Live-свечи начинают
        // ловить sweep'ы сразу на прогретых pools.
        if kline.is_backfill { return Ok(None); }

        // 3. Check for sweep
        if let Some(direction) = self.check_sweep(kline.high, kline.low, kline.close, kline.timestamp) {
            // Создаём active sweep
            let sweep_extreme = if direction == SweepDirection::Bullish {
                kline.low // support sweep, extreme = low
            } else {
                kline.high // resistance sweep, extreme = high
            };

            self.active_sweep = Some(ActiveSweep {
                pool_price: 0.0, // установим ниже
                sweep_extreme,
                direction,
                sweep_ts: kline.timestamp,
                bars_since_sweep: 0,
                mss_confirmed: false,
            });

            // Update pool_price в active_sweep
            if let Some(sweep) = &mut self.active_sweep {
                // Находим pool с которого случился sweep
                for pool in &self.pools {
                    if pool.swept && pool.sweep_ts == Some(kline.timestamp) {
                        sweep.pool_price = pool.price;
                        break;
                    }
                }
            }
        }

        // 4. Если есть active_sweep — проверяем MSS
        // Используем .take() чтобы избежать borrow checker конфликта
        // (нельзя одновременно держать &mut self.active_sweep и звать self.check_mss)
        if let Some(mut sweep) = self.active_sweep.take() {
            sweep.bars_since_sweep += 1;

            // Timeout — retest не случился
            if sweep.bars_since_sweep > RETEST_TIMEOUT_BARS {
                self.n_blocked_retest_timeout += 1;
                // active_sweep уже None (через take) — sweep не возвращаем
                return Ok(None);
            }

            // Проверка MSS
            if !sweep.mss_confirmed {
                if self.check_mss(kline.close, sweep.direction) {
                    sweep.mss_confirmed = true;
                    self.n_mss_confirmed += 1;
                } else {
                    self.n_blocked_no_mss += 1;
                    self.active_sweep = Some(sweep); // сохраняем для следующей свечи
                    return Ok(None);
                }
            }

            // MSS confirmed — проверяем retest (close вернулся к pool_price)
            let retest_distance_pct =
                (kline.close - sweep.pool_price).abs() / sweep.pool_price * 100.0;
            if retest_distance_pct > 0.5 {
                // Цена ещё не вернулась к pool — ждём
                self.active_sweep = Some(sweep); // сохраняем для следующей свечи
                return Ok(None);
            }

            // Проверка позиции (не открываем если уже в позиции)
            if !self.filter.can_signal(kline.close, kline.timestamp) {
                self.n_blocked_position += 1;
                self.active_sweep = Some(sweep); // сохраняем для следующей свечи
                return Ok(None);
            }

            // Готовим signal
            let direction = sweep.direction;
            let entry_price = sweep.pool_price;
            let sl_price = sweep.sweep_extreme;

            // TP — следующий opposite pool, fallback на RR-based
            let tp_price = self
                .find_target_pool(direction, entry_price)
                .unwrap_or_else(|| {
                    let sl_dist = (entry_price - sl_price).abs();
                    match direction {
                        SweepDirection::Bullish => entry_price + sl_dist * self.rr_ratio,
                        SweepDirection::Bearish => entry_price - sl_dist * self.rr_ratio,
                    }
                });

            // Для long: TP должна быть выше entry; для short — ниже
            let (side, sl, tp) = match direction {
                SweepDirection::Bullish => {
                    let tp = tp_price.max(entry_price * 1.005); // min 0.5% TP
                    ("BUY", sl_price, tp)
                }
                SweepDirection::Bearish => {
                    let tp = tp_price.min(entry_price * 0.995);
                    ("SELL", sl_price, tp)
                }
            };

            let entry = round_to_tick(entry_price, TICK_SIZE);
            let sl = round_to_tick(sl, TICK_SIZE);
            let tp = round_to_tick(tp, TICK_SIZE);

            // Проверка Net RR
            let is_long = direction == SweepDirection::Bullish;
            // ✅ P23: slippage-guard — спред шире лимита = вход запрещён
            if self.spread_too_wide() {
                return Ok(None);
            }
            let eff_rr = match self.net_rr(entry, sl, tp, is_long) {
                Some(rr) => rr,
                None => {
                    // active_sweep уже None (через take)
                    return Ok(None);
                }
            };
            if eff_rr < self.min_net_rr {
                // active_sweep уже None (через take)
                return Ok(None);
            }

            // Position sizing с учётом fees
            let raw_qty = calc_position_size_with_fees(
                self.balance,
                entry,
                sl,
                self.risk_per_trade_pct,
                self.fee_rate * 2.0,
            );
            let quantity = floor_to_step(raw_qty, QTY_STEP);
            if quantity <= 0.0 {
                // active_sweep уже None (через take)
                return Ok(None);
            }

            // Регистрируем signal в filter
            self.filter.on_signal(entry, kline.timestamp);
            self.n_signals += 1;
            // active_sweep уже None (через take) — sweep consumed
            return Ok(Some(Signal {
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
            }));
        } else {
            self.n_blocked_no_sweep += 1;
        }

        Ok(None)
    }

    fn diagnostics(&self) -> Option<String> {
        Some(format!(
            "LiquidityGrab(candles) diag | candles={} fractals={} pools={} sweeps={} mss={} signals={} | no_sweep={} no_mss={} low_touches={} retest_timeout={} spr_block={} position={} | rr_ratio={} min_net_rr={} fee={:.4}% | recent_highs={} recent_lows={}",
            self.n_candles,
            self.n_fractals,
            self.n_pools,
            self.n_sweeps,
            self.n_mss_confirmed,
            self.n_signals,
            self.n_blocked_no_sweep,
            self.n_blocked_no_mss,
            self.n_blocked_low_touches,
            self.n_blocked_retest_timeout,
            self.n_spread_block,
            self.n_blocked_position,
            self.rr_ratio,
            self.min_net_rr,
            self.fee_rate * 100.0,
            self.recent_fractal_highs.len(),
            self.recent_fractal_lows.len(),
        ))
    }

    fn in_position(&self) -> bool {
        // ✅ P21: Cooldown больше НЕ считается «в позиции».
        // Фильтр уходит в Cooldown в момент ВЫДАЧИ сигнала (on_signal) — это
        // анти-флап. Раньше in_position()=true на свежевыданном сигнале
        // мгновенно само-блокировал его в main.rs («позиция уже открыта»),
        // и стратегия не могла войти никогда. Anti-flap остаётся в can_signal().
        matches!(self.filter.state(), crate::strategies::PositionState::InPosition)
    }

    fn notify_position_open(&mut self) {
        self.filter.on_position_opened();
    }

    fn notify_position_closed(&mut self, _pnl: f64) {
        self.filter.on_position_closed(self.last_kline_ts);
    }
}
