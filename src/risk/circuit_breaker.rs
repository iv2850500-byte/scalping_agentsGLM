use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct CircuitBreaker {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    losses: VecDeque<Instant>,
    max_losses: usize,
    window: Duration,
    /// ✅ bk: cooldown из конфига (раньше был захардкожен 300 сек)
    cooldown: Duration,
    is_open: bool,
    last_trip: Option<Instant>,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new(5, 3600, 600)
    }
}

impl CircuitBreaker {
    /// * `max_losses` — сколько убытков подряд открывают breaker
    /// * `window_secs` — окно, за которое считаются убытки
    /// * `cooldown_secs` — сколько держать breaker открытым после срабатывания
    pub fn new(max_losses: usize, window_secs: u64, cooldown_secs: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                losses: VecDeque::new(),
                max_losses,
                window: Duration::from_secs(window_secs),
                cooldown: Duration::from_secs(cooldown_secs),
                is_open: false,
                last_trip: None,
            })),
        }
    }

    pub fn record_loss(&self) {
        let mut i = self.inner.lock();
        let now = Instant::now();

        // Чистим устаревшие потери за окном
        while let Some(&t) = i.losses.front() {
            if now.duration_since(t) > i.window {
                i.losses.pop_front();
            } else {
                break;
            }
        }

        // 🔧 ПАТЧ: лог продления кулдауна (раньше молча двигал last_trip)
        if i.is_open {
            tracing::warn!(
                "⏳ CIRCUIT BREAKER: убыток внутри cooldown — таймер продлён"
            );
        }

        i.losses.push_back(now);

        if i.losses.len() >= i.max_losses {
            if !i.is_open {
                tracing::warn!(
                    "🔴 CIRCUIT BREAKER TRIP: {} убытков за {:?}",
                    i.losses.len(),
                    i.window
                );
            }
            i.is_open = true;
            i.last_trip = Some(now);
        }
    }

    /// Проверка: открыт ли breaker.
    ///
    /// При истечении cooldown — полный сброс состояния.
    pub fn is_open(&self) -> bool {
        let mut i = self.inner.lock();

        if i.is_open {
            if let Some(t) = i.last_trip {
                if t.elapsed() >= i.cooldown {
                    tracing::info!(
                        "🟢 CIRCUIT BREAKER RESET (cooldown {:?} истёк)",
                        i.cooldown
                    );
                    i.is_open = false;
                    i.losses.clear();
                    i.last_trip = None;
                    return false;
                }
            }
            return true;
        }

        false
    }

    /// Принудительный сброс (например, ручной или по истечении дня)
    pub fn reset(&self) {
        let mut i = self.inner.lock();
        i.is_open = false;
        i.losses.clear();
        i.last_trip = None;
        tracing::info!("🟢 CIRCUIT BREAKER сброшен вручную");
    }

    /// Геттеры для диагностики
    pub fn losses_count(&self) -> usize {
        self.inner.lock().losses.len()
    }

    pub fn cooldown_remaining(&self) -> Option<Duration> {
        let i = self.inner.lock();
        if let Some(t) = i.last_trip {
            let elapsed = t.elapsed();
            if elapsed < i.cooldown {
                return Some(i.cooldown - elapsed);
            }
        }
        None
    }

    // ============================================================
    // 🔧 ПАТЧ: персист кулдауна — рестарт процесса больше не сбрасывает
    // открытый breaker (раньше рестарт = полный сброс защиты)
    // ============================================================

    /// Остаток кулдауна в мс (0 = breaker не открыт). Пишется в risk_stats.
    pub fn cooldown_remaining_ms(&self) -> u64 {
        let i = self.inner.lock();
        if i.is_open {
            if let Some(t) = i.last_trip {
                let elapsed = t.elapsed();
                if elapsed < i.cooldown {
                    return (i.cooldown - elapsed).as_millis() as u64;
                }
            }
        }
        0
    }

    /// Восстановление открытого breaker после рестарта с остатком кулдауна.
    pub fn restore_open_with_remaining(&self, remaining_ms: u64) {
        if remaining_ms == 0 {
            return;
        }
        let mut i = self.inner.lock();
        let back = i.cooldown.saturating_sub(Duration::from_millis(remaining_ms));
        // checked_sub: Instant не может уйти «до старта процесса» —
        // если аптайм меньше остатка кулдауна, считаем трип только что.
        i.last_trip = Some(Instant::now().checked_sub(back).unwrap_or_else(Instant::now));
        i.is_open = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn test_breaker_trips_after_max_losses() {
        let cb = CircuitBreaker::new(3, 3600, 60);
        assert!(!cb.is_open());

        cb.record_loss();
        assert!(!cb.is_open(), "1 loss — breaker закрыт");

        cb.record_loss();
        assert!(!cb.is_open(), "2 losses — breaker закрыт");

        cb.record_loss();
        assert!(cb.is_open(), "3 losses — breaker должен открыться");

        assert_eq!(cb.losses_count(), 3);
    }

    #[test]
    fn test_breaker_ignores_old_losses_outside_window() {
        let cb = CircuitBreaker::new(3, 1, 60);

        cb.record_loss();
        cb.record_loss();
        sleep(Duration::from_millis(1100));
        cb.record_loss();

        assert!(!cb.is_open(), "старые потери вышли из окна");
        assert!(cb.losses_count() <= 2);
    }

    #[test]
    fn test_breaker_manual_reset() {
        let cb = CircuitBreaker::new(2, 3600, 600);
        cb.record_loss();
        cb.record_loss();
        assert!(cb.is_open());

        cb.reset();
        assert!(!cb.is_open());
        assert_eq!(cb.losses_count(), 0);
    }

    #[test]
    fn test_breaker_cooldown_from_config() {
        let cb = CircuitBreaker::new(1, 3600, 0);
        cb.record_loss();
        assert!(!cb.is_open(), "cooldown 0 должен сразу сбросить breaker");
    }

    #[test]
    fn test_cooldown_remaining() {
        let cb = CircuitBreaker::new(1, 3600, 600);
        cb.record_loss();
        let remaining = cb.cooldown_remaining();
        assert!(remaining.is_some());
        assert!(remaining.unwrap() <= Duration::from_secs(600));
    }

    #[test]
    fn test_persist_restore_roundtrip() {
        let cb = CircuitBreaker::new(1, 3600, 600);
        cb.record_loss();
        let saved = cb.cooldown_remaining_ms();
        assert!(saved > 590_000 && saved <= 600_000);

        // «Рестарт»: новый breaker + восстановление
        let cb2 = CircuitBreaker::new(1, 3600, 600);
        assert!(!cb2.is_open());
        cb2.restore_open_with_remaining(saved);
        assert!(cb2.is_open(), "после restore breaker должен быть открыт");
        let left = cb2.cooldown_remaining_ms();
        assert!(left <= saved, "остаток не может вырасти");
    }
}