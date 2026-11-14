use anyhow::Result;
use std::future::Future;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            multiplier: 2.0,
        }
    }
}

/// Exponential backoff с ограничением.
///
/// Пример:
/// ```ignore
/// let res = retry_with_backoff(RetryConfig::default(), || async {
///     client.place_market_order(symbol, side, qty).await
/// }).await?;
/// ```
pub async fn retry_with_backoff<F, Fut, T>(
    cfg: RetryConfig,
    mut op: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut delay = cfg.initial_delay;
    let mut last_err = None;

    for attempt in 1..=cfg.max_attempts {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                tracing::warn!(
                    "retry attempt {}/{} failed: {}",
                    attempt, cfg.max_attempts, e
                );
                last_err = Some(e);

                if attempt == cfg.max_attempts {
                    break;
                }

                tokio::time::sleep(delay).await;
                let next = Duration::from_secs_f64(delay.as_secs_f64() * cfg.multiplier);
                delay = next.min(cfg.max_delay);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("retry failed")))
}