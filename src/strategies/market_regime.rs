use crate::websocket::WebSocketMessage;

#[derive(Debug, Clone, PartialEq)]
pub enum MarketRegime {
    Trending,
    Ranging,
    Volatile,
}

/// ✅ Patched MarketRegimeDetector с directional bias check.
///
/// Старая логика (только range):
///   range > volatile_pct → Volatile
///   range > trending_pct → Trending
///   else → Ranging
///
/// Проблема: не отличает **тренд** (цена 5→14% в одну сторону) от **пилы**
/// (5→10→5→10 — range 5%, но направления нет). Оба классифицировались как Trending.
///
/// Новая логика (directional bias):
///   directional_bias = |close - first_close| / range
///   = 1.0 если цена прошла range в одну сторону
///   = 0.0 если цена вернулась к старту (полная пила)
///
///   Volatile: range > volatile_pct AND directional_bias < 0.6
///   Trending: (range > trending_pct AND directional_bias >= 0.6)
///             OR (range > volatile_pct AND directional_bias >= 0.7)
///   Ranging: иначе
pub struct MarketRegimeDetector {
    window: usize,
    volatile_pct: f64,
    trending_pct: f64,
    directional_threshold: f64,  // 0.6 = 60% направленного движения
    strong_trend_threshold: f64, // 0.7 = 70% направленного движения
    price_history: Vec<f64>,
    last_directional_bias: f64,   // для diagnostics
}

impl MarketRegimeDetector {
    pub fn new(window: usize, volatile_pct: f64, trending_pct: f64) -> Self {
        Self {
            window: window.max(20),
            volatile_pct,
            trending_pct,
            directional_threshold: 0.6,
            strong_trend_threshold: 0.7,
            price_history: Vec::with_capacity(window.max(20) * 2),
            last_directional_bias: 0.0,
        }
    }

    pub fn with_atr_style(atr_period: usize, atr_threshold: f64) -> Self {
        let window = atr_period.max(10) * 20;
        let volatile = (atr_threshold / 100.0).max(0.1);
        let trending = (volatile / 6.0).max(0.02);
        Self::new(window, volatile, trending)
    }

    pub fn detect_from_msg(&mut self, msg: &WebSocketMessage) -> MarketRegime {
        match msg {
            WebSocketMessage::Trade(t) => {
                self.price_history.push(t.price);
            }
            WebSocketMessage::BookTicker(bt) => {
                let mid = (bt.best_bid + bt.best_ask) / 2.0;
                if mid > 0.0 {
                    self.price_history.push(mid);
                }
            }
            // ✅ ГЛАВНОЕ: обработка Kline
            WebSocketMessage::Kline(k) => {
                if k.close > 0.0 {
                    self.price_history.push(k.close);
                }
            }
            _ => return MarketRegime::Ranging,
        }

        if self.price_history.len() > self.window {
            let excess = self.price_history.len() - self.window;
            self.price_history.drain(0..excess);
        }

        if self.price_history.len() < 20 {
            return MarketRegime::Ranging;
        }

        let last = match self.price_history.last() {
            Some(&p) if p > 0.0 => p,
            _ => return MarketRegime::Ranging,
        };

        let max = self
            .price_history
            .iter()
            .cloned()
            .fold(f64::NEG_INFINITY, f64::max);
        let min = self
            .price_history
            .iter()
            .cloned()
            .fold(f64::INFINITY, f64::min);
        let range_pct = (max - min) / last * 100.0;

        // ✅ Directional bias check (новое)
        let first = self.price_history[0];
        let directional_move = (last - first).abs() / last * 100.0;
        let directional_bias = if range_pct > 0.0 {
            directional_move / range_pct
        } else {
            0.0
        };
        self.last_directional_bias = directional_bias;

        // === НОВАЯ ЛОГИКА ===
        // Volatile: широкий range БЕЗ направления (пила)
        if range_pct > self.volatile_pct && directional_bias < self.strong_trend_threshold {
            return MarketRegime::Volatile;
        }

        // Trending: есть диапазон и направление
        if range_pct > self.trending_pct && directional_bias >= self.directional_threshold {
            return MarketRegime::Trending;
        }

        // Если range > volatile_pct но directional_bias >= strong_trend_threshold →
        // это сильный тренд даже в волатильной среде
        if range_pct > self.volatile_pct && directional_bias >= self.strong_trend_threshold {
            return MarketRegime::Trending;
        }

        MarketRegime::Ranging
    }

    pub fn last_range_pct(&self) -> f64 {
        if self.price_history.len() < 2 {
            return 0.0;
        }
        let last = *self.price_history.last().unwrap();
        if last <= 0.0 {
            return 0.0;
        }
        let max = self
            .price_history
            .iter()
            .cloned()
            .fold(f64::NEG_INFINITY, f64::max);
        let min = self
            .price_history
            .iter()
            .cloned()
            .fold(f64::INFINITY, f64::min);
        (max - min) / last * 100.0
    }

    /// ✅ Для diagnostics: показывает directional bias (0.0 - 1.0)
    /// 0.0 = полная пила, 1.0 = идеальный направленный тренд
    pub fn last_directional_bias(&self) -> f64 {
        self.last_directional_bias
    }
}
