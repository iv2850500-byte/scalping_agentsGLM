use anyhow::Result;
use reqwest::Client;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Глобальный трекер времени Binance.
/// ОДИН экземпляр на процесс — все REST-клиенты читают общий offset.
#[derive(Clone)]
pub struct TimeSync {
    offset_ms: Arc<parking_lot::RwLock<i64>>,
    synced: Arc<AtomicBool>,
}

impl TimeSync {
    pub fn new() -> Self {
        Self {
            offset_ms: Arc::new(parking_lot::RwLock::new(0)),
            synced: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Серверное время = локальное + offset.
    pub fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis() + *self.offset_ms.read()
    }

    /// Публичный геттер для проверки clock skew в main.rs.
    pub fn offset_ms(&self) -> i64 {
        *self.offset_ms.read()
    }

    /// Установить offset (используется Futures-клиентом).
    pub fn set_offset(&self, offset: i64) {
        *self.offset_ms.write() = offset;
        self.synced.store(true, Ordering::Release);
    }

    pub fn is_synced(&self) -> bool {
        self.synced.load(Ordering::Relaxed)
    }

    /// Синхронизация с Binance. Идемпотентна.
    pub async fn sync(&self, base_url: &str) -> Result<()> {
        if self.synced.load(Ordering::Acquire) {
            return Ok(());
        }

        let url = format!("{}/api/v3/time", base_url.trim_end_matches('/'));
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .pool_idle_timeout(std::time::Duration::from_secs(20)).tcp_keepalive(std::time::Duration::from_secs(15))/* P19-POOL */.build()?;
        let resp: serde_json::Value = client.get(&url).send().await?.json().await?;
        let server_ms = resp["serverTime"].as_i64().unwrap_or(0);
        let local_ms = chrono::Utc::now().timestamp_millis();
        let offset = server_ms - local_ms;

        *self.offset_ms.write() = offset;
        self.synced.store(true, Ordering::Release);

        tracing::info!(
            "⏰ Синхронизация часов: смещение {} мс (сервер − локально)",
            offset
        );
        Ok(())
    }

    /// Фоновый пересинк каждые 5 минут.
    pub fn spawn_periodic_sync(&self, base_url: String) {
    let offset = self.offset_ms.clone();
    let synced = self.synced.clone();

    // ✅ Определяем эндпоинт по URL
    let is_futures = base_url.contains("fapi")
        || base_url.contains("futures")
        || base_url.contains("future");
    let time_path = if is_futures { "fapi/v1/time" } else { "api/v3/time" };
    let base_url_clean = base_url.trim_end_matches('/').to_string();

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(300));
        interval.tick().await;

        loop {
            interval.tick().await;

            let client = Client::builder()
                .timeout(Duration::from_secs(5))
                .pool_idle_timeout(std::time::Duration::from_secs(20)).tcp_keepalive(std::time::Duration::from_secs(15))/* P19-POOL */.build()
                .unwrap_or_default();
            let url = format!("{}/{}", base_url_clean, time_path);

            if let Ok(resp) = client.get(&url).send().await {
                if let Ok(json) = resp.json::<serde_json::Value>().await {
                    let server_ms = json["serverTime"].as_i64().unwrap_or(0);
                    let local_ms = chrono::Utc::now().timestamp_millis();
                    let new_offset = server_ms - local_ms;
                    *offset.write() = new_offset;
                    synced.store(true, Ordering::Release);

                    tracing::debug!("⏰ Пересинк: смещение {} мс", new_offset);
                }
            }
        }
    });
    }
}

impl Default for TimeSync {
    fn default() -> Self {
        Self::new()
    }
}