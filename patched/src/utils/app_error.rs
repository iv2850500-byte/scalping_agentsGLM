//! P64/: типизированные ошибки приложения (E5b, thiserror).
//!
//! Зачем: `main` возвращал `anyhow::Result` — при старте ЛЮБАЯ ошибка
//! давала exit-код 1, и внешний supervisor (P64 supervisor.ps1) не мог
//! отличить «конфиг битый — рестарт бессмыслен» от «сеть флапнула —
//! рестарт нужен». AppError несёт класс ошибки и канонический exit-код:
//!
//!   0   — чистое завершение (ctrl_c / deploy.ps1 stop);
//!   1   — фатальная (конфиг/ключи) — рестарт бессмыслен: алерт и стоп;
//!   2   — временная/краш — рестарт с backoff;
//!   3   — stall по watchdog — рестарт;
//!   101 — паника Rust (дефолтный runtime) — рестарт.
//!
//! Миграция call-site'ов на AppError постепенная (E5c+); сейчас тип
//! фундамент: main-обёртка (P64) маппит любой anyhow::Error через
//! From, тесты фиксируют семантику кодов.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    /// P64/: битый/неполный config.toml, отсутствующие секции.
    #[error("config: {0}")]
    Config(String),

    /// P64/: ключи отклонены биржей (401/403), allow_live-барьер и т.п.
    #[error("auth: {0}")]
    Auth(String),

    /// P64/: сеть/REST/WS недоступны (транзиентное).
    #[error("network: {0}")]
    Network(String),

    /// P64/: биржа ответила ошибкой (429/5xx/слишком много веса).
    #[error("exchange: {0}")]
    Exchange(String),

    /// P64/: SQLite (state.db) недоступна/повреждена.
    #[error("db: {0}")]
    Db(#[from] sqlx::Error),

    /// P64/: локальный файл/каталог (логи, KILL, логи супервизора).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// P64/: некорректный JSON/TOML-документ.
    #[error("format: {0}")]
    Format(#[from] serde_json::Error),

    /// P64/: событийный цикл молчит дольше порога (заготовка под
    /// exit 3; процессный supervisor.ps1 рестартует по коду).
    #[error("watchdog: событийный цикл молчит {0} сек")]
    WatchdogStall(u64),

    /// P64/: пойманная паника (hook пишет в лог, класс — для кодов).
    #[error("panic: {0}")]
    Panic(String),

    /// P64/: всё остальное, включая anyhow-ошибки до миграции (E5c+).
    #[error("{0}")]
    Other(String),
}

impl AppError {
    /// P64/: канонический exit-код процесса для supervisor.ps1.
    pub fn exit_code(&self) -> i32 {
        match self {
            AppError::Config(_) | AppError::Auth(_) => 1,
            AppError::WatchdogStall(_) => 3,
            _ => 2,
        }
    }

    /// P64/: true = рестарт бессмыслен (ошибка не уйдёт сама).
    pub fn is_fatal(&self) -> bool {
        matches!(self, AppError::Config(_) | AppError::Auth(_))
    }
}

impl From<anyhow::Error> for AppError {
    // P64/: anyhow-ошибки приходят из кода до миграции на AppError;
    // классифицируем как Other (exit 2 = рестарт с backoff).
    fn from(e: anyhow::Error) -> Self {
        AppError::Other(e.to_string())
    }
}

// P65/E5c: классификация anyhow-ошибки для main-обёртки: если внутри
// уже лежит мигрированный AppError (config load, allow_live, ...) —
// берём как есть; иначе Other (exit 2, рестарт с backoff).
pub fn classify(e: anyhow::Error) -> AppError {
    e.downcast::<AppError>()
        .unwrap_or_else(|e| AppError::Other(e.to_string()))
}

/// P64/: канонический Result приложения.
pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod p64_tests {
    use super::*;

    // P64/: фатальные классы -> exit 1 (supervisor НЕ рестартует)
    #[test]
    fn t64_exit_code_fatal_is_one() {
        assert_eq!(
            AppError::Config("config.toml: битый TOML".into()).exit_code(),
            1
        );
        assert_eq!(AppError::Auth("401: ключ отклонён".into()).exit_code(), 1);
        assert!(AppError::Config("x".into()).is_fatal());
        assert!(AppError::Auth("y".into()).is_fatal());
    }

    // P64/: watchdog stall -> exit 3, не фатальный
    #[test]
    fn t64_exit_code_stall_is_three() {
        assert_eq!(AppError::WatchdogStall(180).exit_code(), 3);
        assert!(!AppError::WatchdogStall(180).is_fatal());
    }

    // P64/: транзиентные классы -> exit 2 (рестарт с backoff)
    #[test]
    fn t64_exit_code_transient_is_two() {
        let io = AppError::from(std::io::Error::new(
            std::io::ErrorKind::Other,
            "disk full",
        ));
        assert_eq!(io.exit_code(), 2);
        let js = AppError::from(
            serde_json::from_str::<serde_json::Value>("{bad").unwrap_err(),
        );
        assert_eq!(js.exit_code(), 2);
        assert_eq!(AppError::Network("timeout".into()).exit_code(), 2);
        assert_eq!(AppError::Exchange("429 rate limit".into()).exit_code(), 2);
        assert_eq!(AppError::Panic("task panicked".into()).exit_code(), 2);
        assert_eq!(AppError::Other("misc".into()).exit_code(), 2);
        assert!(!io.is_fatal());
    }

    // P64/: anyhow -> Other с сохранением текста
    #[test]
    fn t64_anyhow_to_other_keeps_text() {
        let a = anyhow::anyhow!("sync_time не удался");
        let e: AppError = a.into();
        assert!(
            matches!(&e, AppError::Other(s) if s.contains("sync_time")),
            "unexpected: {e:?}"
        );
        assert_eq!(e.exit_code(), 2);
    }

    // P64/: Display человекочитаем (попадёт в лог и алерт)
    #[test]
    fn t64_display_readable() {
        // точный текст sqlx может меняться между версиями — проверяем префикс
        let db = AppError::Db(sqlx::Error::RowNotFound).to_string();
        assert!(db.starts_with("db: "), "unexpected: {db}");
        assert!(db.contains("no rows"), "unexpected: {db}");
        assert_eq!(
            AppError::WatchdogStall(95).to_string(),
            "watchdog: событийный цикл молчит 95 сек"
        );
    }

    // P65/E5c: downcast возвращает мигрированный AppError как есть (exit 1)
    #[test]
    fn t65_classify_downcasts_app_error() {
        let e = anyhow::Error::new(AppError::Config("config.toml: нет секции [global]".into()));
        let c = classify(e);
        assert!(matches!(c, AppError::Config(_)), "unexpected: {c:?}");
        assert_eq!(c.exit_code(), 1);
        assert!(c.is_fatal());
    }

    // P65/E5c: прочие ошибки -> Other (exit 2), текст сохранён
    #[test]
    fn t65_classify_wraps_other_as_other() {
        let e = anyhow::anyhow!("sync_time не удался");
        let c = classify(e);
        assert!(matches!(&c, AppError::Other(s) if s.contains("sync_time")), "unexpected: {c:?}");
        assert_eq!(c.exit_code(), 2);
        assert!(!c.is_fatal());
    }
}
