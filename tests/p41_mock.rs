//! ✅ P41: mock-тесты «стратегия → mock exchange» (аудит, P2-хвост).
//!
//! Контракт: сигнал в формате keltner_dca → ExecutionEngine (simulate =
//! mock-биржа без сети) → StateManager (sqlite в temp) → состояние позиции.
//! Дополнительно: S12-лестница (дозакупка суммирует объём, обновляет
//! initial_risk и персистит в БД), F11 floor_to_step, K6-валидация конфига.
//!
//! Запуск: `cargo test` (integration-тесты компилируются против lib).

use anyhow::Result;
use async_trait::async_trait;
use rust_decimal::Decimal;
use scalping_agents::config::AppConfig;
use scalping_agents::exchange::traits::{ExchangeClient, OrderResult};
use scalping_agents::execution::ExecutionEngine;
use scalping_agents::risk::GlobalRiskManager;
use scalping_agents::state::StateManager;
use scalping_agents::strategies::Signal;
use scalping_agents::utils::math::floor_to_step;

// ============================================================
// Mock exchange: заглушка контракта ExchangeClient. Simulate-путь
// движка REST не касается — заглушка нужна только для конструктора.
// ============================================================
struct MockExchange;

#[async_trait]
impl ExchangeClient for MockExchange {
    async fn place_market_order(
        &self, symbol: &str, side: &str, quantity: f64, _client_order_id: Option<String>,
    ) -> Result<OrderResult> {
        Ok(OrderResult {
            order_id: 1,
            symbol: symbol.to_string(),
            side: side.to_string(),
            price: 0.0,
            quantity,
            executed_qty: quantity,
            status: "FILLED".into(),
            commission: 0.0,
        })
    }

    async fn place_limit_order(
        &self, symbol: &str, side: &str, quantity: f64, price: f64, _id: Option<String>,
    ) -> Result<OrderResult> {
        Ok(OrderResult {
            order_id: 2,
            symbol: symbol.to_string(),
            side: side.to_string(),
            price,
            quantity,
            executed_qty: 0.0,
            status: "NEW".into(),
            commission: 0.0,
        })
    }

    async fn place_protective_order(
        &self, symbol: &str, side: &str, _stop: bool, trigger: f64, _id: Option<String>,
    ) -> Result<OrderResult> {
        Ok(OrderResult {
            order_id: 3,
            symbol: symbol.to_string(),
            side: side.to_string(),
            price: trigger,
            quantity: 0.0,
            executed_qty: 0.0,
            status: "NEW".into(),
            commission: 0.0,
        })
    }

    async fn cancel_order(&self, _symbol: &str, _order_id: u64) -> Result<bool> {
        Ok(true)
    }

    async fn get_order_status(&self, symbol: &str, order_id: u64) -> Result<OrderResult> {
        Ok(OrderResult {
            order_id,
            symbol: symbol.to_string(),
            side: "BUY".into(),
            price: 0.0,
            quantity: 0.0,
            executed_qty: 0.0,
            status: "CANCELED".into(),
            commission: 0.0,
        })
    }

    async fn get_order_by_client_id(
        &self, _symbol: &str, _client_order_id: &str,
    ) -> Result<OrderResult> {
        Err(anyhow::anyhow!("mock: not found"))
    }

    async fn get_open_orders(&self, _symbol: &str) -> Result<Vec<OrderResult>> {
        Ok(Vec::new())
    }

    async fn get_balance(&self, _asset: &str) -> Result<f64> {
        Ok(1000.0)
    }

    async fn get_order_book(
        &self, _symbol: &str, _limit: usize,
    ) -> Result<(Vec<(f64, f64)>, Vec<(f64, f64)>)> {
        Ok((vec![(100.0, 1.0)], vec![(100.1, 1.0)]))
    }

    async fn sync_time(&self) -> Result<()> {
        Ok(())
    }
}

// ============================================================
// Вспомогательное: конфиг из inline-TOML в temp-файле (реальный загрузчик
// AppConfig с deny_unknown_fields + validate()).
// ============================================================
fn write_test_config(tag: &str) -> Result<(String, String)> {
    let dir = std::env::temp_dir();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let db_path = dir.join(format!("p41_mock_{tag}_{ts}.sqlite"));
    let cfg_path = dir.join(format!("p41_mock_{tag}_{ts}.toml"));
    let toml_text = format!(
        r#"[global]
exchange = "binance"

[[bots]]
name = "p41_mock"
strategy = {{ name = "keltner_dca" }}
market_type = "futures"
simulate = true
api_key = "p41mockapikey01"
api_secret = "p41mockapisecret01"
db_path_override = "{}"
"#,
        db_path.to_string_lossy().replace('\\', "/")
    );
    std::fs::write(&cfg_path, toml_text)?;
    Ok((cfg_path.to_string_lossy().to_string(), db_path.to_string_lossy().to_string()))
}

fn keltner_signal(symbol: &str, side: &str, price: f64, qty: f64, sl: f64, tp: f64) -> Signal {
    Signal {
        symbol: symbol.to_string(),
        side: side.to_string(),
        price,
        quantity: qty,
        strategy_name: "keltner_dca".to_string(),
        leverage: 1.0,
        stop_loss: Some(sl),
        take_profit: Some(tp),
        atr_hint: 0.0,
        entry_spread: 0.0,
    }
}

// ============================================================
// Тест 1: сигнал keltner_dca → mock-exchange (simulate) → позиция в стейте.
// ============================================================
#[tokio::test]
async fn strategy_signal_executes_against_mock_exchange() -> Result<()> {
    let (cfg_path, _db) = write_test_config("entry")?;
    let app = AppConfig::load_from(&cfg_path)?;
    let global = app.global.clone();
    let bot = app.bots[0].clone();
    assert!(bot.simulate, "тест рассчитан на simulate-движок");

    let state = std::sync::Arc::new(StateManager::new(&bot, &global).await?);
    let engine = ExecutionEngine::new(
        &bot,
        std::sync::Arc::new(MockExchange),
        state.clone(),
        std::sync::Arc::new(GlobalRiskManager::new()),
        None,
        None,
    );

    let sig = keltner_signal("XAUUSDT", "BUY", 100.0, 1.0, 98.0, 101.0);
    let outcome = engine.execute(sig).await?;
    assert!(
        matches!(outcome, scalping_agents::execution::ExecOutcome::Entry { .. }),
        "первая транша должна исполниться: {outcome:?}"
    );

    let pos = state
        .get_positions()
        .into_iter()
        .find(|p| p.symbol == "XAUUSDT")
        .expect("позиция должна существовать после Entry");
    assert!((pos.quantity - 1.0).abs() < 1e-9, "qty первой транши");
    assert!((pos.entry_price - 100.0).abs() < 1e-9);
    assert_eq!(pos.stop_loss, Some(98.0));
    assert_eq!(pos.take_profit, Some(101.0));
    assert!((pos.initial_risk - 2.0).abs() < 1e-9, "initial_risk = |100-98|");
    Ok(())
}

// ============================================================
// Тест 2 (S12): дозакупка суммирует объём, ОБНОВЛЯЕТ initial_risk
// (раньше оставался риск первой транши) и персистит в БД.
// ============================================================
#[tokio::test]
async fn dca_ladder_updates_initial_risk_and_persists() -> Result<()> {
    let (cfg_path, db) = write_test_config("ladder")?;
    let app = AppConfig::load_from(&cfg_path)?;
    let global = app.global.clone();
    let bot = app.bots[0].clone();

    let state = std::sync::Arc::new(StateManager::new(&bot, &global).await?);
    let engine = ExecutionEngine::new(
        &bot,
        std::sync::Arc::new(MockExchange),
        state.clone(),
        std::sync::Arc::new(GlobalRiskManager::new()),
        None,
        None,
    );

    // транша 1: BUY 1.0 @ 100, SL 98 → risk/ед = 2.0
    let s1 = keltner_signal("XAUUSDT", "BUY", 100.0, 1.0, 98.0, 101.0);
    engine.execute(s1).await?;
    // транша 2: BUY 2.0 @ 98, SL 96.5 → risk/ед = 1.5 (обновляется!)
    let s2 = keltner_signal("XAUUSDT", "BUY", 98.0, 2.0, 96.5, 100.0);
    engine.execute(s2).await?;

    let pos = state
        .get_positions()
        .into_iter()
        .find(|p| p.symbol == "XAUUSDT")
        .expect("позиция после двух транш");
    assert!((pos.quantity - 3.0).abs() < 1e-9, "qty = 1+2, было {}", pos.quantity);
    let expected_avg = (100.0 * 1.0 + 98.0 * 2.0) / 3.0;
    assert!((pos.entry_price - expected_avg).abs() < 1e-9);
    assert!(
        (pos.initial_risk - 1.5).abs() < 1e-9,
        "S12: initial_risk должен обновиться на риск ВТОРОЙ транши (1.5), а не {}",
        pos.initial_risk
    );

    // S12: персист — переоткрываем StateManager на той же БД
    let state2 = StateManager::new(&bot, &global).await?;
    let pos2 = state2
        .get_positions()
        .into_iter()
        .find(|p| p.symbol == "XAUUSDT")
        .expect("позиция восстановлена из БД");
    assert!((pos2.quantity - 3.0).abs() < 1e-9, "qty персистится");
    assert!((pos2.initial_risk - 1.5).abs() < 1e-9, "initial_risk персистится (S12)");
    let _ = std::fs::remove_file(&db);
    Ok(())
}

// ============================================================
// Тест 3 (F11/N7): floor_to_step на границе шага.
// ============================================================
#[test]
fn floor_to_step_boundary_no_artifact() {
    assert!((floor_to_step(0.3, 0.1) - 0.3).abs() < 1e-12);
    assert!((floor_to_step(0.7, 0.1) - 0.7).abs() < 1e-12);
    assert!((floor_to_step(0.29, 0.1) - 0.2).abs() < 1e-12);
    assert!(floor_to_step(0.3, 0.1) <= 0.3, "инвариант floor");
}

// ============================================================
// Тест 4 (D15): порог CB и лосс — из конфига с дефолтами (3 / 0.10).
// ============================================================
#[test]
fn circuit_breaker_config_defaults() -> Result<()> {
    let (cfg_path, _db) = write_test_config("cb")?;
    let app = AppConfig::load_from(&cfg_path)?;
    let bot = &app.bots[0];
    assert_eq!(bot.circuit_breaker_threshold, 3, "дефолт порога CB");
    assert_eq!(bot.circuit_breaker_loss_usdt, Decimal::new(10, 1), "дефолт лосса CB");
    Ok(())
}
