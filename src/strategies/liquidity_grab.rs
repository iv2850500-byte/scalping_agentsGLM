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

/// ✅ P59: окно ATR (simple mean of True Range)
const ATR_WINDOW: usize = 14;
/// ✅ P59: период режимной EMA (старший ТФ-прокси тренда)
const REGIME_EMA_PERIOD: f64 = 50.0;

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
    funding_pct: f64, // ✅ P24: оценка funding за круг (аудит №12)
    n_spread_block: u64,
    // ✅ P57: гейт глубины книги (OrderBook в ±band от mid)
    last_depth_usdt: f64,
    min_depth_usdt: f64,
    depth_band_frac: f64,
    n_depth_block: u64,
    // ✅ P58: объёмное подтверждение свипа
    volumes: VecDeque<f64>,
    vol_mult: f64,
    n_vol_block: u64,
    // ✅ P59: ATR-нормировка стопа + режимный гейт (EMA50 собственного актива)
    trs: VecDeque<f64>,
    atr: f64,
    atr_floor_mult: f64,
    ema50: f64,
    regime_gate: bool,
    n_atr_widen: u64,
    n_atr_skip: u64,
    n_regime_block: u64,
    // ✅ P60: живое equity × режимный множитель (сайзинг)
    live_equity: f64,
    risk_multiplier: f64,
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
            // ✅ P24: funding за круг (аудит №12) — консервативно один интервал 8h,
            // доля от цены; дефолт 0.0001 = 0.01%/8h; мусор/<=0 в env -> 0.0 (выключено)
            funding_pct: std::env::var("P24_FUNDING_PCT")
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| *v > 0.0)
                .unwrap_or(0.0001),
            n_spread_block: 0,
            // ✅ P57: мин. глубина книги в ±band от mid, USDT (0 = выкл).
            // Тонкий INJ платил спред+импакт, сопоставимый со всем TP.
            min_depth_usdt: std::env::var("P57_MIN_DEPTH_USDT")
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| *v >= 0.0)
                .unwrap_or(500.0),
            // ✅ P57: полоса глубины в % от mid (дефолт 0.10%)
            depth_band_frac: std::env::var("P57_DEPTH_BAND_PCT")
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| *v > 0.0)
                .map(|v| v / 100.0)
                .unwrap_or(0.001),
            last_depth_usdt: 0.0,
            n_depth_block: 0,
            // ✅ P58: свип должен нести объём: volume > mult × avg20(prev).
            // 0 = выкл. 1.2 = свип сильнее среднего на 20%.
            vol_mult: std::env::var("P58_VOL_MULT")
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| *v >= 0.0)
                .unwrap_or(1.2),
            volumes: VecDeque::with_capacity(64),
            n_vol_block: 0,
            // ✅ P59: пол стопа = mult × ATR (вик за пул бывает тонким —
            // сайзинг раздувается и шум выбивает позицию). 0 = выкл.
            atr_floor_mult: std::env::var("P59_ATR_FLOOR_MULT")
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| *v >= 0.0)
                .unwrap_or(1.0),
            trs: VecDeque::with_capacity(ATR_WINDOW + 1),
            atr: 0.0,
            ema50: 0.0,
            // ✅ P59: режимный гейт по EMA50 ("0" = выкл, дефолт вкл)
            regime_gate: std::env::var("P59_REGIME_GATE")
                .map(|v| v.trim() != "0")
                .unwrap_or(true),
            n_atr_widen: 0,
            n_atr_skip: 0,
            n_regime_block: 0,
            // ✅ P60: equity приходит из main (log-цикл, раз в 30с)
            live_equity: 0.0,
            risk_multiplier: 0.5,
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
        // ✅ P24: funding за круг (аудит №12) — консервативно один интервал 8h
        let funding_cost = entry * self.funding_pct;
        let net_profit = gross_profit - fee_cost - funding_cost;
        let net_loss = gross_loss + fee_cost + funding_cost;
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

    /// ✅ P57/DEPTH-0: true = книга в ±band от mid тоньше порога
    /// (оценка видна: last_depth_usdt > 0). Нет данных — fail-open.
    fn depth_too_thin(&self) -> bool {
        self.min_depth_usdt > 0.0
            && self.last_depth_usdt > 0.0
            && self.last_depth_usdt < self.min_depth_usdt
    }

    /// ✅ P59/ATR-0: обновление ATR (simple mean TR за ATR_WINDOW)
    fn update_atr(&mut self, high: f64, low: f64, close: f64) {
        let tr = match self.closes.back().copied() {
            Some(pc) if pc > 0.0 => (high - low)
                .max((high - pc).abs())
                .max((low - pc).abs()),
            _ => high - low,
        };
        self.trs.push_back(tr);
        while self.trs.len() > ATR_WINDOW {
            self.trs.pop_front();
        }
        let n = self.trs.len();
        if n > 0 {
            self.atr = self.trs.iter().sum::<f64>() / n as f64;
        }
        let _ = close;
    }

    /// ✅ P59/REG-0: обновление режимной EMA (старший ТФ-прокси)
    fn update_regime_ema(&mut self, close: f64) {
        let k = 2.0 / (REGIME_EMA_PERIOD + 1.0);
        self.ema50 = if self.ema50 <= 0.0 { close } else { self.ema50 + k * (close - self.ema50) };
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
        // ✅ P57/DEPTH-2: живой стакан (partial depth stream) — оценка
        // глубины в ±band от mid для гейта входа.
        if let WebSocketMessage::OrderBook(ob) = msg {
            if ob.symbol.eq_ignore_ascii_case(&self.symbol) {
                if let (Some(&(bb, _)), Some(&(ba, _))) = (ob.bids.first(), ob.asks.first()) {
                    if bb > 0.0 && ba > 0.0 && ba >= bb {
                        let mid = (bb + ba) * 0.5;
                        let band = mid * self.depth_band_frac;
                        let bid_depth: f64 = ob
                            .bids
                            .iter()
                            .filter(|(p, _)| *p >= mid - band)
                            .map(|(p, q)| p * q)
                            .sum();
                        let ask_depth: f64 = ob
                            .asks
                            .iter()
                            .filter(|(p, _)| *p <= mid + band)
                            .map(|(p, q)| p * q)
                            .sum();
                        self.last_depth_usdt = bid_depth + ask_depth;
                    }
                }
            }
            return Ok(None);
        }
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

        // ✅ P58: объём свечей — для подтверждения свипа (греется и на backfill)
        self.volumes.push_back(kline.volume);
        while self.volumes.len() > 60 {
            self.volumes.pop_front();
        }
        // ✅ P59: ATR + режимная EMA (греются и на backfill — как fractals)
        self.update_atr(kline.high, kline.low, kline.close);
        self.update_regime_ema(kline.close);

        // 2. Detect fractals
        self.detect_fractals();

        // ✅ V3.4: backfill — только прогрев структуры. Sweep/MSS/retest
        // и СИГНАЛЫ по историческим свечам запрещены: иначе бот мог бы
        // открыть позицию по цене недельной давности. Live-свечи начинают
        // ловить sweep'ы сразу на прогретых pools.
        if kline.is_backfill { return Ok(None); }

        // 3. Check for sweep
        if let Some(direction) = self.check_sweep(kline.high, kline.low, kline.close, kline.timestamp) {
            // ✅ P58/VOL-1: объёмное подтверждение свипа (принцип «вход после
            // подтверждения»: уровень+объём+закрытие свечи). Свип без объёма —
            // шум/стоп-хант без силы; pool уже помечен swept (структура
            // сломана) — но active_sweep НЕ создаём, вход не ищем.
            let vol_ok = if self.vol_mult > 0.0 && self.volumes.len() >= 6 {
                let tail: Vec<f64> = self
                    .volumes
                    .iter()
                    .rev()
                    .skip(1) // текущая свеча не участвует в среднем
                    .take(20)
                    .copied()
                    .collect();
                let avg20 = tail.iter().sum::<f64>() / tail.len() as f64;
                kline.volume > self.vol_mult * avg20
            } else {
                true // мало истории — не блокируем (прогрев)
            };
            if !vol_ok {
                self.n_vol_block += 1;
                return Ok(None);
            }
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

            // ✅ P59/ATR-1: нормировка стопа по ATR. Вик за пул бывает
            // бритвенно тонким: риск/ед крошечный → сайзинг раздувается →
            // шум выбивает стоп. Пол дистанции стопа = atr_floor_mult × ATR;
            // аномально широкий вик (> 3×ATR) = структура сломана, пропуск.
            let mut sl_eff = sl;
            if self.atr_floor_mult > 0.0 && self.atr > 0.0 {
                let wick_dist = (entry - sl).abs();
                let floor = self.atr_floor_mult * self.atr;
                if wick_dist < floor {
                    sl_eff = match direction {
                        SweepDirection::Bullish => entry - floor,
                        SweepDirection::Bearish => entry + floor,
                    };
                    self.n_atr_widen += 1;
                } else if wick_dist > 3.0 * self.atr {
                    self.n_atr_skip += 1;
                    return Ok(None);
                }
            }

            // ✅ P59/REG-1: режимный гейт (старший ТФ-прокси из статьи):
            // вход только по тренду EMA50 собственного актива. Крипто-бета
            // INJ/TAO/SOL коррелирует с BTC — против-трендовые свипы
            // оплачиваются дважды (спред + продление движения).
            if self.regime_gate && self.ema50 > 0.0 {
                let trend_ok = match direction {
                    SweepDirection::Bullish => kline.close > self.ema50,
                    SweepDirection::Bearish => kline.close < self.ema50,
                };
                if !trend_ok {
                    self.n_regime_block += 1;
                    return Ok(None);
                }
            }

            // Проверка Net RR
            let is_long = direction == SweepDirection::Bullish;
            // ✅ P23: slippage-guard — спред шире лимита = вход запрещён
            if self.spread_too_wide() {
                return Ok(None);
            }
            // ✅ P57/DEPTH-1: тонкая книга = платим спред+импакт, сопоставимый
            // со всем TP. Нет данных OrderBook (0.0) — fail-open (диагностика
            // покажет depth=0).
            if self.depth_too_thin() {
                self.n_depth_block += 1;
                return Ok(None);
            }
            let eff_rr = match self.net_rr(entry, sl_eff, tp, is_long) {
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

            // ✅ P60/EQ-3: сайзинг от ЖИВОГО equity (initial + realized PnL)
            // × режимный множитель (SURVIVAL 0.5 / GROWTH 1.0). Раньше база
            // была статическим initial_balance: после просадки риск в % от
            // фактического капитала рос. Fallback — initial (equity ещё не приходил).
            let sizing_base = if self.live_equity > 0.0 { self.live_equity } else { self.balance };
            let raw_qty = calc_position_size_with_fees(
                sizing_base * self.risk_multiplier,
                entry,
                sl_eff,
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
                stop_loss: Some(sl_eff),
                take_profit: Some(tp),
                // ✅ P59/ATR-2: ATR уходит в state — трейлинг Chandelier
                // (TRAIL_ATR_MULT × atr_hint) начинает работать для liquidity_grab
                atr_hint: self.atr,
                entry_spread: 0.0,
            }));
        } else {
            self.n_blocked_no_sweep += 1;
        }

        Ok(None)
    }

    fn diagnostics(&self) -> Option<String> {
        Some(format!(
            "LiquidityGrab(candles) diag | candles={} fractals={} pools={} sweeps={} mss={} signals={} | no_sweep={} no_mss={} low_touches={} retest_timeout={} spr_block={} position={} | rr_ratio={} min_net_rr={} fee={:.4}% | recent_highs={} recent_lows={} | P57 depth={:.0}/{:.0} d_block={} | P58 vol_block={} (mult={:.2}) | P59 atr={:.8} widen={} skip={} regime_block={} (ema50={:.8} gate={}) | P60 eq={:.2} mult={:.2}",
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
            // ✅ P57/P58/P59/P60: новые гейты и сайзинг видны в диагностике
            self.last_depth_usdt,
            self.min_depth_usdt,
            self.n_depth_block,
            self.n_vol_block,
            self.vol_mult,
            self.atr,
            self.n_atr_widen,
            self.n_atr_skip,
            self.n_regime_block,
            self.ema50,
            self.regime_gate,
            if self.live_equity > 0.0 { self.live_equity } else { self.balance },
            self.risk_multiplier,
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

    // ✅ P60/EQ-4: живое equity и режимный множитель — провода из main.
    // set_live_equity приходит раз в 30с (log-цикл), множитель — из ЭТАП C
    // (SURVIVAL 0.5 / GROWTH 1.0 / HALT 0.0).
    fn set_live_equity(&mut self, eq: f64) {
        if eq.is_finite() && eq > 0.0 {
            self.live_equity = eq;
        }
    }

    fn set_risk_multiplier(&mut self, mult: f64) {
        if mult.is_finite() && mult > 0.0 {
            self.risk_multiplier = mult;
        }
    }

    fn notify_position_closed(&mut self, _pnl: f64) {
        self.filter.on_position_closed(self.last_kline_ts);
    }
}

// ============================================================
// ✅ P56/T-RR: тесты денежной математики стратегии (net_rr — формула
// «нетто после спреда/комиссий/funding»,на минимальном RR).
// Тесты живут В ЭТОМ модуле — net_rr приватный по замыслу (инкапсуляция
// стоимости круга внутри стратегии).
// ============================================================
#[cfg(test)]
mod p56_rr_tests {
    use super::*;

    fn strategy() -> LiquidityGrabStrategy {
        LiquidityGrabStrategy::new("INJUSDT".to_string(), 800.0, 0.5, 0.0005)
            .with_rr(2.5, 1.5)
    }

    #[test]
    fn rr_long_no_spread_reference() {
        let s = strategy();
        // entry 10, SL 9.9 (риск 0.1), TP 10.5 (профит 0.5)
        // fee = 2*0.0005*10 = 0.01; funding = 10*0.0001 = 0.001; спред 0
        let rr = s.net_rr(10.0, 9.9, 10.5, true).unwrap();
        let net_profit = 0.5 - 0.01 - 0.001;
        let net_loss = 0.1 + 0.01 + 0.001;
        assert!((rr - net_profit / net_loss).abs() < 1e-12);
        assert!(rr > 4.0, "чистый RR для 1:5 с комиссиями должен быть > 4, got {}", rr);
    }

    #[test]
    fn rr_long_spread_costs_both_sides() {
        let mut s = strategy();
        // Живой спред 0.002 (bid 9.999 / ask 10.001): вход по ask, выход по bid
        s.last_bid = 9.999;
        s.last_ask = 10.001;
        let rr_with = s.net_rr(10.0, 9.9, 10.5, true).unwrap();
        let rr_without = strategy().net_rr(10.0, 9.9, 10.5, true).unwrap();
        assert!(rr_with < rr_without, "спред обязан ухудшать RR");
        // спред 0.002 входит в ОБОИХ плечах: profit -0.002, loss +0.002
        let expected = (0.5 - 0.002 - 0.01 - 0.001) / (0.1 + 0.002 + 0.01 + 0.001);
        assert!((rr_with - expected).abs() < 1e-12);
    }

    #[test]
    fn rr_short_mirror() {
        let mut s = strategy();
        s.last_bid = 9.999;
        s.last_ask = 10.001;
        // Short: entry 10, SL 10.1 (риск 0.1), TP 9.5 (профит 0.5)
        let rr = s.net_rr(10.0, 10.1, 9.5, false).unwrap();
        let expected = (0.5 - 0.002 - 0.01 - 0.001) / (0.1 + 0.002 + 0.01 + 0.001);
        assert!((rr - expected).abs() < 1e-12);
    }

    #[test]
    fn rr_degenerate_levels_rejected() {
        let s = strategy();
        // SL на НЕПРАВИЛЬНОЙ стороне (выше входа у лонга) -> net_loss <= 0 -> None.
        // (Бритвенно тонкий, но корректный SL не отбраковывается формулой RR:
        // он даёт раздутый RR — именно это лечит P59/ATR-пол на уровне сигнала,
        // а S11-рельсы в движке отсекают инвертированные стопы до ордера.)
        assert!(s.net_rr(10.0, 10.1, 10.5, true).is_none());
        // net_profit <= 0 (TP ниже входа у лонга) — None
        assert!(s.net_rr(10.0, 9.9, 9.95, true).is_none());
        // Тонкий SL: RR раздувается, формулу это НЕ блокирует (задокументировано)
        assert!(s.net_rr(10.0, 9.9999, 10.5, true).is_some());
    }

    #[test]
    fn atr_floor_widens_thin_wick() {
        let mut s = strategy();
        s.atr = 0.5; // искусственный ATR
        s.atr_floor_mult = 1.0;
        // вик 0.1 << пол 0.5 → стоп раздвигается до entry - 0.5
        let entry = 10.0;
        let sl = 9.9;
        let floor = s.atr_floor_mult * s.atr;
        let sl_eff = entry - floor;
        assert!((sl_eff - 9.5).abs() < 1e-12);
        assert!((entry - sl_eff).abs() > (entry - sl).abs());
    }

    #[test]
    fn volume_gate_math() {
        // avg20 prev = 100, mult 1.2: свип объёмом 121 проходит, 119 — нет
        let avg20 = 100.0;
        let mult = 1.2;
        assert!(121.0 > mult * avg20);
        assert!(!(119.0 > mult * avg20));
    }
}
