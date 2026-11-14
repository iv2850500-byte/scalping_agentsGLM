use dashmap::DashMap;
use std::sync::Arc;

/// ✅ 4б (К-1): ОДИН экземпляр на весь процесс (создаётся в main).
/// Ключ — «БОТ/СИМВОЛ»: два бота на одном символе не затирают записи друг друга.
pub struct GlobalRiskManager {
    /// «БОТ/СИМВОЛ» → риск в USDT
    positions: Arc<DashMap<String, f64>>,
}

impl Default for GlobalRiskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalRiskManager {
    pub fn new() -> Self {
        Self {
            positions: Arc::new(DashMap::new()),
        }
    }

    fn key(bot: &str, symbol: &str) -> String {
        format!("{}/{}", bot.to_uppercase(), symbol.to_uppercase())
    }

    /// Регистрируем позицию от имени конкретного бота
    pub fn register_position(&self, bot: &str, symbol: &str, risk_usdt: f64) {
        self.positions.insert(Self::key(bot, symbol), risk_usdt.max(0.0));
    }

    pub fn unregister_position(&self, bot: &str, symbol: &str) {
        self.positions.remove(&Self::key(bot, symbol));
    }

    /// Всего открытых позиций ПО ВСЕМ ботам
    pub fn active_positions_count(&self) -> usize {
        self.positions.len()
    }

    /// Суммарный риск ПО ВСЕМ ботам, USDT
    pub fn total_risk(&self) -> f64 {
        self.positions.iter().map(|e| *e.value()).sum()
    }
}
// ==== PATCH P15: portfolio daily stop + KILL file (appended by patch_p15d) ====
// State lives in a process-wide static: no changes to the original struct.
// KILL file: create an empty file named KILL in the project root to block entries.
// ==== P54/HALT-2: портфельный дневной стоп = ЖЁСТКИЙ HALT ====
// Раньше стоп был «мягким» фактически: (1) база = сумма initial_balance ботов
// (не account_capital_usdt из K6) — лимит 5% считался от неправильной базы и
// никогда не срабатывал; (2) срыв персиста daily PnL молча обезоруживал стоп
// после рестарта (fail-open); (3) движок входа не проверял halt (только main
// loop). Теперь: база задаётся из config (P54/HALT-1 в main.rs), персист-сбой
// = fail-closed, переход через лимит = один Telegram-алерт, движок тоже гейтится.
#[derive(Default)]
struct P15PortfolioState {
    equity_base: f64,
    realized_today: f64,
    day_no: u64,
    stop_pct_cfg: f64,
    /// ✅ P54/HALT-2: алерт о переходе лимита отправлен ОДИН раз на инцидент.
    halted_notified: bool,
    /// ✅ P54/HALT-3 (C8-хвост): запись daily PnL на диск не удалась —
    /// рестарт обнулит счётчик убытков и ОБЕЗОРУЖИТ стоп. Пока персист
    /// сломан — fail-closed: входы запрещены (KILL-семантика).
    persistence_broken: bool,
}

static P15_PORTFOLIO: std::sync::Mutex<Option<P15PortfolioState>> = std::sync::Mutex::new(None);

impl GlobalRiskManager {
    fn p15_day_no() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() / 86400)
            .unwrap_or(0)
    }

    fn p15_lock() -> std::sync::MutexGuard<'static, Option<P15PortfolioState>> {
        match P15_PORTFOLIO.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn p15_day_file() -> &'static str {
        "data/p15_portfolio_day.txt"
    }

    pub fn set_equity_base(&self, equity: f64) {
        let mut g = Self::p15_lock();
        let s = g.get_or_insert_with(P15PortfolioState::default);
        s.equity_base = equity;
        if s.stop_pct_cfg == 0.0 {
            // ✅ P54/HALT-2: порог можно переопределить (.env P15_STOP_PCT,
            // 0.5..=20.0); дефолт 5% — согласован с K6 preflight (check_k6).
            s.stop_pct_cfg = std::env::var("P15_STOP_PCT")
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| (0.5..=20.0).contains(v))
                .unwrap_or(5.0);
        }
    }

    pub fn load_persisted_day(&self) {
        let mut g = Self::p15_lock();
        let s = g.get_or_insert_with(P15PortfolioState::default);
        s.day_no = Self::p15_day_no();
        if let Ok(txt) = std::fs::read_to_string(Self::p15_day_file()) {
            let parts: Vec<&str> = txt.trim().split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(day) = parts[0].parse::<u64>() {
                    if day == s.day_no {
                        if let Ok(pnl) = parts[1].parse::<f64>() {
                            s.realized_today = pnl;
                        }
                    }
                }
            }
        }
        tracing::info!(
            "[P15/P54] portfolio HARD HALT armed | day={} | realized_today={:.4} | halt при -{:.1}% of {:.2} = -{:.2} USDT",
            s.day_no, s.realized_today, s.stop_pct_cfg, s.equity_base,
            s.equity_base * s.stop_pct_cfg / 100.0
        );
    }

    pub fn stop_pct(&self) -> f64 {
        let g = Self::p15_lock();
        match g.as_ref() {
            Some(s) => s.stop_pct_cfg,
            None => 5.0,
        }
    }

    /// ✅ P54/HALT-2: текущий дневной убыток портфеля (для метрик/диагностики).
    pub fn portfolio_realized_today(&self) -> f64 {
        let g = Self::p15_lock();
        g.as_ref().map(|s| s.realized_today).unwrap_or(0.0)
    }

    /// ЖЁСТКИЙ гейт входов: KILL file / дневной стоп портфеля / fail-closed.
    /// Err = вход запрещён (строка — причина для лога).
    pub fn entries_allowed(&self) -> Result<(), String> {
        if crate::utils::killfile::kill_switch_active() {
            return Err(String::from("KILL file present - entries disabled"));
        }
        let g = Self::p15_lock();
        if let Some(s) = g.as_ref() {
            // ✅ P54/HALT-3 (C8): персист сломан — счётчик убытков ненадёжен,
            // после рестарта стоп был бы обезоружен. Fail-CLOSED, не fail-open.
            if s.persistence_broken {
                return Err(String::from(
                    "portfolio daily-PnL persist FAILING (disk) - fail-closed: entries disabled",
                ));
            }
            if s.equity_base > 0.0 && s.stop_pct_cfg > 0.0 {
                let limit = s.equity_base * s.stop_pct_cfg / 100.0;
                if s.realized_today <= -limit {
                    // ✅ P54/HALT-2: это HARD HALT — входы запрещены ВСЕМ ботам
                    // (state общий на процесс), позиций это не касается:
                    // существующие ведутся штатно (exits-only mode).
                    return Err(format!(
                        "PORTFOLIO HARD HALT: realized today {:.4} <= -{:.4} (порог {:.1}% от {:.2}); exits-only",
                        s.realized_today, limit, s.stop_pct_cfg, s.equity_base
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn add_closed_pnl(&self, pnl: f64) {
        let snapshot;
        let crossed_halt;
        {
            let mut g = Self::p15_lock();
            let s = g.get_or_insert_with(P15PortfolioState::default);
            let today = Self::p15_day_no();
            if s.day_no != today {
                s.day_no = today;
                s.realized_today = 0.0; // new UTC day - reset
                s.halted_notified = false; // ✅ P54/HALT-2: новый день — новый инцидент
            }
            s.realized_today += pnl;
            let limit = if s.equity_base > 0.0 && s.stop_pct_cfg > 0.0 {
                s.equity_base * s.stop_pct_cfg / 100.0
            } else {
                f64::INFINITY
            };
            crossed_halt = s.realized_today <= -limit && !s.halted_notified;
            if crossed_halt {
                s.halted_notified = true;
            }
            snapshot = (s.day_no, s.realized_today);
        }
        // ✅ P54/HALT-3: сбой записи = fail-closed на входы (см. entries_allowed).
        let _ = std::fs::create_dir_all("data");
        match std::fs::write(
            Self::p15_day_file(),
            format!("{} {:.6}\n", snapshot.0, snapshot.1),
        ) {
            Ok(()) => {
                let mut g = Self::p15_lock();
                if let Some(s) = g.as_mut() {
                    s.persistence_broken = false; // восстановился
                }
            }
            Err(e) => {
                tracing::error!(
                    "[P54/HALT] 🚨 не удалось записать daily PnL ({}): после рестарта стоп был бы обезоружен — ВХОДЫ FAIL-CLOSED",
                    e
                );
                let mut g = Self::p15_lock();
                if let Some(s) = g.as_mut() {
                    s.persistence_broken = true;
                }
            }
        }
        // ✅ P54/HALT-2: один Telegram-алерт на переход лимита. Метод остался
        // синхронным (десяток call-сайтов в main.rs) — алерт уходит фоновой
        // задачей из tokio-контекста (все вызывающие — внутри рантайма).
        if crossed_halt {
            let detail = format!(
                "дневной убыток портфеля {:.4} USDT достиг лимита; новые входы запрещены у всех ботов (exits-only)",
                snapshot.1
            );
            if tokio::runtime::Handle::try_current().is_ok() {
                // detail переезжает в таск — future владеет строкой ('static).
                tokio::spawn(async move {
                    crate::utils::telegram::notify("PORTFOLIO HARD HALT", &detail).await;
                });
            }
        }
    }
}