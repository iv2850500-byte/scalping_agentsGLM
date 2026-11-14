use crate::exchange::ExchangeClient;
use crate::state::StateManager;
use crate::strategies::Signal;
use crate::websocket::WebSocketMessage;
use anyhow::Result;
use std::sync::Arc;

pub struct StateAgent {
    manager: Arc<StateManager>,
    _exchange_client: Arc<dyn ExchangeClient>,
}

impl StateAgent {
    pub fn new(manager: Arc<StateManager>, exchange_client: Arc<dyn ExchangeClient>) -> Self {
        Self {
            manager,
            _exchange_client: exchange_client,
        }
    }

    pub async fn update(&self, msg: &WebSocketMessage) -> Result<Option<Signal>> {
        match msg {
            WebSocketMessage::Trade(trade) => {
                // ✅ P35/S12: кривая цена Trade (0 / NaN / inf) не должна
                // портить позицию (extreme_price, ложные SL/TP). Ветки
                // BookTicker и Kline уже гейтятся — выравниваем Trade с ними.
                if !trade.price.is_finite() || trade.price <= 0.0 {
                    return Ok(None);
                }
                if let Some(exit_signal) =
                    self.manager.check_and_update_position(&trade.symbol, trade.price)
                {
                    return Ok(Some(exit_signal));
                }
            }
            // ✅ 3г: цена позиции по миду bookTicker
            WebSocketMessage::BookTicker(bt) => {
                // ✅ P41/W5: добавлены конечность и bid<=ask (перекрёстный
                // стакан давал абсурдный mid = цену позиции).
                if bt.best_bid > 0.0
                    && bt.best_ask > 0.0
                    && bt.best_bid.is_finite()
                    && bt.best_ask.is_finite()
                    && bt.best_ask >= bt.best_bid
                {
                    let mid = (bt.best_bid + bt.best_ask) / 2.0;
                    if let Some(exit_signal) =
                        self.manager.check_and_update_position(&bt.symbol, mid)
                    {
                        return Ok(Some(exit_signal));
                    }
                }
            }
            // ✅ 6 (К-6 довод): цена позиции по свече — futures-боты остаются
            // живыми при обрыве WS: kline-поллер работает по REST.
            // ✅ П3: backfill-свечи (история при старте) ПРОПУСКАЕМ — старые
            // цены не должны срабатывать SL/TP восстановленных позиций.
            WebSocketMessage::Kline(k) => {
                if k.close > 0.0 && !k.is_backfill {
                    if let Some(exit_signal) =
                        self.manager.check_and_update_position(&k.symbol, k.close)
                    {
                        return Ok(Some(exit_signal));
                    }
                }
            }
            _ => {}
        }
        Ok(None)
    }

    pub async fn sync_if_needed(&self) -> Result<()> {
        Ok(())
    }
}