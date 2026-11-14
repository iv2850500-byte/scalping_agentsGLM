use crate::config::BotConfig;
use crate::strategies::{Signal, Strategy};
use crate::websocket::WebSocketMessage;
use anyhow::Result;

pub struct StrategyAgent {
    name: String,
}

impl StrategyAgent {
    pub fn new(config: &BotConfig) -> Self {
        Self {
            name: format!("StrategyAgent_{}", config.name),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// ✅ П2 (Н-18): дедупликация ВЫРЕЗАНА. Старая версия сравнивала только
    /// symbol/side/strategy (без цены и времени), не сбрасывала last_signal
    /// и ПОЖИЗНЕННО глотала повторные сигналы: SpreadCapture после первого
    /// BUY замолкал до рестарта процесса. Анти-спам обеспечивает SignalFilter
    /// внутри стратегий (cooldown по event-time + price-delta).
    pub async fn process(
        &mut self,
        msg: &WebSocketMessage,
        strategy: &mut dyn Strategy,
    ) -> Result<Option<Signal>> {
        strategy.process(msg)
    }
}