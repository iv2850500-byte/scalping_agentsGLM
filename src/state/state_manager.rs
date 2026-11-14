use crate::config::{BotConfig, GlobalConfig};
use crate::strategies::Signal;
use anyhow::Result;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use rust_decimal::prelude::ToPrimitive;
use sqlx::sqlite::SqlitePool;
use std::sync::Arc;

const BREAKEVEN_AT_R: f64 = 1.5;
// 🔧 ОТКЛЮЧЕНО (эксперимент momentum): partial@1.5R срезал winners
// (win урезался с ~0.14 до ~0.02 USDT). Вернуть при включении partial.
// const PARTIAL_AT_R: f64 = 2.0;
// const PARTIAL_FRACTION: f64 = 0.5;
const TRAIL_ATR_MULT: f64 = 2.0;

// ✅ P41/E14: A4-трейлинг SpreadCapture и его константы удалены вместе с
// мёртвой веткой в check_and_update_position (стратегии SpreadCapture
// больше нет в фабрике — origin появиться не может).

/// 🔧 ЭТАП B: частота подтяжки биржевого стопа — не чаще, чем раз в N секунд
/// (анти-спам cancel+place при каждом тике).
pub const TRAIL_SYNC_MIN_INTERVAL_MS: i64 = 10_000;

fn round_to(value: f64, decimals: usize) -> f64 {
    let s = format!("{:.*}", decimals, value);
    s.parse::<f64>().unwrap_or(value)
}

fn price_precision(symbol: &str) -> usize {
    if symbol.contains("PEPE") || symbol.contains("DOGS") {
        8
    } else {
        2
    }
}

/// ✅ P35/S11: строгий парсинг side. Раньше любое значение БД кроме точного
/// "BUY" становилось Short — мусорная строка восстанавливалась как шорт
/// с entry_price из БД. Теперь неизвестное значение отбрасывается с error!.
fn parse_side(side: &str) -> Option<PositionSide> {
    match side.trim().to_ascii_uppercase().as_str() {
        "BUY" => Some(PositionSide::Long),
        "SELL" => Some(PositionSide::Short),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct Position {
    pub symbol: String,
    pub side: PositionSide,
    pub entry_price: f64,
    pub quantity: f64,
    pub current_price: f64,
    pub extreme_price: f64,
    pub unrealized_pnl: f64,
    pub opened_at: DateTime<Utc>,
    pub stop_loss: Option<f64>,
    pub take_profit: Option<f64>,
    pub sl_order_id: Option<String>,
    pub tp_order_id: Option<String>,

    pub origin_strategy: String,
    pub funding_accrued: f64,
    pub fees_paid: f64,

    pub initial_risk: f64,
    pub atr_hint: f64,
    pub partial_taken: bool,

    /// Спред на момент входа (P41/E14: потребитель A4-трейлинг удалён,
/// поле остаётся для журнала и RR-диагностики).
    pub entry_spread: f64,

    /// 🔧 ЭТАП B: время последней синхронизации биржевого стопа (анти-спам)
    pub last_trail_sync_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PositionSide {
    Long,
    Short,
}

pub struct StateManager {
    pub config: BotConfig,
    positions: Arc<DashMap<String, Position>>,
    pool: SqlitePool,
    fee_rate: f64,
}

impl StateManager {
    pub async fn new(config: &BotConfig, global: &GlobalConfig) -> Result<Self> {
        let db_path_str = config
            .db_path_override
            .as_ref()
            .unwrap_or(&global.db_path);

        let db_path = std::path::PathBuf::from(db_path_str);
        if let Some(parent) = db_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
        let pool = SqlitePool::connect(&db_url).await?;

        let _ = sqlx::query("PRAGMA journal_mode=WAL").fetch_all(&pool).await;
        let _ = sqlx::query("PRAGMA busy_timeout=5000").fetch_all(&pool).await;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS positions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                bot_name TEXT NOT NULL,
                symbol TEXT NOT NULL,
                side TEXT NOT NULL,
                entry_price REAL NOT NULL,
                quantity REAL NOT NULL,
                current_price REAL,
                stop_loss REAL,
                take_profit REAL,
                opened_at TEXT NOT NULL,
                origin_strategy TEXT DEFAULT '',
                funding_accrued REAL DEFAULT 0,
                fees_paid REAL DEFAULT 0,
                initial_risk REAL DEFAULT 0,
                atr_hint REAL DEFAULT 0,
                partial_taken INTEGER DEFAULT 0,
                entry_spread REAL DEFAULT 0
            )"#,
        )
        .execute(&pool)
        .await?;

        // Миграции
        sqlx::query("ALTER TABLE positions ADD COLUMN origin_strategy TEXT DEFAULT ''")
            .execute(&pool).await.ok();
        sqlx::query("ALTER TABLE positions ADD COLUMN funding_accrued REAL DEFAULT 0")
            .execute(&pool).await.ok();
        sqlx::query("ALTER TABLE positions ADD COLUMN extreme_price REAL")
            .execute(&pool).await.ok();
        sqlx::query("ALTER TABLE positions ADD COLUMN fees_paid REAL DEFAULT 0")
            .execute(&pool).await.ok();
        sqlx::query("ALTER TABLE positions ADD COLUMN initial_risk REAL DEFAULT 0")
            .execute(&pool).await.ok();
        sqlx::query("ALTER TABLE positions ADD COLUMN atr_hint REAL DEFAULT 0")
            .execute(&pool).await.ok();
        sqlx::query("ALTER TABLE positions ADD COLUMN partial_taken INTEGER DEFAULT 0")
            .execute(&pool).await.ok();
        sqlx::query("ALTER TABLE positions ADD COLUMN entry_spread REAL DEFAULT 0")
            .execute(&pool).await.ok();
        // 🔧 ЭТАП B: id защитных ордеров персистятся — рестарт знает, что сторожит
        sqlx::query("ALTER TABLE positions ADD COLUMN sl_order_id TEXT")
            .execute(&pool).await.ok();
        sqlx::query("ALTER TABLE positions ADD COLUMN tp_order_id TEXT")
            .execute(&pool).await.ok();

        let positions = Arc::new(DashMap::new());
        // ✅ P35/S6: extreme_price добавлен в SELECT. Раньше колонка не
        // выбиралась: try_get("extreme_price") всегда падал в
        // unwrap_or(entry_price), и трейлинг восстановленной позиции
        // считался от входа, а не от фактического экстремума.
        let rows = sqlx::query(
            "SELECT symbol, side, entry_price, quantity, current_price, extreme_price, \
             stop_loss, take_profit, opened_at, origin_strategy, funding_accrued, fees_paid, \
             initial_risk, atr_hint, partial_taken, entry_spread, \
             sl_order_id, tp_order_id \
             FROM positions WHERE bot_name = ?",
        )
        .bind(&config.name)
        .fetch_all(&pool)
        .await?;

        for row in rows {
            use sqlx::Row;
            let symbol: String = row.try_get("symbol").unwrap_or_default();
            let side: String = row.try_get("side").unwrap_or_default();
            let entry_price: f64 = row.try_get("entry_price").unwrap_or(0.0);
            let quantity: f64 = row.try_get("quantity").unwrap_or(0.0);
            let current_price: f64 = row.try_get("current_price").unwrap_or(entry_price);
            let stop_loss: Option<f64> = row.try_get("stop_loss").ok();
            let take_profit: Option<f64> = row.try_get("take_profit").ok();
            let opened_at_str: String = row.try_get("opened_at").unwrap_or_default();
            let opened_at = DateTime::parse_from_rfc3339(&opened_at_str)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());

            let origin_strategy: String = row.try_get("origin_strategy").unwrap_or_default();
            let funding_accrued: f64 = row.try_get("funding_accrued").unwrap_or(0.0);
            let fees_paid: f64 = row.try_get("fees_paid").unwrap_or(0.0);
            let mut initial_risk: f64 = row.try_get("initial_risk").unwrap_or(0.0);
            let atr_hint: f64 = row.try_get("atr_hint").unwrap_or(0.0);
            let partial_taken: bool = row.try_get::<i64, _>("partial_taken").unwrap_or(0) != 0;
            let entry_spread: f64 = row.try_get("entry_spread").unwrap_or(0.0);
            let sl_order_id: Option<String> = row.try_get("sl_order_id").ok();
            let tp_order_id: Option<String> = row.try_get("tp_order_id").ok();

            if initial_risk <= 0.0 {
                if let Some(sl) = stop_loss {
                    initial_risk = (entry_price - sl).abs();
                }
            }

            // ✅ P35/S11: неизвестный side — строка ПРОПУСКАЕТСЯ с error!,
            // а не превращается в шорт.
            let ps = match parse_side(&side) {
                Some(v) => v,
                None => {
                    tracing::error!(
                        "[{}] ⚠️ позиция {} в БД с неизвестным side='{}' — ПРОПУЩЕНА",
                        config.name, symbol, side
                    );
                    continue;
                }
            };

            positions.insert(
                format!("{}_{}", config.name, symbol),
                Position {
                    symbol,
                    side: ps,
                    entry_price,
                    quantity,
                    current_price,
                    extreme_price: row.try_get("extreme_price").unwrap_or(entry_price),
                    unrealized_pnl: 0.0,
                    opened_at,
                    stop_loss,
                    take_profit,
                    sl_order_id,
                    tp_order_id,
                    origin_strategy,
                    funding_accrued,
                    fees_paid,
                    initial_risk,
                    atr_hint,
                    partial_taken,
                    entry_spread,
                    last_trail_sync_ms: 0,
                },
            );
        }

        if !positions.is_empty() {
            tracing::warn!(
                "[{}] ⚠️ Восстановлено {} позиций из БД — проверьте на бирже!",
                config.name,
                positions.len()
            );
        }

        // ✅ P22: фолбэк комиссии выровнен с ExecutionEngine (futures taker
        // 0.0005 / spot 0.00075). Раньше здесь был 0.001: при отсутствии
        // fee_rate в конфиге BE-цена и net-PnL считались по двойной комиссии.
        let fee_rate = config.fee_rate.map(|d| d.to_f64().unwrap_or(0.0005)).unwrap_or(
            if config.market_type == "futures" { 0.0005 } else { 0.00075 },
        );

        tracing::info!(
            "[{}] 💸 Fee rate: {:.5} ({:.3}%) на сторону — round-trip {:.3}%",
            config.name, fee_rate, fee_rate * 100.0, fee_rate * 2.0 * 100.0
        );

        Ok(Self {
            config: config.clone(),
            positions,
            pool,
            fee_rate,
        })
    }

    pub fn has_position(&self, symbol: &str) -> bool {
        let key = format!("{}_{}", self.config.name, symbol);
        self.positions.contains_key(&key)
    }

    // ============================================================
    // 🔧 ЭТАП B: id защитных ордеров
    // ============================================================
    pub async fn set_sl_order_id(&self, symbol: &str, id: String) {
        let key = format!("{}_{}", self.config.name, symbol);
        if let Some(mut pos) = self.positions.get_mut(&key) {
            pos.sl_order_id = Some(id.clone());
        }
        let _ = sqlx::query("UPDATE positions SET sl_order_id=? WHERE bot_name=? AND symbol=?")
            .bind(&id).bind(&self.config.name).bind(symbol)
            .execute(&self.pool).await;
    }

    pub async fn set_tp_order_id(&self, symbol: &str, id: String) {
        let key = format!("{}_{}", self.config.name, symbol);
        if let Some(mut pos) = self.positions.get_mut(&key) {
            pos.tp_order_id = Some(id.clone());
        }
        let _ = sqlx::query("UPDATE positions SET tp_order_id=? WHERE bot_name=? AND symbol=?")
            .bind(&id).bind(&self.config.name).bind(symbol)
            .execute(&self.pool).await;
    }

    /// (sl_order_id, tp_order_id)
    pub fn get_protective_ids(&self, symbol: &str) -> (Option<String>, Option<String>) {
        let key = format!("{}_{}", self.config.name, symbol);
        match self.positions.get(&key) {
            Some(p) => (p.sl_order_id.clone(), p.tp_order_id.clone()),
            None => (None, None),
        }
    }

    /// 🔧 ЭТАП B: можно ли сейчас подтягивать стоп (анти-спам)
    pub fn trail_sync_allowed(&self, symbol: &str) -> bool {
        let key = format!("{}_{}", self.config.name, symbol);
        match self.positions.get(&key) {
            Some(p) => {
                chrono::Utc::now().timestamp_millis() - p.last_trail_sync_ms
                    >= TRAIL_SYNC_MIN_INTERVAL_MS
            }
            None => false,
        }
    }

    pub fn mark_trail_sync(&self, symbol: &str) {
        let key = format!("{}_{}", self.config.name, symbol);
        if let Some(mut pos) = self.positions.get_mut(&key) {
            pos.last_trail_sync_ms = chrono::Utc::now().timestamp_millis();
        }
    }

    // ============================================================
    // Проверки SL/TP/менеджмента (двусторонний momentum)
    // ============================================================
    pub fn check_and_update_position(&self, symbol: &str, current_price: f64) -> Option<Signal> {
        let key = format!("{}_{}", self.config.name, symbol);
        if let Some(mut pos) = self.positions.get_mut(&key) {
            pos.current_price = current_price;
            if pos.side == PositionSide::Long && current_price > pos.extreme_price {
                pos.extreme_price = current_price;
            } else if pos.side == PositionSide::Short && current_price < pos.extreme_price {
                pos.extreme_price = current_price;
            }

            // ✅ P35/S9a: .max(0) — если opened_at в будущем (скью часов после
            // восстановления из БД), num_seconds() отрицателен, и cast as u64
            // давал ~1.8e19 → ложный Exit_Timeout с первого же тика.
            let dur = Utc::now().signed_duration_since(pos.opened_at).num_seconds().max(0) as u64;
            if dur >= self.config.max_position_duration_sec {
                return Some(self.create_close_signal(&pos, "Timeout"));
            }

            pos.unrealized_pnl = match pos.side {
                PositionSide::Long => (current_price - pos.entry_price) * pos.quantity,
                PositionSide::Short => (pos.entry_price - current_price) * pos.quantity,
            };

            let profit_per_unit = match pos.side {
                PositionSide::Long => current_price - pos.entry_price,
                PositionSide::Short => pos.entry_price - current_price,
            };
            let r_progress = if pos.initial_risk > 0.0 {
                profit_per_unit / pos.initial_risk
            } else {
                0.0
            };

            // ✅ P41/E14: мёртвая ветка A4-трейлинга SpreadCapture удалена. Origin
            // "SpreadCapture" в позициях появиться не может (P15 убрал стратегию
            // из фабрики). Гипотетические legacy-строки с таким origin теперь
            // управляются общим Momentum-путём ниже (BE/Chandelier/SL/TP-хиты там есть).

            // ============================================================
            // Momentum: книжный менеджмент (BE/Chandelier)
            // 🔧 ЭТАП B: partial ОТКЛЮЧЁН (эксперимент: partial срезал winners)
            // 🔧 ЭТАП B: при сдвиге SL вперёд — сигнал синхронизации биржевого
            // стопа (Exit_UpdateSL), если защита стоит на бирже.
            // ============================================================
            let mut sl_moved_to: Option<f64> = None;

            if r_progress >= BREAKEVEN_AT_R {
                let be_price = match pos.side {
                    PositionSide::Long => pos.entry_price * (1.0 + self.fee_rate * 2.0),
                    PositionSide::Short => pos.entry_price * (1.0 - self.fee_rate * 2.0),
                };
                // ✅ FIX: unwrap_or(NEG_INFINITY.max(be_price)) давал для Long
                // без SL cur = be_price → better=false → BE никогда не ставился.
                let cur = pos.stop_loss.unwrap_or(match pos.side {
                    PositionSide::Long => f64::NEG_INFINITY,
                    PositionSide::Short => f64::INFINITY,
                });
                let better = match pos.side {
                    PositionSide::Long => be_price > cur,
                    PositionSide::Short => be_price < cur,
                };
                if better {
                    pos.stop_loss = Some(be_price);
                    sl_moved_to = Some(be_price);
                    tracing::info!(
                        "[{}] 🛡 BE+1R: SL {} → {:.prec$} (сделка защищена в безубыток)",
                        self.config.name, pos.symbol, be_price,
                        prec = price_precision(&pos.symbol)
                    );
                }
            }

            // [P9:trailing-mode] "atr" (default = old behavior) | "percent"
            // (config keys always) | "tightest" (min of active mechanisms).
            let trail_dist = {
                let offset = self
                    .config
                    .break_even_offset_percent
                    .to_f64()
                    .unwrap_or(0.05)
                    / 100.0;
                let activation = self
                    .config
                    .trailing_stop_activation_percent
                    .to_f64()
                    .unwrap_or(0.15)
                    / 100.0;
                let profit_pct = profit_per_unit / pos.entry_price;
                let pct_dist = if profit_pct >= activation && offset > 0.0 {
                    pos.extreme_price * offset
                } else {
                    0.0
                };
                let atr_dist = if pos.atr_hint > 0.0 {
                    TRAIL_ATR_MULT * pos.atr_hint
                } else {
                    0.0
                };
                match self.config.trailing_mode.as_str() {
                    "percent" => pct_dist,
                    "tightest" => {
                        if atr_dist > 0.0 && pct_dist > 0.0 {
                            atr_dist.min(pct_dist)
                        } else if pct_dist > 0.0 {
                            pct_dist
                        } else {
                            atr_dist
                        }
                    }
                    _ => {
                        if atr_dist > 0.0 {
                            atr_dist
                        } else {
                            pct_dist
                        }
                    }
                }
            };

                        // ============================================================
            // 🔧 TRAILING EXIT: выход при распаде прибыли.
            // Пик R капается; падение с пика до +0.15R (покрывает комиссии)
            // = немедленный выход. Иначе winner тает до BE-нуля/таймаута
            // и превращается в −0.02..−0.03 после taker-комиссии.
            // ============================================================
            const TRAIL_EXIT_PEAK_R: f64 = 0.5;   // активация после +0.5R
            const TRAIL_EXIT_FLOOR_R: f64 = 0.15; // выход при распаде до +0.15R

            let peak_r = if pos.initial_risk > 0.0 {
                let peak_profit = match pos.side {
                    PositionSide::Long => pos.extreme_price - pos.entry_price,
                    PositionSide::Short => pos.entry_price - pos.extreme_price,
                };
                peak_profit / pos.initial_risk
            } else {
                0.0
            };

            // ✅ FIX: условие r_progress >= 0 убрано — если цена проскочила
            // ноль между тиками (5с), выход не срабатывал и позиция ехала
            // до полного SL (-1R). Теперь фиксируем остаток по рынку при
            // любом значении ниже пола +0.15R.
            if peak_r >= TRAIL_EXIT_PEAK_R && r_progress <= TRAIL_EXIT_FLOOR_R {
                tracing::info!(
                    "[{}] 🪂 TRAILING EXIT: пик R={:+.2} упал до {:+.2} — фиксирую остаток",
                    self.config.name, peak_r, r_progress
                );
                return Some(self.create_close_signal(&pos, "TrailExit"));
            }

                       // 🔧 ПОЛУ-ТАЙМАУТ: 70% срока при плоской позиции — выход
            if dur >= self.config.max_position_duration_sec * 7 / 10
                && r_progress.abs() < 0.1
            {
                return Some(self.create_close_signal(&pos, "TimeoutFlat"));
            } 

            if trail_dist > 0.0 {
                let chandelier = match pos.side {
                    PositionSide::Long => pos.extreme_price - trail_dist,
                    PositionSide::Short => pos.extreme_price + trail_dist,
                };
                let better = match (pos.stop_loss, pos.side) {
                    (Some(cur), PositionSide::Long) => chandelier > cur,
                    (Some(cur), PositionSide::Short) => chandelier < cur,
                    (None, _) => true,
                };
                if better {
                    pos.stop_loss = Some(chandelier);
                    sl_moved_to = Some(chandelier);
                }
            }

            // 🔧 ЭТАП B: SL сдвинулся вперёд И биржевая защита стоит →
            // просим движок синхронизировать STOP_MARKET (не чаще 10с)
            // ✅ P35/S1 (CRITICAL): убран самодедлок DashMap. Раньше при живом
            // RefMut от positions.get_mut(&key) звались trail_sync_allowed и
            // mark_trail_sync — оба внутри делают positions.get/get_mut ПО ТОМУ
            // ЖЕ ключу → нереентерабельный RwLock шарда → зависает весь цикл
            // бота (в real/тестнет-режиме — при первом же сдвиге SL вперёд
            // при стоящей биржевой защите). Троттлинг теперь читается и
            // пишется напрямую через pos, без обращения к карте.
            if let Some(new_sl) = sl_moved_to {
                let now_ms = Utc::now().timestamp_millis();
                if pos.sl_order_id.is_some()
                    && now_ms - pos.last_trail_sync_ms >= TRAIL_SYNC_MIN_INTERVAL_MS
                {
                    pos.last_trail_sync_ms = now_ms;
                    return Some(Signal {
                        symbol: pos.symbol.clone(),
                        side: if pos.side == PositionSide::Long { "SELL".into() } else { "BUY".into() },
                        price: new_sl,
                        quantity: 0.0, // 0 = НЕ закрывать, только синхронизация стопа
                        strategy_name: "Exit_UpdateSL".into(),
                        leverage: 1.0,
                        stop_loss: Some(new_sl),
                        take_profit: pos.take_profit,
                        atr_hint: pos.atr_hint,
                        entry_spread: pos.entry_spread,
                    });
                }
            }

            if let Some(sl) = pos.stop_loss {
                let hit = match pos.side {
                    PositionSide::Long => current_price <= sl,
                    PositionSide::Short => current_price >= sl,
                };
                if hit {
                    return Some(self.create_close_signal(&pos, "StopLoss"));
                }
            }
            if let Some(tp) = pos.take_profit {
                let hit = match pos.side {
                    PositionSide::Long => current_price >= tp,
                    PositionSide::Short => current_price <= tp,
                };
                if hit {
                    return Some(self.create_close_signal(&pos, "TakeProfit"));
                }
            }
        }
        None
    }

    fn create_close_signal(&self, pos: &Position, reason: &str) -> Signal {
        Signal {
            symbol: pos.symbol.clone(),
            side: if pos.side == PositionSide::Long {
                "SELL".into()
            } else {
                "BUY".into()
            },
            price: pos.current_price,
            quantity: pos.quantity,
            strategy_name: format!("Exit_{}", reason),
            leverage: 1.0,
            stop_loss: pos.stop_loss,
            take_profit: pos.take_profit,
            atr_hint: pos.atr_hint,
            entry_spread: pos.entry_spread,
        }
    }

    pub async fn add_position(
        &self,
        symbol: String,
        side: String,
        entry_price: f64,
        quantity: f64,
        stop_loss: Option<f64>,
        take_profit: Option<f64>,
        origin_strategy: String,
    ) {
        self.add_position_with_risk(
            symbol, side, entry_price, quantity, stop_loss, take_profit,
            origin_strategy, 0.0, 0.0, 0.0,
        )
        .await;
    }

    /// Расширенное открытие. entry_spread — спред момента входа.
    pub async fn add_position_with_risk(
        &self,
        symbol: String,
        side: String,
        entry_price: f64,
        quantity: f64,
        stop_loss: Option<f64>,
        take_profit: Option<f64>,
        origin_strategy: String,
        initial_risk: f64,
        atr_hint: f64,
        entry_spread: f64,
    ) {
        // ✅ P35/S11: неизвестный side — отказ вместо молчаливого Short.
        let ps = match parse_side(&side) {
            Some(v) => v,
            None => {
                tracing::error!(
                    "[{}] ⚠️ add_position: неизвестный side='{}' для {} — позиция НЕ создана",
                    self.config.name, side, symbol
                );
                return;
            }
        };
        let opened_at = Utc::now();

        let quantity = round_to(quantity, 8);

        let initial_risk = if initial_risk > 0.0 {
            initial_risk
        } else if let Some(sl) = stop_loss {
            (entry_price - sl).abs()
        } else {
            0.0
        };

        let pos = Position {
            symbol: symbol.clone(),
            side: ps,
            entry_price,
            quantity,
            current_price: entry_price,
            extreme_price: entry_price,
            unrealized_pnl: 0.0,
            opened_at,
            stop_loss,
            take_profit,
            sl_order_id: None,
            tp_order_id: None,
            origin_strategy: origin_strategy.clone(),
            funding_accrued: 0.0,
            fees_paid: 0.0,
            initial_risk,
            atr_hint,
            partial_taken: false,
            entry_spread,
            last_trail_sync_ms: 0,
        };
        self.positions
            .insert(format!("{}_{}", self.config.name, symbol.clone()), pos);

        if initial_risk > 0.0 && !origin_strategy.is_empty() && origin_strategy != "Reconciled" {
            tracing::info!(
                "[{}] 📐 R-менеджмент: {} риск/ед = {:.8} ({:.3}%), ATR={:.8}, спред={:.4}% | BE@+1R, trail",
                self.config.name, symbol, initial_risk,
                initial_risk / entry_price * 100.0, atr_hint, entry_spread * 100.0
            );
        }

        let _ = sqlx::query(
            "INSERT INTO positions \
             (bot_name, symbol, side, entry_price, quantity, current_price, \
              stop_loss, take_profit, opened_at, origin_strategy, funding_accrued, fees_paid, \
              initial_risk, atr_hint, partial_taken, entry_spread) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&self.config.name)
        .bind(&symbol)
        .bind(&side)
        .bind(entry_price)
        .bind(quantity)
        .bind(entry_price)
        .bind(stop_loss)
        .bind(take_profit)
        .bind(opened_at.to_rfc3339())
        .bind(&origin_strategy)
        .bind(0.0_f64)
        .bind(0.0_f64)
        .bind(initial_risk)
        .bind(atr_hint)
        .bind(0i64)
        .bind(entry_spread)
        .execute(&self.pool)
        .await;
    }

    pub fn origin_strategy_for(&self, symbol: &str) -> Option<String> {
        let key = format!("{}_{}", self.config.name, symbol);
        self.positions
            .get(&key)
            .map(|p| p.origin_strategy.clone())
    }

    pub fn accrue_funding(&self, symbol: &str, amount_usdt: f64) {
        let key = format!("{}_{}", self.config.name, symbol);
        if let Some(mut pos) = self.positions.get_mut(&key) {
            pos.funding_accrued += amount_usdt;
        }
    }

    pub fn add_fees_paid(&self, symbol: &str, amount: f64) {
        if amount <= 0.0 {
            return;
        }
        let key = format!("{}_{}", self.config.name, symbol);
        if let Some(mut pos) = self.positions.get_mut(&key) {
            pos.fees_paid += amount;
        }
    }

    pub fn open_symbols(&self) -> Vec<String> {
        self.positions
            .iter()
            .map(|e| e.value().symbol.clone())
            .collect()
    }

    pub async fn close_position(&self, symbol: &str, exec_price: f64, exec_qty: f64) -> f64 {
        let key = format!("{}_{}", self.config.name, symbol);

        let Some((_, pos)) = self.positions.remove(&key) else {
            tracing::warn!(
                "[{}] close_position {}: позиции нет — PnL=0 (повтор/фантом)",
                self.config.name, symbol
            );
            return 0.0;
        };

        let prec = price_precision(symbol);
        let entry = pos.entry_price;
        let exit = exec_price;
        let qty = round_to(exec_qty, 8);

        if qty <= 0.0 {
            self.positions.insert(key, pos);
            return 0.0;
        }

        let gross_pnl = match pos.side {
            PositionSide::Long => (exit - entry) * qty,
            PositionSide::Short => (entry - exit) * qty,
        };

        let remaining = pos.quantity - qty;
        let dust = (pos.quantity * 1e-6).max(1e-9);

        if remaining > dust {
            // Частичное закрытие (для полноты; в momentum-эксперименте отключено)
            let share = qty / pos.quantity;
            let fees = if pos.fees_paid > 0.0 {
                pos.fees_paid * share
            } else {
                (entry + exit) * qty * self.fee_rate
            };
            let funding_share = pos.funding_accrued * share;
            let net_pnl = gross_pnl - fees + funding_share;

            let rest_qty = round_to(remaining, 8);
            let rest_funding = pos.funding_accrued - funding_share;
            let rest_fees = if pos.fees_paid > 0.0 { pos.fees_paid - fees } else { 0.0 };
            let dur = Utc::now().signed_duration_since(pos.opened_at).num_seconds();

            tracing::warn!(
                "💰 ЧАСТИЧНОЕ ЗАКРЫТИЕ [{}] {} | {} {:.5} из {:.5} | вход={:.prec$} выход={:.prec$} | {}с\n   \
                 брутто={:+.4} | комиссия ({})={:.6} | funding={:+.6} | НЕТТО={:+.4} | ОСТАТОК {:.5}",
                self.config.name, pos.symbol,
                match pos.side { PositionSide::Long => "ЛОНГ", PositionSide::Short => "ШОРТ" },
                qty, pos.quantity, entry, exit, dur,
                gross_pnl,
                if pos.fees_paid > 0.0 { "ФАКТ" } else { "оценка" },
                fees, funding_share, net_pnl, rest_qty,
                prec = prec,
            );

            let mut rest = pos;
            rest.quantity = rest_qty;
            rest.current_price = exit;
            rest.funding_accrued = rest_funding;
            rest.fees_paid = rest_fees;
            self.positions.insert(key, rest);

            let _ = sqlx::query(
                "UPDATE positions SET quantity=?, current_price=?, funding_accrued=?, fees_paid=? \
                 WHERE bot_name=? AND symbol=?",
            )
            .bind(rest_qty).bind(exit).bind(rest_funding).bind(rest_fees)
            .bind(&self.config.name).bind(symbol)
            .execute(&self.pool).await;

            return net_pnl;
        }

        // Полное закрытие
        let fees = if pos.fees_paid > 0.0 {
            pos.fees_paid
        } else {
            (entry + exit) * qty * self.fee_rate
        };
        let net_pnl = gross_pnl - fees + pos.funding_accrued;
        let dur = Utc::now().signed_duration_since(pos.opened_at).num_seconds();

        tracing::info!(
            "💰 ЗАКРЫТИЕ [{}] {} | {} {:.5} | вход={:.prec$} выход={:.prec$} | {}с\n   \
             брутто=({:.prec$}−{:.prec$})×{:.5}={:+.4} | комиссия ({})={:.6} | funding={:+.6}\n   \
             НЕТТО={:+.4} USDT",
            self.config.name, pos.symbol,
            match pos.side { PositionSide::Long => "ЛОНГ", PositionSide::Short => "ШОРТ" },
            qty, entry, exit, dur,
            if pos.side == PositionSide::Long { exit } else { entry },
            if pos.side == PositionSide::Long { entry } else { exit },
            qty, gross_pnl,
            if pos.fees_paid > 0.0 { "ФАКТ биржи" } else { "оценка" },
            fees, pos.funding_accrued, net_pnl,
            prec = prec,
        );

        let _ = sqlx::query("DELETE FROM positions WHERE bot_name=? AND symbol=?")
            .bind(&self.config.name).bind(symbol)
            .execute(&self.pool).await;

        net_pnl
    }

    pub async fn persist_positions(&self) {
        // ✅ P35/S8: снапшот ДО цикла. Раньше Ref из iter() жил через .await
        // (execute(&pool) в теле), держа read-lock шарда DashMap на всё
        // время записи в БД и станя писателей (check_and_update_position,
        // close_position, sync_position_qty).
        let snapshot: Vec<Position> = self.positions.iter().map(|r| r.value().clone()).collect();
        for pos in &snapshot {
            let _ = sqlx::query(
                "UPDATE positions SET stop_loss=?, extreme_price=?, funding_accrued=?, \
                 current_price=?, fees_paid=?, initial_risk=?, atr_hint=?, partial_taken=?, entry_spread=?, \
                 sl_order_id=?, tp_order_id=? \
                 WHERE bot_name=? AND symbol=?",
            )
            .bind(pos.stop_loss).bind(pos.extreme_price).bind(pos.funding_accrued)
            .bind(pos.current_price).bind(pos.fees_paid).bind(pos.initial_risk)
            .bind(pos.atr_hint)
            .bind(if pos.partial_taken { 1i64 } else { 0i64 })
            .bind(pos.entry_spread)
            .bind(&pos.sl_order_id)
            .bind(&pos.tp_order_id)
            .bind(&self.config.name).bind(&pos.symbol)
            .execute(&self.pool).await;
        }
    }

    pub async fn purge_position(&self, symbol: &str) {
        let key = format!("{}_{}", self.config.name, symbol);
        self.positions.remove(&key);
        let _ = sqlx::query("DELETE FROM positions WHERE bot_name=? AND symbol=?")
            .bind(&self.config.name).bind(symbol)
            .execute(&self.pool).await;
        tracing::warn!("[{}] 🧹 фантомная позиция {} удалена", self.config.name, symbol);
    }

    pub async fn sync_position_qty(&self, symbol: &str, qty: f64, entry_price: f64) {
        if qty <= 0.0 {
            self.purge_position(symbol).await;
            return;
        }
        let key = format!("{}_{}", self.config.name, symbol);
        if let Some(mut pos) = self.positions.get_mut(&key) {
            pos.quantity = qty;
            pos.entry_price = entry_price;
        }
        let _ = sqlx::query(
            "UPDATE positions SET quantity=?, entry_price=? WHERE bot_name=? AND symbol=?",
        )
        .bind(qty).bind(entry_price)
        .bind(&self.config.name).bind(symbol)
        .execute(&self.pool).await;
    }

        /// 🆕 DCA: add to existing position, recalculate avg entry
    pub async fn add_dca_order(
        &self,
        symbol: &str,
        side: &str,
        entry_price: f64,
        quantity: f64,
        stop_loss: Option<f64>,
        take_profit: Option<f64>,
        origin_strategy: String,
        initial_risk: f64,
        atr_hint: f64,
        entry_spread: f64,
    ) {
        let key = format!("{}_{}", self.config.name, symbol);

        // Проверяем — есть ли уже позиция?
        let exists = self.positions.contains_key(&key);

        if exists {
            // Позиция есть — обновляем avg entry
            let mut new_qty = 0.0;
            let mut new_avg = 0.0;
            let mut new_initial_risk: f64 = 0.0;
            if let Some(mut pos) = self.positions.get_mut(&key) {
                let old_qty = pos.quantity;
                let old_entry = pos.entry_price;
                // ✅ P41/S12: берём актуальный initial_risk (уже обновлён выше),
                // для persist при initial_risk<=0 оставляем прежний.
                new_initial_risk = if initial_risk > 0.0 { initial_risk } else { pos.initial_risk };
                new_qty = old_qty + quantity;
                new_avg = if new_qty > 0.0 {
                    (old_entry * old_qty + entry_price * quantity) / new_qty
                } else {
                    entry_price
                };
                pos.quantity = new_qty;
                pos.entry_price = new_avg;
                // ✅ P41/S12: DCA-ордер может нести свежие SL/TP — применяем.
                // Ослабление подтянутого SL ГАСИМ с WARN (Long: новый SL ниже
                // старого, Short: выше = риск больше принятого). Лестница
                // keltner_dca ре-анкорит SL по Donchian легитимно — такие сдвиги
                // становятся видимыми в логе, поведение не меняется.
                if stop_loss.is_some() {
                    let weakened = match (pos.side, pos.stop_loss, stop_loss) {
                        (PositionSide::Long, Some(cur), Some(new)) => new < cur,
                        (PositionSide::Short, Some(cur), Some(new)) => new > cur,
                        _ => false,
                    };
                    if weakened {
                        tracing::warn!(
                            "[{}] ⚠️ DCA: SL {:.prec$} ослабил бы подтянутый {:.prec$} — оставляю подтянутый",
                            self.config.name,
                            stop_loss.unwrap_or(0.0),
                            pos.stop_loss.unwrap_or(0.0),
                            prec = price_precision(symbol)
                        );
                    } else {
                        pos.stop_loss = stop_loss;
                    }
                }
                if take_profit.is_some() {
                    pos.take_profit = take_profit;
                }
                // ✅ P41/S12: initial_risk устаревал (риск ПЕРВОЙ транши) —
                // прогресс R и peak_r позиций лестницы считались от неверной базы.
                if initial_risk > 0.0 {
                    pos.initial_risk = initial_risk;
                }
                tracing::info!(
                    "[{}] 📈 DCA add: {} +{:.6} @ {:.4} → avg={:.4} total={:.6}",
                    self.config.name, symbol, quantity, entry_price, new_avg, new_qty,
                );
            }
            // ✅ FIX: усреднение персистится в БД — раньше после рестарта
            // читались старые qty/entry (съезжала база PnL, reconciliation
            // видел ложный дрейф и «чинил» позицию по бирже).
            if new_qty > 0.0 {
                let _ = sqlx::query(
                    "UPDATE positions SET quantity=?, entry_price=?, \
                     stop_loss=COALESCE(?, stop_loss), take_profit=COALESCE(?, take_profit), \
                     initial_risk=? \
                     WHERE bot_name=? AND symbol=?",
                )
                .bind(new_qty)
                .bind(new_avg)
                .bind(stop_loss)
                .bind(take_profit)
                .bind(new_initial_risk)
                .bind(&self.config.name)
                .bind(symbol)
                .execute(&self.pool)
                .await;
            }
        } else {
            // Нет позиции — создаём новую (как обычно)
            self.add_position_with_risk(
                symbol.to_string(), side.to_string(), entry_price, quantity,
                stop_loss, take_profit, origin_strategy, initial_risk,
                atr_hint, entry_spread,
            ).await;
            tracing::info!(
                "[{}] 📈 DCA first order: {} {:.6} @ {:.4}",
                self.config.name, symbol, quantity, entry_price,
            );
        }
    }

    pub fn get_positions(&self) -> Vec<Position> {
        self.positions.iter().map(|e| e.value().clone()).collect()
    }

    /// 🔧 ЭТАП B: обновить sl_order_id (совместимость со старым API)
    pub fn update_sl_order_id(&self, symbol: &str, id: String) {
        let key = format!("{}_{}", self.config.name, symbol);
        if let Some(mut pos) = self.positions.get_mut(&key) {
            pos.sl_order_id = Some(id);
        }
    }

    pub fn log_state(&self) {
        let positions: Vec<_> = self.positions.iter().map(|e| e.value().clone()).collect();
        if positions.is_empty() {
            return;
        }

        let mut total_upnl = 0.0;
        for p in &positions {
            total_upnl += p.unrealized_pnl;
            let prec = price_precision(&p.symbol);
            let sl_str = p.stop_loss.map(|v| format!("{:.prec$}", v, prec = prec)).unwrap_or("—".into());
            let tp_str = p.take_profit.map(|v| format!("{:.prec$}", v, prec = prec)).unwrap_or("—".into());
            let dir = match p.side { PositionSide::Long => "ЛОНГ", PositionSide::Short => "ШОРТ" };
            let move_pct = match p.side {
                PositionSide::Long => (p.current_price - p.entry_price) / p.entry_price * 100.0,
                PositionSide::Short => (p.entry_price - p.current_price) / p.entry_price * 100.0,
            };
            let age_sec = (Utc::now() - p.opened_at).num_seconds();
            let r_now = if p.initial_risk > 0.0 {
                let profit = match p.side {
                    PositionSide::Long => p.current_price - p.entry_price,
                    PositionSide::Short => p.entry_price - p.current_price,
                };
                profit / p.initial_risk
            } else { 0.0 };
            let r_str = if p.initial_risk > 0.0 {
                format!(" | R={:+.2}{}", r_now, if p.partial_taken { " (part✓)" } else { "" })
            } else { String::new() };
            // 🔧 ЭТАП B: статус защит в строке позиции
            let guard = format!(
                " | 🛡{}{}",
                if p.sl_order_id.is_some() { "SL✓" } else { "SL✗" },
                if p.tp_order_id.is_some() { "TP✓" } else { "TP✗" }
            );

            match prec {
                8 => tracing::info!(
                    "💼 [{}] {} {} | объём={:.0} | вход={:.8} | тек={:.8} | Δ={:+.3}% | {}с | SL={} | TP={} | PnL={:+.4} | funding={:+.4} | fees={:.4}{}{}",
                    self.config.name, dir, p.symbol, p.quantity, p.entry_price, p.current_price,
                    move_pct, age_sec, sl_str, tp_str, p.unrealized_pnl, p.funding_accrued,
                    p.fees_paid, r_str, guard,
                ),
                _ => tracing::info!(
                    "💼 [{}] {} {} | объём={:.5} | вход={:.2} | тек={:.2} | Δ={:+.3}% | {}с | SL={} | TP={} | PnL={:+.4} | funding={:+.4} | fees={:.4}{}{}",
                    self.config.name, dir, p.symbol, p.quantity, p.entry_price, p.current_price,
                    move_pct, age_sec, sl_str, tp_str, p.unrealized_pnl, p.funding_accrued,
                    p.fees_paid, r_str, guard,
                ),
            }
        }

        tracing::info!(
            "💰 [{}] открыто={} | плавающий PnL={:+.4} USDT",
            self.config.name, positions.len(), total_upnl
        );
    }
}