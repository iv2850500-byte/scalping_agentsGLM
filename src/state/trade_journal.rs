use crate::strategies::Signal;
use anyhow::Result;
use chrono::Utc;
use sqlx::sqlite::SqlitePool;

pub struct TradeJournal {
    pool: SqlitePool,
    bot_name: String,
}

impl TradeJournal {
    pub async fn new(db_path: &str, bot_name: &str) -> Result<Self> {
        let url = format!("sqlite:{}?mode=rwc", db_path);
        let pool = SqlitePool::connect(&url).await?;
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS trade_journal (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                bot_name TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                event TEXT NOT NULL,
                symbol TEXT,
                side TEXT,
                price REAL,
                quantity REAL,
                pnl REAL,
                details TEXT
            )"#,
        )
        .execute(&pool)
        .await?;
        Ok(Self { pool, bot_name: bot_name.to_string() })
    }

    pub async fn record_signal(&self, sig: &Signal) -> Result<()> {
        sqlx::query(
            "INSERT INTO trade_journal (bot_name, timestamp, event, symbol, side, price, quantity) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&self.bot_name)
        .bind(Utc::now().to_rfc3339())
        .bind("SIGNAL")
        .bind(&sig.symbol)
        .bind(&sig.side)
        .bind(sig.price)
        .bind(sig.quantity)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn record_execution(&self, sig: &Signal, status: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO trade_journal (bot_name, timestamp, event, symbol, details) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&self.bot_name)
        .bind(Utc::now().to_rfc3339())
        .bind("EXECUTION")
        .bind(&sig.symbol)
        .bind(status)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// ✅ PATCHED: теперь сохраняет side, price, quantity (для маркеров на дашборде)
    /// Раньше сохранял только symbol + pnl → dashboard фильтровал по !t.price → 0 маркеров
    pub async fn record_close(&self, sig: &Signal, pnl: f64) -> Result<()> {
        // ✅ [P62/2] Мгновенный Telegram-алерт закрытия ДО записи в БД.
        // record_close — единая воронка ВСЕХ путей закрытия (стратегия,
        // таймер, частичный выход, биржевое закрытие P53/H9): один сайт
        // патча покрывает все 11 мест вызова в main.rs. Алерт идёт ПЕРЕД
        // insert: если sqlite сломан, денежное событие всё равно уйдёт
        // оператору (сам алерт fire-and-forget, ошибок не возвращает).
        crate::utils::telegram::notify(
            "Trade closed",
            &crate::utils::telegram::fmt_close(&self.bot_name, sig, pnl),
        )
        .await;
        sqlx::query(
            "INSERT INTO trade_journal (bot_name, timestamp, event, symbol, side, price, quantity, pnl) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&self.bot_name)
        .bind(Utc::now().to_rfc3339())
        .bind("CLOSE")
        .bind(&sig.symbol)
        .bind(&sig.side)
        .bind(sig.price)
        .bind(sig.quantity)
        .bind(pnl)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
