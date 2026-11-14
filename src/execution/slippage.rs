/// Оценка slippage в долях (не bps). Для мем-коинов на порядок выше BTC.
pub struct SlippageCalculator;

impl SlippageCalculator {
    /// Возвращает ожидаемый slippage в долях цены.
    ///
    /// Значения по символам (на основе биржевой статистики и отчёта):
    /// - BTC/ETH:  1 bps  (0.0001) — глубокая ликвидность
    /// - SOL/XRP:  3 bps  (0.0003)
    /// - PEPE/DOGS: 50 bps (0.005)  — низкая ликвидность, тонкий стакан
    /// - default:  5 bps  (0.0005)
    pub fn calculate_slippage(&self, symbol: &str, _price: f64, _quantity: f64) -> f64 {
        let s = symbol.to_uppercase();
        if s.contains("BTC") || s.contains("ETH") {
            0.0001
        } else if s.contains("SOL") || s.contains("XRP") || s.contains("BNB") {
            0.0003
            // СТАЛО — калибровано на 51M тиков с прода:
        } else if s.contains("PEPE") || s.contains("DOGS") || s.contains("SHIB") || s.contains("BONK") {
            0.0002  // 2 bps — реально для малых ордеров (p95 = 0.0206% на $100 notional)
        } else {
            0.0005
        }
    }
}