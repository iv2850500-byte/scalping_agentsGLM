use crate::risk::RiskManager;
use crate::strategies::Signal;

pub struct RiskAgent {
    manager: RiskManager,
}

impl RiskAgent {
    pub fn new(manager: RiskManager) -> Self {
        Self { manager }
    }

    pub async fn check(&mut self, signal: &Signal) -> bool { // AUDIT-P4: &mut проброс
        self.manager.check(signal)
    }

    pub fn record_trade(&mut self, symbol: &str, pnl: f64) {
        self.manager.record_trade_result(symbol, pnl);
    }

    pub fn reset_circuit_breaker(&mut self) {
        self.manager.reset_circuit_breaker();
    }

    // ✅ bk9: авто-стоп
    pub fn is_suspended(&self) -> bool {
        self.manager.is_suspended()
    }

    pub fn suspend_reason(&self) -> Option<&str> {
        self.manager.suspend_reason()
    }

    pub fn resume(&mut self) {
        self.manager.resume();
    }

    // ============================================================
    // Геттеры для прометея
    // ============================================================
    pub fn win_rate(&self) -> f64 {
        self.manager.win_rate()
    }

    pub fn profit_factor(&self) -> f64 {
        self.manager.profit_factor()
    }

    pub fn total_trades(&self) -> u32 {
        self.manager.total_trades()
    }

    pub fn realized_pnl(&self) -> f64 {
        use rust_decimal::prelude::ToPrimitive;
        self.manager.realized_pnl().to_f64().unwrap_or(0.0)
    }

    /// 🔧 ЭТАП C: текущий множитель риска (GROWTH 1.0 / SURVIVAL 0.5 / HALT 0.0).
    /// Проводится в стратегию 30-сек циклом main.rs — сайзинг сделки
    /// умножается на него (риск никогда не растёт при падающем EV).
    // P16: expose live equity handle for strategy sizing sync
    pub fn equity_handle(&self) -> std::sync::Arc<std::sync::RwLock<f64>> {
        self.manager.equity_handle()
    }

    // ✅ P60/EQ-1: текущее живое equity (initial + realized PnL) — для
    // сайтовинга стратегий (проводится в strategy.set_live_equity, main.rs).
    pub fn live_equity(&self) -> f64 {
        self.manager.live_equity()
    }

    pub fn risk_multiplier(&self) -> f64 {
        self.manager.risk_multiplier()
    }

    /// ✅ 4в: сохранение риск-статистики в БД.
    /// ✅ Ф3: &mut self — внутри flush_stats бот читает и исполняет
    /// команды (resume) с дашборда.
    pub async fn flush_stats(&mut self) {
        self.manager.flush_stats().await;
    }
}