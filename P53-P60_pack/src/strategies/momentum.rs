//! momentum.rs v3 FULL — momentum strategy со всеми 6 патчами Фазы A.
//!
//! ПАТЧИ (все 6):
//!   1. EMA200 trend filter — block LONG если close < EMA200, block SHORT если close > EMA200
//!   2. Volume spike rejection — block если volume > 3× avg(20) (импульс, не разворот)
//!   3. Loss-streak cooldown — 4ч пауза после 2 подряд лоссов
//!   4. Supertrend(10, 3.0) — 2-й trend filter (binary, на основе ATR)
//!   5. MFI(14) — фильтр боковика (block if MFI > 80 или < 20 в regime != Trending)
//!   6. ROC(12) — детектор дампов (block SHORT при ROC < -5%)
//!
//! state_manager.rs уже настроен: BE=1.5R, partial отключён, trail=2.0×ATR.
//! Это drop-in replacement для src/strategies/momentum.rs (полная v3 с 6 патчами).

use crate::strategies::market_regime::{MarketRegime, MarketRegimeDetector};
use crate::strategies::{PositionState, Signal, SignalFilter, Strategy};
use crate::utils::math::{calc_position_size_with_fees, floor_to_step, round_to_tick};
use crate::websocket::WebSocketMessage;
use anyhow::Result;
use std::collections::VecDeque;

const BTC_TICK_SIZE: f64 = 0.01;
const BTC_QTY_STEP: f64 = 0.00001;

const DEFAULT_FEE_RATE: f64 = 0.0002;
const DEFAULT_MIN_NET_RR: f64 = 1.2;
const DEFAULT_MIN_EV_PCT: f64 = 0.0;

/// ✅ ПРАВКА 3 (edge-запас): вход должен превышать минимально допустимый
/// R:R на 15%.
const EDGE_QUALITY_MULT: f64 = 1.15;

// 🆕 ПАТЧ 1: EMA200
const EMA200_PERIOD: usize = 200;
const EMA200_DEAD_ZONE_PCT: f64 = 0.001;

// 🆕 ПАТЧ 2: Volume spike rejection
const VOL_SPIKE_REJECTION_MULT: f64 = 3.0;
const VOL_SPIKE_WINDOW: usize = 20;

// 🆕 ПАТЧ 3: Loss-streak cooldown
const LOSS_STREAK_THRESHOLD: u32 = 2;
const LOSS_COOLDOWN_MS: i64 = 4 * 3600 * 1000;

// 🆕 ПАТЧ 4: Supertrend
const SUPERTREND_PERIOD: usize = 10;
const SUPERTREND_MULT: f64 = 3.0;

// 🆕 ПАТЧ 5: MFI
const MFI_PERIOD: usize = 14;
const MFI_OVERBOUGHT: f64 = 80.0;
const MFI_OVERSOLD: f64 = 20.0;

// 🆕 ПАТЧ 6: ROC
const ROC_PERIOD: usize = 12;
const ROC_DUMP_THRESHOLD: f64 = -0.05;

pub struct MomentumStrategy {
    symbol: String,
    balance: f64,
    risk_per_trade_pct: f64,

    closes: VecDeque<f64>,
    highs: VecDeque<f64>,
    lows: VecDeque<f64>,
    volumes: VecDeque<f64>,
    taker_buys: VecDeque<f64>,
    atrs: VecDeque<f64>,

    lookback: usize,
    atr_period: usize,
    ofi_window: usize,

    min_atr_threshold_pct: f64,
    ofi_threshold: f64,
    volume_anomaly_threshold: f64,

    tp_atr_mult: f64,
    min_tp_pct: f64,
    rr_ratio: f64,

    filter: SignalFilter,
    regime_detector: MarketRegimeDetector,

    trading_start_utc: u32,
    trading_end_utc: u32,

    max_trades_per_day: u32,
    trades_today: u32,
    current_day: i64,
    last_trade_ts_ms: Option<i64>,
    min_time_between_trades_ms: i64,
    last_kline_ts: i64,

    fee_rate: f64,
    min_net_rr: f64,
    min_ev_pct: f64,

    trades_won: u32,
    trades_lost: u32,
    total_win_amount: f64,
    total_loss_amount: f64,

    blocked_by_atr: u64,
    blocked_by_momentum: u64,
    blocked_by_ofi: u64,
    blocked_by_volume: u64,
    blocked_by_regime: u64,
    blocked_by_time: u64,
    blocked_by_limit_day: u64,
    blocked_by_cooldown: u64,
    blocked_by_net_rr: u64,
    blocked_by_edge: u64,
    passed_filters: u64,
    total_closed_candles: u64,

    // 🆕 Счётчики для новых фильтров
    blocked_by_ema200: u64,
    blocked_by_vol_spike: u64,
    blocked_by_loss_streak: u64,
    blocked_by_supertrend: u64,
    blocked_by_mfi: u64,
    blocked_by_roc: u64,

    // 🆕 EMA200 state
    ema200_value: Option<f64>,
    ema200_alpha: f64,

    // 🆕 Loss streak
    consecutive_losses: u32,
    last_loss_ts: Option<i64>,

    // 🆕 Supertrend state — ВАЖНО: храним prev_final_upper и prev_final_lower ОТДЕЛЬНО
    prev_final_upper: Option<f64>,
    prev_final_lower: Option<f64>,
    supertrend_direction: i8, // +1 = UP (long-favorable), -1 = DOWN (short-favorable)
    supertrend_value: Option<f64>,

    // ✅ P23: slippage-модель по живому bookTicker (аудит: «спред не моделируется»)
    last_bid: f64,
    last_ask: f64,
    max_spread_pct: f64,
    funding_pct: f64, // ✅ P24: оценка funding за круг (аудит №12)
    n_spread_block: u64,
    // ✅ P60/EQ-5: режимный множитель риска (SURVIVAL 0.5 / GROWTH 1.0)
    risk_multiplier: f64,
}

impl MomentumStrategy {
    pub fn new(symbol: String, balance: f64, risk_pct: f64) -> Self {
        Self {
            symbol,
            balance,
            risk_per_trade_pct: risk_pct,

            closes: VecDeque::with_capacity(500),
            highs: VecDeque::with_capacity(500),
            lows: VecDeque::with_capacity(500),
            volumes: VecDeque::with_capacity(500),
            taker_buys: VecDeque::with_capacity(500),
            atrs: VecDeque::with_capacity(500),

            lookback: 20,
            atr_period: 14,
            ofi_window: 5,

            min_atr_threshold_pct: 0.05,
            ofi_threshold: 0.15,
            volume_anomaly_threshold: 1.5,

            tp_atr_mult: 2.5,
            min_tp_pct: 0.0025,
            rr_ratio: 2.5,

            filter: SignalFilter::new(60, 0.15),
            regime_detector: MarketRegimeDetector::new(100, 0.5, 0.1),

            trading_start_utc: 7,
            trading_end_utc: 22,

            max_trades_per_day: 10,
            trades_today: 0,
            current_day: -1,
            last_trade_ts_ms: None,
            min_time_between_trades_ms: 300_000,
            last_kline_ts: 0,

            fee_rate: DEFAULT_FEE_RATE,
            min_net_rr: DEFAULT_MIN_NET_RR,
            min_ev_pct: DEFAULT_MIN_EV_PCT,

            trades_won: 0,
            trades_lost: 0,
            total_win_amount: 0.0,
            total_loss_amount: 0.0,

            blocked_by_atr: 0,
            blocked_by_momentum: 0,
            blocked_by_ofi: 0,
            blocked_by_volume: 0,
            blocked_by_regime: 0,
            blocked_by_time: 0,
            blocked_by_limit_day: 0,
            blocked_by_cooldown: 0,
            blocked_by_net_rr: 0,
            blocked_by_edge: 0,
            passed_filters: 0,
            total_closed_candles: 0,

            blocked_by_ema200: 0,
            blocked_by_vol_spike: 0,
            blocked_by_loss_streak: 0,
            blocked_by_supertrend: 0,
            blocked_by_mfi: 0,
            blocked_by_roc: 0,

            ema200_value: None,
            ema200_alpha: 2.0 / (EMA200_PERIOD as f64 + 1.0),

            consecutive_losses: 0,
            last_loss_ts: None,

            prev_final_upper: None,
            prev_final_lower: None,
            supertrend_direction: 1,
            supertrend_value: None,

            last_bid: 0.0, last_ask: 0.0,
            // ✅ P23: лимит спреда (доля от мид-цены); <=0 или мусор = guard выключен
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
            // ✅ P60/EQ-5: старт в SURVIVAL (как RiskManager, ЭТАП C)
            risk_multiplier: 0.5,
        }
    }

    pub fn with_windows(mut self, lookback: usize, atr_period: usize, ofi_window: usize) -> Self {
        self.lookback = lookback.max(2);
        self.atr_period = atr_period.max(2);
        self.ofi_window = ofi_window.max(1);
        self
    }

    pub fn with_fee(mut self, fee_rate: f64, min_net_rr: f64, min_ev_pct: f64) -> Self {
        self.fee_rate = fee_rate;
        self.min_net_rr = min_net_rr;
        self.min_ev_pct = min_ev_pct;
        self
    }

    pub fn with_tp_sl(mut self, tp_atr_mult: f64, min_tp_pct: f64, rr_ratio: f64) -> Self {
        self.tp_atr_mult = tp_atr_mult.max(0.5);
        self.min_tp_pct = min_tp_pct.max(0.0005);
        self.rr_ratio = rr_ratio.max(1.0);
        self
    }

    pub fn with_thresholds(
        mut self,
        min_atr_threshold_pct: f64,
        ofi_threshold: f64,
        volume_anomaly_threshold: f64,
    ) -> Self {
        self.min_atr_threshold_pct = min_atr_threshold_pct.max(0.0);
        self.ofi_threshold = ofi_threshold.clamp(0.02, 0.95);
        self.volume_anomaly_threshold = volume_anomaly_threshold.max(1.0);
        self
    }

    pub fn with_limits(
        mut self,
        max_trades_per_day: u32,
        min_time_between_trades_ms: i64,
        trading_start_utc: u32,
        trading_end_utc: u32,
    ) -> Self {
        self.max_trades_per_day = max_trades_per_day.max(1);
        self.min_time_between_trades_ms = min_time_between_trades_ms.max(0);
        self.trading_start_utc = trading_start_utc;
        self.trading_end_utc = trading_end_utc;
        self
    }

    pub fn with_regime(
        mut self,
        window: usize,
        volatile_pct: f64,
        trending_pct: f64,
    ) -> Self {
        self.regime_detector = MarketRegimeDetector::new(window, volatile_pct, trending_pct);
        self
    }

    fn min_tp_pct(&self) -> f64 {
        let fee_rt = 2.0 * self.fee_rate;
        let denom = 1.0 - (1.0 / self.rr_ratio) * self.min_net_rr;
        if denom <= 0.0 {
            return f64::INFINITY;
        }
        (fee_rt * (1.0 + self.min_net_rr) / denom).max(0.001)
    }

    fn update_atr(&mut self) {
        let len = self.closes.len();
        if len < 2 {
            return;
        }
        let high = self.highs[len - 1];
        let low = self.lows[len - 1];
        let prev_close = self.closes[len - 2];

        let tr = (high - low)
            .max((high - prev_close).abs())
            .max((low - prev_close).abs());

        let atr = if let Some(&prev_atr) = self.atrs.back() {
            let n = self.atr_period as f64;
            (prev_atr * (n - 1.0) + tr) / n
        } else {
            tr
        };
        self.atrs.push_back(atr);
        let max_hist = self.atr_period * 3 + 10;
        while self.atrs.len() > max_hist {
            self.atrs.pop_front();
        }
    }

    fn current_atr(&self) -> f64 {
        self.atrs.back().copied().unwrap_or(0.0)
    }

    fn calculate_momentum(&self) -> f64 {
        if self.closes.len() < self.lookback {
            return 0.0;
        }
        let now = *self.closes.back().unwrap();
        let prev = self.closes[self.closes.len() - self.lookback];
        if prev <= 0.0 {
            0.0
        } else {
            (now - prev) / prev
        }
    }

    fn calculate_ofi(&self) -> f64 {
        let n = self.ofi_window.min(self.taker_buys.len());
        if n == 0 {
            return 0.0;
        }
        let start = self.taker_buys.len() - n;
        let mut buy = 0.0;
        let mut sell = 0.0;
        for i in start..self.taker_buys.len() {
            buy += self.taker_buys[i];
            sell += (self.volumes[i] - self.taker_buys[i]).max(0.0);
        }
        let total = buy + sell;
        if total <= 0.0 {
            0.0
        } else {
            (buy - sell) / total
        }
    }

    fn volume_anomaly(&self) -> f64 {
        let len = self.volumes.len();
        if len < 10 {
            return 1.0;
        }
        let window = 100.min(len - 1);
        let start = len - 1 - window;
        let mut sum = 0.0;
        for i in start..(len - 1) {
            sum += self.volumes[i];
        }
        let avg = sum / window as f64;
        if avg <= 0.0 {
            1.0
        } else {
            self.volumes[len - 1] / avg
        }
    }

    // 🆕 ПАТЧ 2: Volume spike rejection
    fn is_volume_spike(&self) -> bool {
        let len = self.volumes.len();
        if len < VOL_SPIKE_WINDOW + 1 {
            return false;
        }
        let current = self.volumes[len - 1];
        let mut sum = 0.0;
        for i in (len - 1 - VOL_SPIKE_WINDOW)..(len - 1) {
            sum += self.volumes[i];
        }
        let avg = sum / VOL_SPIKE_WINDOW as f64;
        if avg <= 0.0 {
            return false;
        }
        current >= avg * VOL_SPIKE_REJECTION_MULT
    }

    // 🆕 ПАТЧ 1: EMA200
    fn update_ema200(&mut self, close: f64) {
        if let Some(prev) = self.ema200_value {
            let new_ema = close * self.ema200_alpha + prev * (1.0 - self.ema200_alpha);
            self.ema200_value = Some(new_ema);
        } else if self.closes.len() >= EMA200_PERIOD {
            let sum: f64 = self.closes.iter().take(EMA200_PERIOD).sum();
            self.ema200_value = Some(sum / EMA200_PERIOD as f64);
        }
    }

    fn is_above_ema200(&self, close: f64) -> bool {
        match self.ema200_value {
            Some(ema) => close > ema * (1.0 + EMA200_DEAD_ZONE_PCT),
            None => false,
        }
    }

    fn is_below_ema200(&self, close: f64) -> bool {
        match self.ema200_value {
            Some(ema) => close < ema * (1.0 - EMA200_DEAD_ZONE_PCT),
            None => false,
        }
    }

    // 🆕 ПАТЧ 4: Supertrend — ПРАВИЛЬНАЯ реализация с carry-forward
    // Алгоритм:
    //   basic_upper = HL2 + mult × ATR(N)
    //   basic_lower = HL2 - mult × ATR(N)
    //   final_upper = if basic_upper < prev_final_upper OR prev_close > prev_final_upper
    //                 then basic_upper else prev_final_upper
    //   final_lower = if basic_lower > prev_final_lower OR prev_close < prev_final_lower
    //                 then basic_lower else prev_final_lower
    //   direction flips:
    //     UP→DOWN если close < final_lower (supertrend = final_upper)
    //     DOWN→UP если close > final_upper (supertrend = final_lower)
    fn update_supertrend(&mut self) {
        let len = self.closes.len();
        if len < SUPERTREND_PERIOD + 1 {
            return;
        }

        let high = self.highs[len - 1];
        let low = self.lows[len - 1];
        let hl2 = (high + low) / 2.0;
        let close = self.closes[len - 1];

        let atr_window = SUPERTREND_PERIOD.min(self.atrs.len());
        if atr_window == 0 {
            return;
        }
        let atr_sum: f64 = self.atrs.iter().rev().take(atr_window).sum();
        let atr = atr_sum / atr_window as f64;

        let basic_upper = hl2 + SUPERTREND_MULT * atr;
        let basic_lower = hl2 - SUPERTREND_MULT * atr;

        let prev_close = self.closes[len - 2];

        let final_upper = match self.prev_final_upper {
            Some(prev_up) => {
                if basic_upper < prev_up || prev_close > prev_up {
                    basic_upper
                } else {
                    prev_up
                }
            }
            None => basic_upper,
        };

        let final_lower = match self.prev_final_lower {
            Some(prev_lo) => {
                if basic_lower > prev_lo || prev_close < prev_lo {
                    basic_lower
                } else {
                    prev_lo
                }
            }
            None => basic_lower,
        };

        let new_dir = if self.supertrend_direction == 1 {
            if close < final_lower { -1 } else { 1 }
        } else {
            if close > final_upper { 1 } else { -1 }
        };

        self.prev_final_upper = Some(final_upper);
        self.prev_final_lower = Some(final_lower);
        self.supertrend_direction = new_dir;
        self.supertrend_value = Some(if new_dir == 1 { final_lower } else { final_upper });
    }

    fn supertrend_is_up(&self) -> bool {
        self.supertrend_direction == 1
    }

    // 🆕 ПАТЧ 5: MFI
    fn calculate_mfi(&self) -> Option<f64> {
        let len = self.closes.len();
        if len < MFI_PERIOD + 1 {
            return None;
        }
        let mut pos_flow = 0.0;
        let mut neg_flow = 0.0;
        let start = len - MFI_PERIOD - 1;
        for i in start..(len - 1) {
            let typical_price = (self.highs[i + 1] + self.lows[i + 1] + self.closes[i + 1]) / 3.0;
            let prev_typical = (self.highs[i] + self.lows[i] + self.closes[i]) / 3.0;
            let money_flow = typical_price * self.volumes[i + 1];
            if typical_price > prev_typical {
                pos_flow += money_flow;
            } else if typical_price < prev_typical {
                neg_flow += money_flow;
            }
        }
        if neg_flow <= 0.0 {
            return Some(100.0);
        }
        let ratio = pos_flow / neg_flow;
        Some(100.0 - 100.0 / (1.0 + ratio))
    }

    // 🆕 ПАТЧ 6: ROC
    fn calculate_roc(&self) -> Option<f64> {
        let len = self.closes.len();
        if len < ROC_PERIOD + 1 {
            return None;
        }
        let current = *self.closes.back().unwrap();
        let past = self.closes[len - 1 - ROC_PERIOD];
        if past <= 0.0 {
            return None;
        }
        Some((current - past) / past)
    }

    fn is_trading_hour(&self, ts_ms: i64) -> bool {
        let secs_of_day = ((ts_ms / 1000) % 86_400) as u32;
        let hour = secs_of_day / 3600;
        if self.trading_start_utc <= self.trading_end_utc {
            hour >= self.trading_start_utc && hour < self.trading_end_utc
        } else {
            hour >= self.trading_start_utc || hour < self.trading_end_utc
        }
    }

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

    // ✅ P23: slippage-guard — true = спред шире лимита, вход запрещён
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

    fn expected_value(&self, wr: f64, avg_win: f64, avg_loss: f64) -> f64 {
        wr * avg_win - (1.0 - wr) * avg_loss
    }

    pub fn record_trade_result(&mut self, pnl: f64) {
        if pnl > 0.0 {
            self.trades_won += 1;
            self.total_win_amount += pnl;
            self.consecutive_losses = 0;
        } else {
            self.trades_lost += 1;
            self.total_loss_amount += pnl.abs();
            self.consecutive_losses = self.consecutive_losses.saturating_add(1);
            self.last_loss_ts = Some(chrono::Utc::now().timestamp_millis());
        }
    }
}

impl Strategy for MomentumStrategy {
    fn name(&self) -> &str { "momentum" }

    // ✅ V3.4: backfill-прогрев. Индикаторы (EMA200/Supertrend/ATR/regime)
    // прогреваются на 250 свечах истории сразу после старта, а не через
    // 8+ дней живых 1h-свечей. Сигналы по backfill запрещены guard'ом
    // внутри process() (if kline.is_backfill { return Ok(None); }).
    fn wants_backfill(&self) -> bool { true }

    fn notify_position_open(&mut self) {
        self.filter.on_position_opened();
    }

    // P16: sizing base follows live equity
    fn set_live_equity(&mut self, eq: f64) { self.balance = eq; }

    // ✅ P60/EQ-5: режимный множитель из RiskManager (ЭТАП C)
    fn set_risk_multiplier(&mut self, mult: f64) {
        if mult.is_finite() && mult > 0.0 {
            self.risk_multiplier = mult;
        }
    }

    fn notify_position_closed(&mut self, pnl: f64) {
        self.filter.on_position_closed(self.last_kline_ts);
        self.record_trade_result(pnl);
    }

    fn in_position(&self) -> bool {
        matches!(self.filter.state(), PositionState::InPosition)
    }

    fn diagnostics(&self) -> Option<String> {
        let total = self.blocked_by_atr
            + self.blocked_by_momentum
            + self.blocked_by_ofi
            + self.blocked_by_volume
            + self.blocked_by_regime
            + self.blocked_by_time
            + self.blocked_by_limit_day
            + self.blocked_by_cooldown
            + self.blocked_by_net_rr
            + self.blocked_by_edge
            + self.blocked_by_ema200
            + self.blocked_by_vol_spike
            + self.blocked_by_loss_streak
            + self.blocked_by_supertrend
            + self.blocked_by_mfi
            + self.blocked_by_roc
            + self.n_spread_block
            + self.passed_filters;

        Some(format!(
            "Momentum(candles) diag v3full | candles={} total={} | vol={} atr={} mom={} ofi={} regime={} time={} lim={} cd={} rr={} spr={} edge={} ema={} vspk={} lstreak={} st={} mfi={} roc={} passed={} | min_tp={:.4}% fee_rt={:.4}% lb={} atr_p={} ofi_win={} regime_range={:.3}% last_atr={:.4} rr_edge≥{:.2} trades={}/{} | ema200={} st_dir={} cons_loss={}",
            self.total_closed_candles, total,
            self.blocked_by_volume, self.blocked_by_atr, self.blocked_by_momentum, self.blocked_by_ofi,
            self.blocked_by_regime, self.blocked_by_time, self.blocked_by_limit_day,
            self.blocked_by_cooldown, self.blocked_by_net_rr, self.n_spread_block, self.blocked_by_edge,
            self.blocked_by_ema200, self.blocked_by_vol_spike, self.blocked_by_loss_streak,
            self.blocked_by_supertrend, self.blocked_by_mfi, self.blocked_by_roc,
            self.passed_filters,
            self.min_tp_pct() * 100.0, 2.0 * self.fee_rate * 100.0,
            self.lookback, self.atr_period, self.ofi_window,
            self.regime_detector.last_range_pct(), self.current_atr(),
            self.min_net_rr * EDGE_QUALITY_MULT,
            self.trades_today, self.max_trades_per_day,
            self.ema200_value.map(|v| format!("{:.4}", v)).unwrap_or_else(|| "n/a".to_string()),
            if self.supertrend_direction == 1 { "UP" } else { "DOWN" },
            self.consecutive_losses,
        ))
    }

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
        if !kline.symbol.eq_ignore_ascii_case(&self.symbol) { return Ok(None); }
        if !kline.is_closed { return Ok(None); }

        self.last_kline_ts = kline.timestamp;
        self.total_closed_candles += 1;

        let regime = self.regime_detector.detect_from_msg(msg);

        let day = kline.timestamp / 86_400_000;
        if day != self.current_day {
            self.current_day = day;
            self.trades_today = 0;
        }

        self.closes.push_back(kline.close);
        self.highs.push_back(kline.high);
        self.lows.push_back(kline.low);
        self.volumes.push_back(kline.volume);
        self.taker_buys.push_back(kline.taker_buy_volume);

        let max_hist = self.lookback.max(self.atr_period * 2).max(self.ofi_window).max(EMA200_PERIOD).max(120) + 10;
        while self.closes.len() > max_hist {
            self.closes.pop_front(); self.highs.pop_front(); self.lows.pop_front();
            self.volumes.pop_front(); self.taker_buys.pop_front();
        }

        self.update_atr();
        self.update_ema200(kline.close);
        self.update_supertrend();

        if kline.is_backfill { return Ok(None); }
        if !self.filter.can_signal(kline.close, kline.timestamp) { return Ok(None); }
        if self.closes.len() < self.lookback.max(self.atr_period) { return Ok(None); }

        // 🆕 ПАТЧ 3: Loss-streak cooldown
        if self.consecutive_losses >= LOSS_STREAK_THRESHOLD {
            if let Some(last_ts) = self.last_loss_ts {
                let now = chrono::Utc::now().timestamp_millis();
                if now - last_ts < LOSS_COOLDOWN_MS {
                    self.blocked_by_loss_streak += 1;
                    return Ok(None);
                }
            }
        }

        // 🆕 ПАТЧ 2: Volume spike rejection
        if self.is_volume_spike() {
            self.blocked_by_vol_spike += 1;
            tracing::debug!("🎯 [{}] vol-spike rejection", self.symbol);
            return Ok(None);
        }

        let vol_anom = self.volume_anomaly();
        if vol_anom < self.volume_anomaly_threshold {
            self.blocked_by_volume += 1;
            return Ok(None);
        }

        let atr = self.current_atr();
        let atr_pct = atr / kline.close;
        let min_atr_pct = self.min_atr_threshold_pct / 100.0;
        if atr_pct < min_atr_pct {
            self.blocked_by_atr += 1;
            return Ok(None);
        }

        let ofi = self.calculate_ofi();
        if ofi.abs() < self.ofi_threshold {
            self.blocked_by_ofi += 1;
            return Ok(None);
        }

        let momentum = self.calculate_momentum();
        if momentum.abs() < 0.0005 {
            self.blocked_by_momentum += 1;
            return Ok(None);
        }
        if (ofi > 0.0) != (momentum > 0.0) {
            self.blocked_by_momentum += 1;
            return Ok(None);
        }

        let is_long = momentum > 0.0;

        // 🆕 ПАТЧ 6: ROC — block SHORT на паническом дампе (ловля ножей)
        let roc = self.calculate_roc();
        let is_dump = roc.map(|r| r < ROC_DUMP_THRESHOLD).unwrap_or(false);
        if is_dump && !is_long {
            self.blocked_by_roc += 1;
            tracing::debug!(
                "🎯 [{}] ROC block SHORT: ROC={:.2}% — ловля ножей",
                self.symbol, roc.unwrap_or(0.0) * 100.0
            );
            return Ok(None);
        }

        // 🆕 ПАТЧ 5: MFI — фильтр боковика
        if regime != MarketRegime::Trending {
            if let Some(mfi) = self.calculate_mfi() {
                if mfi > MFI_OVERBOUGHT || mfi < MFI_OVERSOLD {
                    self.blocked_by_mfi += 1;
                    return Ok(None);
                }
            }
        }

        // 🆕 ПАТЧ 1+4: EMA200 + Supertrend — BINARY trend filter (оба фильтра)
        let above_ema = self.is_above_ema200(kline.close);
        let below_ema = self.is_below_ema200(kline.close);
        let st_up = self.supertrend_is_up();

        // LONG: close > EMA200 И Supertrend UP
        if is_long && (!above_ema || !st_up) {
            self.blocked_by_ema200 += 1;
            tracing::debug!(
                "🎯 [{}] trend block LONG: above_ema={} st_up={} — контр-тренд",
                self.symbol, above_ema, st_up
            );
            return Ok(None);
        }
        // SHORT: close < EMA200 И Supertrend DOWN (т.е. !st_up)
        if !is_long && (!below_ema || st_up) {
            self.blocked_by_ema200 += 1;
            tracing::debug!(
                "🎯 [{}] trend block SHORT: below_ema={} st_up={} — контр-тренд",
                self.symbol, below_ema, st_up
            );
            return Ok(None);
        }

        // --- Regime ---
        if regime != MarketRegime::Trending {
            self.blocked_by_regime += 1;
            return Ok(None);
        }

        if !self.is_trading_hour(kline.timestamp) {
            self.blocked_by_time += 1;
            return Ok(None);
        }

        if self.trades_today >= self.max_trades_per_day {
            self.blocked_by_limit_day += 1;
            return Ok(None);
        }
        if let Some(last_ts) = self.last_trade_ts_ms {
            if kline.timestamp - last_ts < self.min_time_between_trades_ms {
                self.blocked_by_cooldown += 1;
                return Ok(None);
            }
        }

        let min_tp = self.min_tp_pct();
        let tp_pct = (atr_pct * self.tp_atr_mult).max(min_tp).max(self.min_tp_pct).min(0.02);
        let sl_pct = tp_pct / self.rr_ratio;

        let (side, sl, tp) = if is_long {
            ("BUY", kline.close * (1.0 - sl_pct), kline.close * (1.0 + tp_pct))
        } else {
            ("SELL", kline.close * (1.0 + sl_pct), kline.close * (1.0 - tp_pct))
        };

        let entry = round_to_tick(kline.close, BTC_TICK_SIZE);
        let sl = round_to_tick(sl, BTC_TICK_SIZE);
        let tp = round_to_tick(tp, BTC_TICK_SIZE);

        let eff_rr = match self.net_rr(entry, sl, tp, is_long) {
            Some(rr) => rr,
            None => {
                self.blocked_by_net_rr += 1;
                return Ok(None);
            }
        };

        if eff_rr + 1e-4 < self.min_net_rr {
            self.blocked_by_net_rr += 1;
            return Ok(None);
        }
        if eff_rr < self.min_net_rr * EDGE_QUALITY_MULT {
            self.blocked_by_edge += 1;
            return Ok(None);
        }

        // ✅ P23: slippage-guard — спред шире лимита = вход запрещён
        if self.spread_too_wide() {
            return Ok(None);
        }

        let total_trades = self.trades_won + self.trades_lost;
        if total_trades >= 20 {
            let wr = self.trades_won as f64 / total_trades as f64;
            let avg_win = if self.trades_won > 0 { self.total_win_amount / self.trades_won as f64 } else { 0.0 };
            let avg_loss = if self.trades_lost > 0 { self.total_loss_amount / self.trades_lost as f64 } else { 0.0 };
            if avg_loss > 0.0 {
                let ev = self.expected_value(wr, avg_win, avg_loss);
                let ev_pct = ev / avg_loss * 100.0;
                if ev_pct < self.min_ev_pct {
                    return Ok(None);
                }
            }
        }

        let raw_qty = calc_position_size_with_fees(
            self.balance, entry, sl,
            // ✅ P60/EQ-5: риск × режимный множитель (никогда не растёт при падающем EV)
            self.risk_per_trade_pct * self.risk_multiplier, self.fee_rate * 2.0,
        );
        let quantity = floor_to_step(raw_qty, BTC_QTY_STEP);
        if quantity <= 0.0 { return Ok(None); }

        self.passed_filters += 1;
        self.trades_today += 1;
        self.last_trade_ts_ms = Some(kline.timestamp);
        self.filter.on_signal(entry, kline.timestamp);

        tracing::info!(
            "🎯 [{}] v3full signal: {} qty={:.5} @ {:.4} SL={:.4} TP={:.4} | EMA200={} ST={} ROC={:.2}% MFI={} vol_anom={:.2}x rr={:.2}",
            self.symbol, side, quantity, entry, sl, tp,
            self.ema200_value.map(|v| format!("{:.4}", v)).unwrap_or_else(|| "n/a".to_string()),
            if st_up { "UP" } else { "DOWN" },
            roc.map(|r| r * 100.0).unwrap_or(0.0),
            self.calculate_mfi().map(|m| format!("{:.0}", m)).unwrap_or_else(|| "n/a".to_string()),
            vol_anom, eff_rr,
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
            atr_hint: atr,
            entry_spread: 0.0,
        }))
    }
}

#[cfg(test)]
mod p24_tests {
    use super::*;

    // ✅ P24: funding строго ухудшает net_rr; =0 -> формула как до P24
    #[test]
    fn test_net_rr_funding_p24() {
        let mut s = MomentumStrategy::new("TAOUSDT".to_string(), 1000.0, 0.5);
        s.last_bid = 0.0; s.last_ask = 0.0; // без спреда — чистый эффект funding
        s.funding_pct = 0.0001;
        let with_f = s.net_rr(100.0, 99.0, 102.0, true).unwrap();
        s.funding_pct = 0.0;
        let no_f = s.net_rr(100.0, 99.0, 102.0, true).unwrap();
        let fee = 2.0 * DEFAULT_FEE_RATE * 100.0;
        let exp_with = (2.0 - fee - 0.01) / (1.0 + fee + 0.01);
        let exp_no = (2.0 - fee) / (1.0 + fee);
        assert!((with_f - exp_with).abs() < 1e-9, "with_f={with_f} exp={exp_with}");
        assert!((no_f - exp_no).abs() < 1e-9, "no_f={no_f} exp={exp_no}");
        assert!(with_f < no_f);
        // шорт — симметрично
        s.funding_pct = 0.0001;
        let sh = s.net_rr(100.0, 101.0, 98.0, false).unwrap();
        assert!(sh > 0.0 && sh < no_f);
    }

    // ✅ P24: большой funding гасит сделку -> None (стратегия молчит, не торгует в минус)
    #[test]
    fn test_net_rr_funding_none() {
        let mut s = MomentumStrategy::new("TAOUSDT".to_string(), 1000.0, 0.5);
        s.last_bid = 0.0; s.last_ask = 0.0;
        s.funding_pct = 0.5; // 50% за интервал — вырожденный случай
        assert_eq!(s.net_rr(100.0, 99.0, 100.05, true), None);
    }
}
