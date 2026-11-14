pub mod circuit_breaker;
pub mod global_risk_manager;

use crate::config::{BotConfig, GlobalConfig};
use crate::strategies::Signal;
use anyhow::Result;
use chrono::Utc;
use sqlx::sqlite::SqlitePool;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::sync::{Arc, RwLock};

pub use circuit_breaker::CircuitBreaker;
pub use global_risk_manager::GlobalRiskManager;

/// ✅ bk9: минимум сделок, после которого проверяем EV.
const MIN_TRADES_FOR_EV_CHECK: u32 = 30;

/// ✅ bk9: порог EV (в USDT на сделку), ниже которого стратегия признаётся убыточной.
const MIN_EV_USDT: f64 = 0.0;

/// 🔧 PROP-GUARD: лимит просадки эквити от пика (доля). Дефолт 0.20
/// для тестнет-эксперимента. Проп-норма для реального счёта: 0.06–0.10 —
/// выставить через .env PROP_MAX_DD_PCT при переходе на боевой режим.
const PROP_DD_DEFAULT: f64 = 0.20;

// ============================================================
// 🔧 ЭТАП C: режимы выживания (GROWTH / SURVIVAL / HALT)
// ============================================================
/// Окно скользящего EV для определения режима.
const MODE_EV_WINDOW: usize = 20;
/// Порог EV (USDT/сделку), выше которого режим GROWTH (полный риск).
/// EV20 ≥ +0.02 = эдж подтверждён на скользящем окне.
const MODE_EV_GROWTH: f64 = 0.02;
/// Просадка от пика, при которой GROWTH принудительно деградирует
/// в SURVIVAL (мягкий занос перед PROP-GUARD-казнью на 20%).
const MODE_DD_SOFT: f64 = 0.08;

pub struct RiskManager {
    config: BotConfig,
    global: GlobalConfig,
    daily_loss: f64,
    circuit_breaker: CircuitBreaker,
    global_risk: Arc<GlobalRiskManager>,

    realized_pnl: Decimal,
    total_trades: u32,
    win_trades: u32,
    loss_trades: u32,
    gross_profit: Decimal,
    gross_loss: Decimal,

    suspended: bool,
    suspend_reason: Option<String>,
    pool: SqlitePool,
    stat_day: String,

    // --- 🔧 PROP-GUARD: пик эквити и лимит просадки ---
    equity_peak: f64,
    prop_dd_pct: f64,

    // P16: live equity mirror (initial + realized PnL), read by strategies
    live_equity: Arc<RwLock<f64>>,

    // --- 🔧 ЭТАП C: режимы выживания ---
    /// Скользящее окно последних net_pnl (для EV20)
    recent_ev: std::collections::VecDeque<f64>,
    /// Множитель риска: 1.0 = GROWTH, 0.5 = SURVIVAL, 0.0 = HALT
    risk_multiplier: f64,
}

impl RiskManager {
    pub async fn new(
        config: &BotConfig,
        global: &GlobalConfig,
        global_risk: Arc<GlobalRiskManager>,
    ) -> Result<Self> {
        let db_path = config
            .db_path_override
            .as_ref()
            .unwrap_or(&global.db_path);
        let pool = SqlitePool::connect(&format!("sqlite:{}?mode=rwc", db_path)).await?;
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS risk_stats (
                bot_name TEXT PRIMARY KEY,
                realized_pnl TEXT NOT NULL DEFAULT '0',
                total_trades INTEGER NOT NULL DEFAULT 0,
                win_trades INTEGER NOT NULL DEFAULT 0,
                loss_trades INTEGER NOT NULL DEFAULT 0,
                gross_profit TEXT NOT NULL DEFAULT '0',
                gross_loss TEXT NOT NULL DEFAULT '0',
                daily_loss TEXT NOT NULL DEFAULT '0',
                stat_day TEXT NOT NULL DEFAULT '',
                suspended INTEGER NOT NULL DEFAULT 0,
                suspend_reason TEXT NOT NULL DEFAULT ''
            )"#,
        )
        .execute(&pool)
        .await?;

        let _ = sqlx::query(
            "ALTER TABLE risk_stats ADD COLUMN cb_remaining_ms INTEGER NOT NULL DEFAULT 0",
        )
        .execute(&pool)
        .await;

        // 🔧 PROP-GUARD: пик эквити персистится — рестарт не сбрасывает
        let _ = sqlx::query(
            "ALTER TABLE risk_stats ADD COLUMN equity_peak REAL NOT NULL DEFAULT 0",
        )
        .execute(&pool)
        .await;

        let today = Utc::now().format("%Y-%m-%d").to_string();

        // ============================================================
        // 🔧 RESET_STATS: чистый старт эксперимента (.env, один раз).
        // ============================================================
        let mut force_reset = false;
        if std::env::var("RESET_STATS")
            .map(|v| v.trim() == "1")
            .unwrap_or(false)
        {
            let _ = sqlx::query("DELETE FROM risk_stats WHERE bot_name = ?")
                .bind(&config.name)
                .execute(&pool)
                .await;
            force_reset = true;
            tracing::warn!(
                "[risk:{}] 🧹 RESET_STATS=1: статистика удалена — чистый старт гипотезы",
                config.name
            );
        }

        let mut realized_pnl = Decimal::ZERO;
        let (mut total_trades, mut win_trades, mut loss_trades) = (0u32, 0u32, 0u32);
        let (mut gross_profit, mut gross_loss) = (Decimal::ZERO, Decimal::ZERO);
        let mut daily_loss = 0.0f64;
        let mut suspended = false;
        let mut suspend_reason: Option<String> = None;
        let mut cb_remaining_ms: i64 = 0;
        let mut equity_peak = 0.0f64;

        if !force_reset {
            if let Ok(row) = sqlx::query(
                "SELECT realized_pnl, total_trades, win_trades, loss_trades, gross_profit, \
                 gross_loss, daily_loss, stat_day, suspended, suspend_reason, cb_remaining_ms, \
                 equity_peak FROM risk_stats WHERE bot_name = ?",
            )
            .bind(&config.name)
            .fetch_one(&pool)
            .await
            {
                use sqlx::Row;
                realized_pnl = row.try_get::<String, _>("realized_pnl").ok()
                    .and_then(|s| s.parse::<Decimal>().ok()).unwrap_or(Decimal::ZERO);
                total_trades = row.try_get::<u32, _>("total_trades").unwrap_or(0);
                win_trades = row.try_get::<u32, _>("win_trades").unwrap_or(0);
                loss_trades = row.try_get::<u32, _>("loss_trades").unwrap_or(0);
                gross_profit = row.try_get::<String, _>("gross_profit").ok()
                    .and_then(|s| s.parse::<Decimal>().ok()).unwrap_or(Decimal::ZERO);
                gross_loss = row.try_get::<String, _>("gross_loss").ok()
                    .and_then(|s| s.parse::<Decimal>().ok()).unwrap_or(Decimal::ZERO);
                suspended = row.try_get::<i64, _>("suspended").unwrap_or(0) != 0;
                let reason = row.try_get::<String, _>("suspend_reason").unwrap_or_default();
                if suspended && !reason.is_empty() {
                    suspend_reason = Some(reason);
                }
                let saved_day: String = row.try_get::<String, _>("stat_day").unwrap_or_default();
                if saved_day == today {
                    daily_loss = row.try_get::<String, _>("daily_loss").ok()
                        .and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                }
                cb_remaining_ms = row.try_get::<i64, _>("cb_remaining_ms").unwrap_or(0);
                equity_peak = row.try_get::<f64, _>("equity_peak").unwrap_or(0.0);

                tracing::info!(
                    "[risk:{}] 📊 статистика восстановлена из БД: trades={} | WR={:.1}% | pnl={} | daily_loss={:.4}{}",
                    config.name, total_trades,
                    if total_trades > 0 { win_trades as f64 / total_trades as f64 * 100.0 } else { 0.0 },
                    realized_pnl, daily_loss,
                    if suspended { " | SUSPENDED" } else { "" }
                );
            }
        }

        // ============================================================
        // 🔧 PROP-GUARD: лимит просадки из .env PROP_MAX_DD_PCT (0.01–0.50)
        // ============================================================
        let mut prop_dd_pct = PROP_DD_DEFAULT;
        if let Ok(v) = std::env::var("PROP_MAX_DD_PCT") {
            if let Ok(x) = v.trim().parse::<f64>() {
                if (0.01..=0.50).contains(&x) {
                    prop_dd_pct = x;
                    tracing::info!(
                        "[risk:{}] 🛡 PROP_MAX_DD_PCT из .env = {:.2} (дефолт {:.2})",
                        config.name, x, PROP_DD_DEFAULT
                    );
                }
            }
        }

        // Пик эквити: из БД; 0 (после RESET или первая колонка) = текущее эквити
        let initial_balance = config.initial_balance.to_f64().unwrap_or(1000.0);
        let current_equity = initial_balance + realized_pnl.to_f64().unwrap_or(0.0);
        let live_equity = Arc::new(RwLock::new(current_equity)); // P16
        if equity_peak <= 0.0 {
            equity_peak = current_equity;
        }
        if equity_peak < current_equity {
            equity_peak = current_equity;
        }

        // 🔧 PROP-GUARD: проверка ПОСЛЕ восстановления — детерминированный старт
        let mut dd_at_start = 0.0f64;
        if equity_peak > 0.0 {
            dd_at_start = (equity_peak - current_equity) / equity_peak;
            if dd_at_start < 0.0 {
                dd_at_start = 0.0;
            }
        }
        if dd_at_start >= prop_dd_pct && !suspended {
            suspended = true;
            suspend_reason = Some(format!(
                "PROP-GUARD при старте: просадка {:.1}% от пика {:.2} (лимит {:.1}%)",
                dd_at_start * 100.0, equity_peak, prop_dd_pct * 100.0
            ));
            tracing::error!("🚨 PROP-GUARD: {}", suspend_reason.as_deref().unwrap_or(""));
        }

        // ✅ P41/D15: порог срабатывания из конфига (был захардкожен 3;
        // окно/кулдаун уже были настраиваемыми).
        let circuit_breaker = CircuitBreaker::new(
            config.circuit_breaker_threshold as usize, // P41: u32 -> usize (E0308)
            config.circuit_breaker_window_sec,
            config.circuit_breaker_cooldown_sec,
        );
        if cb_remaining_ms > 0 {
            circuit_breaker.restore_open_with_remaining(cb_remaining_ms as u64);
            tracing::warn!(
                "[risk:{}] 🛡 Circuit Breaker восстановлен из БД — кулдаун ещё {} мс",
                config.name, cb_remaining_ms
            );
        }

        tracing::info!(
            "[risk:{}] 🛡 PROP-GUARD: пик эквити={:.2} | текущее={:.2} | DD={:.1}% | лимит={:.1}%",
            config.name, equity_peak, current_equity,
            dd_at_start * 100.0, prop_dd_pct * 100.0
        );

        // 🔧 ЭТАП C: старт всегда в SURVIVAL — докажи эдж, получи полный риск
        tracing::info!(
            "[risk:{}] 🧭 режим выживания: старт в SURVIVAL (множитель риска 0.5) — \
             GROWTH после EV20 ≥ +{:.2} на {} сделках",
            config.name, MODE_EV_GROWTH, MODE_EV_WINDOW
        );

        Ok(Self {
            config: config.clone(),
            global: global.clone(),
            daily_loss,
            circuit_breaker,
            global_risk,
            realized_pnl,
            total_trades,
            win_trades,
            loss_trades,
            gross_profit,
            gross_loss,
            suspended,
            suspend_reason,
            pool,
            stat_day: today,
            // 🔧 PROP-GUARD
            equity_peak,
            prop_dd_pct,
            live_equity,
            // 🔧 ЭТАП C
            recent_ev: std::collections::VecDeque::with_capacity(MODE_EV_WINDOW + 1),
            risk_multiplier: 0.5,
        })
    }

    // ============================================================
    // Проверки перед входом
    // ============================================================
    pub fn check(&mut self, signal: &Signal) -> bool { // AUDIT-P4: &mut для rollover дня
        if signal.strategy_name.starts_with("Exit_") {
            return true;
        }

        // [P11:kill] Kill-switch: KILL file in project root = exits-only mode.
        if crate::utils::killfile::kill_switch_active() {
            tracing::warn!("KILL file active - entry rejected (exits-only mode)");
            return false;
        }

        if self.suspended {
            tracing::warn!(
                "⛔ Бот в SUSPENDED: {}",
                self.suspend_reason.as_deref().unwrap_or("EV ≤ 0")
            );
            return false;
        }

        if self.circuit_breaker.is_open() {
            tracing::warn!("Circuit breaker активен — вход заблокирован");
            return false;
        }

        // AUDIT-P4: rollover дня ЗДЕСЬ (а не только в record_close).
        // Иначе deadlock: дневной стоп -> входов нет -> сделок нет ->
        // record_close не зовётся -> daily_loss никогда не сбросится.
        let today = Utc::now().format("%Y-%m-%d").to_string();
        if self.stat_day != today {
            tracing::info!(
                "[risk:{}] 📅 новый день — дневной убыток сброшен (было {:.4} USDT)",
                self.config.name, self.daily_loss
            );
            self.stat_day = today;
            self.daily_loss = 0.0;
        }
        let max_loss = self.config.max_daily_loss_amount.to_f64().unwrap_or(30.0);
        if self.daily_loss >= max_loss {
            tracing::warn!(
                "Дневной лимит убытка достигнут: {:.4} >= {:.4} USDT",
                self.daily_loss, max_loss
            );
            return false;
        }

        if self.global_risk.active_positions_count() >= self.global.max_concurrent_positions {
            tracing::warn!(
                "Превышен лимит одновременных позиций: {} >= {}",
                self.global_risk.active_positions_count(),
                self.global.max_concurrent_positions
            );
            return false;
        }

        // AUDIT-P3: без SL сигнал раньше имел риск 0 (unwrap_or(price)) и
        // обходил лимит совокупного риска. Нет SL = 2% adverse move.
        let stop = signal.stop_loss.unwrap_or(signal.price * 0.98);
        let new_risk = signal.quantity * (signal.price - stop).abs();

        let initial = self.config.initial_balance.to_f64().unwrap_or(1000.0);
        let realized = self.realized_pnl.to_f64().unwrap_or(0.0);
        let balance = initial + realized;
        // AUDIT-P5: база совокупного риска — floor по equity_peak. Иначе
        // после серии убытков лимит схлопывается, а сайзинг идёт от
        // initial_balance: гейт молча блокирует все новые входы при
        // открытой позиции (deadlock №2, бот не suspended). DD-режим —
        // работа PROP-GUARD и SURVIVAL-множителя, не схлопывание квоты.
        let risk_base = balance.max(self.equity_peak);
        let max_total_risk = risk_base
            * self.config.max_concurrent_risk_pct.to_f64().unwrap_or(2.0)
            / 100.0;
        let current_risk = self.global_risk.total_risk();

        if current_risk + new_risk > max_total_risk {
            tracing::warn!(
                "⛔ Суммарный риск превышен: balance={:.4} (init={:.4}+realized={:.4}), \
                 лимит={:.4}, тек.риск={:.4} + новый={:.4} = {:.4}",
                balance, initial, realized,
                max_total_risk, current_risk, new_risk,
                current_risk + new_risk
            );
            return false;
        }

        true
    }

    // ============================================================
    // Учёт закрытой сделки — ЕДИНЫЙ метод (bk2)
    // ============================================================
    pub fn record_close(&mut self, net_pnl: Decimal) {
        let today = Utc::now().format("%Y-%m-%d").to_string();
        if self.stat_day != today {
            self.stat_day = today;
            self.daily_loss = 0.0;
            tracing::info!("[risk:{}] 📅 новый день — дневной убыток сброшен", self.config.name);
        }
        self.realized_pnl += net_pnl;

        if net_pnl > Decimal::ZERO {
            self.total_trades += 1;
            self.win_trades += 1;
            self.gross_profit += net_pnl;
        } else if net_pnl < Decimal::ZERO {
            self.total_trades += 1;
            self.loss_trades += 1;
            self.gross_loss += net_pnl.abs();

            let loss_f64 = net_pnl.abs().to_f64().unwrap_or(0.0);
            self.daily_loss += loss_f64;
            // ✅ P41/D15: порог «значимого лосса» из конфига (был 0.10 USDT
            // захардкожен).
            let cb_loss = self
                .config
                .circuit_breaker_loss_usdt
                .to_f64()
                .unwrap_or(0.10);
            if loss_f64 >= cb_loss {
                // ✅ [P62/8] алерт на ПЕРЕХОД breaker в open (раньше trip был
                // только в логе). Переход ловим вокруг record_loss: was_open
                // до, is_open после. Продления cooldown внутри открытого
                // состояния алерт НЕ создают.
                let was_open = self.circuit_breaker.is_open();
                self.circuit_breaker.record_loss();
                let now_open = self.circuit_breaker.is_open();
                if !was_open && now_open {
                    let cd = self
                        .circuit_breaker
                        .cooldown_remaining()
                        .map(|d| format!("{:?}", d))
                        .unwrap_or_else(|| "n/a".to_string());
                    crate::utils::telegram::notify_spawn(
                        "CIRCUIT BREAKER TRIPPED",
                        &format!(
                            "{}: {} значимых убытков в окне, входы заблокированы на {}",
                            self.config.name,
                            self.circuit_breaker.losses_count(),
                            cd
                        ),
                    );
                }
            }
            // ✅ [P62/8] пороги дневного убытка бота: 50/80/100% лимита.
            // Ровно один алерт на (бот, день, порог) — notify_once_spawn,
            // ключ включает stat_day (новый день = новый инцидент).
            let max_loss = self.config.max_daily_loss_amount.to_f64().unwrap_or(30.0);
            if let Some(level) = crate::utils::telegram::day_loss_level(self.daily_loss, max_loss) {
                let key = format!("dayloss/{}/{}/{}", self.config.name, self.stat_day, level);
                crate::utils::telegram::notify_once_spawn(
                    &key,
                    "DAILY LOSS THRESHOLD",
                    &format!(
                        "{}: -{:.2} из -{:.2} USDT ({}% дневного лимита)",
                        self.config.name, self.daily_loss, max_loss, level
                    ),
                );
            }
        }

        // ============================================================
        // 🔧 PROP-GUARD: пик эквити монотонно вверх; просадка от пика
        // ≥ лимита → SUSPENDED.
        // ============================================================
        let initial = self.config.initial_balance.to_f64().unwrap_or(1000.0);
        let equity = initial + self.realized_pnl.to_f64().unwrap_or(0.0);
        if let Ok(mut w) = self.live_equity.write() { *w = equity; } // P16
        if equity > self.equity_peak {
            self.equity_peak = equity;
        }
        if !self.suspended && self.equity_peak > 0.0 {
            let dd = (self.equity_peak - equity) / self.equity_peak;
            if dd >= self.prop_dd_pct {
                self.suspended = true;
                self.suspend_reason = Some(format!(
                    "PROP-GUARD: просадка {:.1}% от пика эквити {:.2} (лимит {:.1}%)",
                    dd * 100.0, self.equity_peak, self.prop_dd_pct * 100.0
                ));
                tracing::error!("🚨 АВТО-СТОП (PROP-GUARD): {}", self.suspend_reason.as_deref().unwrap_or(""));
            }
        }

        // ============================================================
        // 🔧 ЭТАП C: режимы выживания — скользящий EV и множитель риска.
        // Ядро: риск НИКОГДА не растёт при падающем EV.
        //   GROWTH   (1.0): EV20 ≥ +0.02 И DD < 8%
        //   SURVIVAL (0.5): всё остальное, что не suspended
        //   HALT     (0.0): suspended
        // ============================================================
        self.recent_ev.push_back(net_pnl.to_f64().unwrap_or(0.0));
        while self.recent_ev.len() > MODE_EV_WINDOW {
            self.recent_ev.pop_front();
        }

        let equity_now = initial + self.realized_pnl.to_f64().unwrap_or(0.0);
        let dd_now = if self.equity_peak > 0.0 {
            ((self.equity_peak - equity_now) / self.equity_peak).max(0.0)
        } else {
            0.0
        };
        let ev20 = if self.recent_ev.is_empty() {
            0.0
        } else {
            self.recent_ev.iter().sum::<f64>() / self.recent_ev.len() as f64
        };

        let old_mult = self.risk_multiplier;
        self.risk_multiplier = if self.suspended {
            0.0 // HALT
        } else if dd_now >= MODE_DD_SOFT || ev20 < 0.0 {
            0.5 // SURVIVAL
        } else if ev20 >= MODE_EV_GROWTH {
            1.0 // GROWTH
        } else {
            0.5 // нейтральная зона — SURVIVAL
        };
        if (self.risk_multiplier - old_mult).abs() > 0.01 {
            tracing::warn!(
                "[risk:{}] 🧭 режим: множитель риска {:.2} → {:.2} | EV20={:+.4} | DD={:.1}%",
                self.config.name, old_mult, self.risk_multiplier,
                ev20, dd_now * 100.0
            );
        }

        if self.total_trades > 0 && self.total_trades % 10 == 0 {
            let wr = self.win_rate();
            let pf = self.profit_factor();
            let ev = self
                .expected_value()
                .and_then(|d| d.to_f64())
                .unwrap_or(0.0);

            tracing::info!(
                "📊 RISK STATS | trades={} | WR={:.1}% | PF={:.2} | EV={:+.4} | realized_pnl={} | peak={:.2} | mult={:.2}",
                self.total_trades,
                wr * 100.0,
                pf,
                ev,
                self.realized_pnl,
                self.equity_peak,
                self.risk_multiplier
            );
        }

        if !self.suspended && self.total_trades >= MIN_TRADES_FOR_EV_CHECK {
            if let Some(ev_dec) = self.expected_value() {
                let ev = ev_dec.to_f64().unwrap_or(0.0);
                if ev <= MIN_EV_USDT {
                    self.suspended = true;
                    self.suspend_reason = Some(format!(
                        "EV={:+.4} USDT после {} сделок — стратегия убыточна",
                        ev, self.total_trades
                    ));
                    tracing::error!(
                        "🚨 АВТО-СТОП: {} | WR={:.1}% | PF={:.2} | realized_pnl={}",
                        self.suspend_reason.as_deref().unwrap_or(""),
                        self.win_rate() * 100.0,
                        self.profit_factor(),
                        self.realized_pnl
                    );
                }
            }
        }
    }

    // P16: live equity accessors
    pub fn equity_handle(&self) -> Arc<RwLock<f64>> {
        self.live_equity.clone()
    }

    pub fn live_equity(&self) -> f64 {
        self.live_equity.read().map(|g| *g).unwrap_or(0.0)
    }

    pub async fn flush_stats(&mut self) {
        if let Ok(row) = sqlx::query(
            "SELECT id FROM bot_commands WHERE consumed = 0 AND command = 'resume' \
             ORDER BY id LIMIT 1",
        )
        .fetch_one(&self.pool)
        .await
        {
            use sqlx::Row;
            let id: i64 = row.try_get("id").unwrap_or(0);
            let _ = sqlx::query("UPDATE bot_commands SET consumed = 1 WHERE id = ?")
                .bind(id)
                .execute(&self.pool)
                .await;
            if self.suspended {
                self.resume();
            } else {
                tracing::info!(
                    "[risk:{}] 📩 команда resume получена — бот не в suspended (no-op)",
                    self.config.name
                );
            }
        }

        let _ = sqlx::query(
            "INSERT INTO risk_stats (bot_name, realized_pnl, total_trades, win_trades, loss_trades, \
             gross_profit, gross_loss, daily_loss, stat_day, suspended, suspend_reason, cb_remaining_ms, \
             equity_peak) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(bot_name) DO UPDATE SET realized_pnl=excluded.realized_pnl, \
             total_trades=excluded.total_trades, win_trades=excluded.win_trades, \
             loss_trades=excluded.loss_trades, gross_profit=excluded.gross_profit, \
             gross_loss=excluded.gross_loss, daily_loss=excluded.daily_loss, \
             stat_day=excluded.stat_day, suspended=excluded.suspended, \
             suspend_reason=excluded.suspend_reason, cb_remaining_ms=excluded.cb_remaining_ms, \
             equity_peak=excluded.equity_peak",
        )
        .bind(&self.config.name)
        .bind(self.realized_pnl.to_string())
        .bind(self.total_trades)
        .bind(self.win_trades)
        .bind(self.loss_trades)
        .bind(self.gross_profit.to_string())
        .bind(self.gross_loss.to_string())
        .bind(self.daily_loss.to_string())
        .bind(&self.stat_day)
        .bind(if self.suspended { 1i64 } else { 0i64 })
        .bind(self.suspend_reason.clone().unwrap_or_default())
        .bind(self.circuit_breaker.cooldown_remaining_ms() as i64)
        .bind(self.equity_peak)
        .execute(&self.pool)
        .await;
    }

    pub fn record_trade_result(&mut self, _symbol: &str, pnl: f64) {
        let dec = Decimal::try_from(pnl).unwrap_or(Decimal::ZERO);
        self.record_close(dec);
    }

    pub fn win_rate(&self) -> f64 {
        if self.total_trades == 0 {
            return 0.0;
        }
        self.win_trades as f64 / self.total_trades as f64
    }

    pub fn profit_factor(&self) -> f64 {
        if self.gross_loss.is_zero() {
            return if self.gross_profit.is_zero() {
                0.0
            } else {
                f64::INFINITY
            };
        }
        (self.gross_profit / self.gross_loss)
            .to_f64()
            .unwrap_or(0.0)
    }

    pub fn expected_value(&self) -> Option<Decimal> {
        if self.total_trades == 0 {
            return None;
        }
        let total = Decimal::from(self.total_trades);
        let wins = Decimal::from(self.win_trades);
        let losses = Decimal::from(self.loss_trades);
        let win_rate = wins / total;

        let avg_win = if self.win_trades > 0 {
            self.gross_profit / wins
        } else {
            Decimal::ZERO
        };
        let avg_loss = if self.loss_trades > 0 {
            self.gross_loss / losses
        } else {
            Decimal::ZERO
        };

        Some(win_rate * avg_win - (Decimal::ONE - win_rate) * avg_loss)
    }

    // AUDIT-P4: раньше метод был мёртв (никто не звал) и заодно обнулял
    // daily_loss — ручной resume обходил бы дневной стоп одним кликом.
    // Теперь: сбрасывает ТОЛЬКО circuit breaker (panic-stop, human override).
    // Дневной бюджет живёт до rollover нового дня. Вызывается из resume().
    pub fn reset_circuit_breaker(&mut self) {
        self.circuit_breaker.reset();
    }

    // AUDIT-P4: ручной resume = human-override: снимает EV-suspend и
    // кулдаун circuit breaker. daily_loss НЕ трогаем (действует до полуночи).
    pub fn resume(&mut self) {
        self.reset_circuit_breaker();
        if self.suspended {
            tracing::warn!(
                "🟢 SUSPENDED сброшен вручную (ранее: {})",
                self.suspend_reason.as_deref().unwrap_or("")
            );
            self.suspended = false;
            self.suspend_reason = None;
        }
    }

    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    pub fn suspend_reason(&self) -> Option<&str> {
        self.suspend_reason.as_deref()
    }

    // ============================================================
    // 🔧 ЭТАП C: геттер множителя риска (для провода в стратегию)
    // ============================================================
    pub fn risk_multiplier(&self) -> f64 {
        self.risk_multiplier
    }

    pub fn realized_pnl(&self) -> Decimal {
        self.realized_pnl
    }
    pub fn total_trades(&self) -> u32 {
        self.total_trades
    }
    pub fn daily_loss(&self) -> f64 {
        self.daily_loss
    }
}