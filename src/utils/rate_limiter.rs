//! ✅ B2: глобальный rate-limiter REST-запросов.
//! Глобальный статик (как метрики) — доступен отовсюду, ничего
//! не нужно протягивать через сигнатуры.

use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;

const WINDOW_MS: u64 = 60_000;
/// Консервативный бюджет: 1200 req/мин (половина лимита Binance 2400/мин)
const BUDGET: u64 = 1200;
const WARN_THRESHOLD: u64 = (BUDGET * 80) / 100;
const HARD_THRESHOLD: u64 = (BUDGET * 95) / 100;

/// Глобальный лимитер — один на процесс.
pub static LIMITER: Lazy<RateLimiter> = Lazy::new(RateLimiter::new);

#[derive(Debug, PartialEq)]
pub enum Acquire {
    /// Шлём запрос
    Allow,
    /// Перегрузка — non-критичный отправитель пропускает тик
    Throttle,
}

pub struct RateLimiter {
    window_start_ms: Mutex<u64>,
    counter: AtomicU64,
    throttled: AtomicU64,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            window_start_ms: Mutex::new(0),
            counter: AtomicU64::new(0),
            throttled: AtomicU64::new(0),
        }
    }

    /// Засчитать запрос. `critical = true` — ордера/отмены/статусы
    /// (всегда шлём, даже у потолка — закрытие позиции важнее лимита);
    /// `critical = false` — поллеры и прочий фон (можем пропустить тик).
    pub async fn acquire(&self, critical: bool) -> Acquire {
        let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
        let mut start = self.window_start_ms.lock().await;
        if *start == 0 || now.saturating_sub(*start) >= WINDOW_MS {
            *start = now;
            self.counter.store(0, Ordering::Relaxed);
        }
        let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;

        if n >= HARD_THRESHOLD {
            self.throttled.fetch_add(1, Ordering::Relaxed);
            if critical {
                tracing::error!(
                    "⛔ RATE: {} req/окно (потолок {}) — критический запрос пропущен",
                    n, HARD_THRESHOLD
                );
                Acquire::Allow
            } else {
                tracing::warn!("⏸ RATE: {} req/окно — фон отложен", n);
                Acquire::Throttle
            }
        } else if n == WARN_THRESHOLD {
            tracing::warn!("⚠️ RATE: {} req/окно (80% бюджета)", n);
            Acquire::Allow
        } else {
            Acquire::Allow
        }
    }
}