//! ✅ Фаза 3г: свечи для futures через REST-поллинг.
//! fstream отдаёт только bookTicker (доказано пробником), REST fapi работает полностью.
//! ✅ Фаза 6: интервал настраивается per-bot (kline_interval: "1m", "15m"...).
//! ✅ П3: первый успешный запрос — backfill (limit=250).
//! 🔧 АНТИ-БАН v2: интервал 10с, бэкофф 10→60с, бан-пауза 120с,
//! телеметрия возраста REST-тишины.
//! 🛡️ RESILIENT v3: 3 внутренние попытки с backoff 500мс+1с внутри
//! одной poll-итерации. Ловит транзиентные TCP-обрывы (os error 10054),
//! которые testnet Binance выдаёт каждые ~50с. Внешняя pause_secs не
//! тронута — она для IP-банов и затяжных падений.
//!
//! ─────────────────────────────────────────────────────────────────────
//! 🔧 FIX 23.10.2026 — BUG#1 (gold_dca candles=0 на протяжении 48ч):
//!   1. Bybit: close_time_ms больше НЕ хардкод «+1 час» — вычисляется
//!      из интервала (4h → +14_400_000). Раньше незакрытая 4h-свеча
//!      публиковалась через час как is_closed=true, а её реальное
//!      закрытие потом терялось (open_time <= last_open_time → skip).
//!   2. Bybit: resp.text() + детект HTML ДО парсинга JSON. Геоблок-страница
//!      раньше падала в "error decoding response body" без диагностики —
//!      поллер молотил впустую и ни разу не публиковал свечу.
//!   3. Binance-ветка НЕ тронута (работает стабильно).
//! ─────────────────────────────────────────────────────────────────────
use crate::websocket::{KlineMessage, WebSocketHub, WebSocketMessage};
use std::sync::Arc;
use std::time::Duration;

/// 🛡️ RESILIENT: сколько попыток внутри одной poll-итерации.
const INNER_ATTEMPTS: u32 = 3;
/// 🛡️ RESILIENT: паузы между внутренними попытками (мс).
const INNER_BACKOFF_MS: &[u64] = &[500, 1000];

/// 🔧 FIX: пауза при детектированном геоблоке (HTML вместо JSON).
/// Ретраить бессмысленно — HTML не «пожелтеет» в JSON, пауза длинная.
const GEOBLOCK_PAUSE_SECS: u64 = 120;

pub fn spawn(
    children: &mut tokio::task::JoinSet<()>,
    hub: Arc<WebSocketHub>,
    rest_base: String,
    symbol: String,
    interval: String,
) {
    children.spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .pool_idle_timeout(std::time::Duration::from_secs(20)).tcp_keepalive(std::time::Duration::from_secs(15))/* P19-POOL */.build()
            .unwrap_or_default();
        let base = rest_base.trim_end_matches('/').to_string();
        let mut last_open_time: i64 = 0;
        let mut backfilled = false; // ✅ П3: до первого успешного ответа — широкий запрос
        let mut poll = tokio::time::interval(Duration::from_secs(10));
        poll.tick().await; // первый тик мгновенный — пропускаем

        // 🔧 АНТИ-БАН v2: динамическая пауза
        let mut pause_secs: u64 = 0;
        let mut consec_fails: u64 = 0;
        // 🔧 ТЕЛЕМЕТРИЯ: момент последнего успешного REST (0 = ещё не было)
        let mut last_ok_ms: i64 = 0;

        loop {
            poll.tick().await;

            // 🔧 АНТИ-БАН: пауза после бана/серии фейлов
            if pause_secs > 0 {
                tokio::time::sleep(Duration::from_secs(pause_secs)).await;
            }

            // ✅ B2: поллер — non-critical: при перегрузке пропускаем тик
            if crate::utils::rate_limiter::LIMITER.acquire(false).await
                == crate::utils::rate_limiter::Acquire::Throttle
            {
                continue;
            }

            let limit = if backfilled { 2 } else { 250 };
            let url = format!(
                "{}/fapi/v1/klines?symbol={}&interval={}&limit={}",
                base,
                symbol.to_uppercase(),
                interval,
                limit
            );

            // 🛡️ RESILIENT: 3 попытки с backoff 500мс+1с внутри одной
            // poll-итерации. Ловит транзиентные TCP-обрывы, которые testnet
            // выдаёт каждые ~50с (os error 10054). После 3 провалов —
            // делегируем во внешнюю pause_secs логику (IP-бан / затяжной сбой).
            let v_opt: Option<serde_json::Value> = {
                let mut attempts: u32 = 0;
                loop {
                    attempts += 1;
                    let resp_result = client.get(&url).send().await;
                    match resp_result {
                        Ok(r) => match read_body_json(r).await {
                            Ok(v) => break Some(v),
                            Err(e) if attempts < INNER_ATTEMPTS => {
                                tracing::warn!(
                                    "📈 [{}] kline-поллер: JSON не разобран, попытка {}/{}: {}",
                                    symbol, attempts, INNER_ATTEMPTS, e
                                );
                                let backoff = INNER_BACKOFF_MS
                                    [(attempts - 1) as usize % INNER_BACKOFF_MS.len()];
                                tokio::time::sleep(Duration::from_millis(backoff)).await;
                                continue;
                            }
                            Err(e) => {
                                consec_fails += 1;
                                pause_secs = (10u64.saturating_mul(consec_fails.min(6))).min(60);
                                tracing::warn!(
                                    "📈 [{}] kline-поллер: ответ не разобран \
                                     (фейл {}, пауза {}с, REST-тишина {}с): {}",
                                    symbol, consec_fails, pause_secs,
                                    rest_silence(last_ok_ms), e
                                );
                                break None;
                            }
                        },
                        Err(e) if attempts < INNER_ATTEMPTS => {
                            tracing::warn!(
                                "📈 [{}] kline-поллер: запрос не прошёл, попытка {}/{}: {}",
                                symbol, attempts, INNER_ATTEMPTS, e
                            );
                            let backoff = INNER_BACKOFF_MS
                                [(attempts - 1) as usize % INNER_BACKOFF_MS.len()];
                            tokio::time::sleep(Duration::from_millis(backoff)).await;
                            continue;
                        }
                        Err(e) => {
                            consec_fails += 1;
                            pause_secs = (10u64.saturating_mul(consec_fails.min(6))).min(60);
                            tracing::warn!(
                                "📈 [{}] kline-поллер: запрос не прошёл \
                                 (фейл {}, пауза {}с, REST-тишина {}с): {}",
                                symbol, consec_fails, pause_secs,
                                rest_silence(last_ok_ms), e
                            );
                            break None;
                        }
                    }
                }
            };

            // 🛡️ Все 3 внутренние попытки провалились — пропускаем обработку,
            // внешняя pause_secs сработает на следующем тике.
            let Some(v) = v_opt else { continue };

            // 🔧 АНТИ-БАН: -1003 = IP ban — пауза 120с, бан не продлеваем
            if v["code"].as_i64() == Some(-1003) {
                pause_secs = 120;
                consec_fails += 1;
                tracing::error!(
                    "📈 [{}] kline-поллер: 🚨 IP BANNED (-1003): {} — пауза 120с",
                    symbol,
                    v["msg"].as_str().unwrap_or("?")
                );
                continue;
            }
            // Прочие ошибки API (не массив)
            if v.as_array().is_none() {
                consec_fails += 1;
                pause_secs = (10u64.saturating_mul(consec_fails.min(6))).min(60);
                tracing::warn!(
                    "📈 [{}] kline-поллер: ответ не массив \
                     (фейл {}, пауза {}с, REST-тишина {}с): {}",
                    symbol, consec_fails, pause_secs,
                    rest_silence(last_ok_ms), v
                );
                continue;
            }

            // Успех: сброс счётчиков
            consec_fails = 0;
            pause_secs = 0;
            last_ok_ms = chrono::Utc::now().timestamp_millis();

            let rows = v.as_array().unwrap();

            for row in rows {
                let Some(open_time) = row[0].as_i64() else { continue };
                let Some(close_time) = row[6].as_i64() else { continue };
                if close_time >= chrono::Utc::now().timestamp_millis() {
                    continue; // ещё формируется
                }
                if open_time <= last_open_time {
                    continue; // уже публиковали
                }
                let p = |i: usize| row[i].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                last_open_time = open_time;

                let close = p(4);
                hub.publish(WebSocketMessage::Kline(KlineMessage {
                    symbol: symbol.to_uppercase(),
                    open: p(1),
                    high: p(2),
                    low: p(3),
                    close,
                    volume: p(5),
                    taker_buy_volume: p(9),
                    timestamp: close_time,
                    is_closed: true,
                    is_backfill: !backfilled,
                }));
                if backfilled {
                    tracing::info!(
                        "📈 [{}] kline-поллер: свеча опубликована (open_time={}, close={})",
                        symbol, open_time, close
                    );
                }
            }

            if !rows.is_empty() {
                if !backfilled {
                    tracing::info!(
                        "📈 [{}] kline-поллер: backfill завершён ({} свечей истории)",
                        symbol, rows.len()
                    );
                }
                backfilled = true;
            }
        }
    });
}

/// 🔍 V3.2: тело ответа как текст + ручной парсинг. При ошибке в сообщение
/// попадает HTTP-статус и первые 160 символов тела — сразу видно, ЧТО
/// вернул тестнет (HTML-страница, JSON-ошибка, пустота) вместо голого
/// "error decoding response body".
async fn read_body_json(r: reqwest::Response) -> Result<serde_json::Value, String> {
    let status = r.status();
    let body = match r.text().await {
        Ok(b) => b,
        Err(e) => return Err(format!("HTTP {} | тело не прочитано: {}", status, e)),
    };
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(v) => Ok(v),
        Err(e) => Err(format!(
            "HTTP {} | тело[:160]: {:?} | serde: {}",
            status,
            body.chars().take(160).collect::<String>(),
            e
        )),
    }
}

/// 🆕 Phase B: Bybit V5 kline poller.
///
/// Отличия от Binance:
///   1. URL: /v5/market/kline?category=linear&symbol=XAUUSDT&interval=60&limit=2
///   2. interval = "60" (число минут), не "1h" как у Binance
///   3. Response: result.list — array of arrays, НО РЕВЕРСНУТЫЙ (новейшие первыми!)
///      Format: [start, open, high, low, close, volume, turnover, ...]
///      Время — в СЕКУНДАХ (не миллисекундах как у Binance!)
///   4. confirm=true — свеча закрыта
pub fn spawn_bybit(
    children: &mut tokio::task::JoinSet<()>,
    hub: Arc<WebSocketHub>,
    rest_base: String,
    symbol: String,
    interval: String,
) {
    children.spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .pool_idle_timeout(std::time::Duration::from_secs(20)).tcp_keepalive(std::time::Duration::from_secs(15))/* P19-POOL */.build()
            .unwrap_or_default();
        let base = rest_base.trim_end_matches('/').to_string();

        // Конвертация interval строки в Bybit-формат:
        //   "1h" → "60", "4h" → "240", "15m" → "15", "1d" → "D", "1w" → "W"
        let bybit_interval = match interval.as_str() {
            "1m" => "1".to_string(),
            "5m" => "5".to_string(),
            "15m" => "15".to_string(),
            "30m" => "30".to_string(),
            "1h" => "60".to_string(),
            "4h" => "240".to_string(),
            "1d" => "D".to_string(),
            "1w" => "W".to_string(),
            "1M" => "M".to_string(),
            other => other.to_string(),
        };

        // 🔧 FIX BUG#1: длительность интервала в мс — для честного close_time.
        // Раньше: close_time_ms = open_time_ms + 3600_000 (хардкод 1h!),
        // а gold_dca работает на 4h → незакрытые 4h-свечи публиковались
        // через час как is_closed=true, реальное закрытие терялось.
        let bybit_interval_ms: i64 = match bybit_interval.as_str() {
            "1" => 60_000,
            "5" => 300_000,
            "15" => 900_000,
            "30" => 1_800_000,
            "60" => 3_600_000,
            "120" => 7_200_000,
            "240" => 14_400_000,
            "360" => 21_600_000,
            "480" => 28_800_000,
            "720" => 43_200_000,
            "D" => 86_400_000,
            "W" => 604_800_000,
            // Оценка 31 день: если чуть недозакрыта — опубликуется на
            // следующем цикле (10с), безопасно в обе стороны.
            "M" => 2_678_400_000,
            _ => 3_600_000,
        };

        let mut last_open_time: i64 = 0;
        let mut backfilled = false;
        let mut poll = tokio::time::interval(Duration::from_secs(10));
        poll.tick().await;

        let mut pause_secs: u64 = 0;
        let mut consec_fails: u64 = 0;

        loop {
            poll.tick().await;

            if pause_secs > 0 {
                tokio::time::sleep(Duration::from_secs(pause_secs)).await;
            }

            if crate::utils::rate_limiter::LIMITER.acquire(false).await
                == crate::utils::rate_limiter::Acquire::Throttle
            {
                continue;
            }

            let limit = if backfilled { 2 } else { 250 };
            let url = format!(
                "{}/v5/market/kline?category=linear&symbol={}&interval={}&limit={}",
                base,
                symbol.to_uppercase(),
                bybit_interval,
                limit
            );

            let resp = match client.get(&url).send().await {
                Ok(r) => r,
                Err(e) => {
                    consec_fails += 1;
                    pause_secs = (10u64.saturating_mul(consec_fails.min(6))).min(60);
                    tracing::warn!(
                        "📈 [{}] kline-поллер (Bybit): запрос не прошёл (фейл {}, пауза {}с): {}",
                        symbol, consec_fails, pause_secs, e
                    );
                    continue;
                }
            };

            // 🔧 FIX BUG#1: сначала ТЕКСТ и детект HTML — геоблок Bybit с RU-IP
            // отдаёт страницу вместо JSON. Раньше resp.json() падал с
            // "error decoding response body" без диагностики, поллер крутился
            // 48ч впустую и ни разу не публиковал свечу (= candles=0).
            let body = match resp.text().await {
                Ok(b) => b,
                Err(e) => {
                    consec_fails += 1;
                    pause_secs = (10u64.saturating_mul(consec_fails.min(6))).min(60);
                    tracing::warn!(
                        "📈 [{}] kline-поллер (Bybit): тело не прочитано \
                         (фейл {}, пауза {}с): {}",
                        symbol, consec_fails, pause_secs, e
                    );
                    continue;
                }
            };
            if looks_like_html(None, &body) {
                // HTML не станет JSON от ретраев — длинная пауза, внятный лог
                consec_fails += 1;
                pause_secs = GEOBLOCK_PAUSE_SECS;
                tracing::error!(
                    "📈 [{}] kline-поллер (Bybit): 🚨 GEOBLOCK — HTML вместо JSON \
                     (фейл {}, пауза {}с): {}",
                    symbol, consec_fails, pause_secs, body_head(&body)
                );
                continue;
            }
            let v: serde_json::Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(e) => {
                    consec_fails += 1;
                    pause_secs = (10u64.saturating_mul(consec_fails.min(6))).min(60);
                    tracing::warn!(
                        "📈 [{}] kline-поллер (Bybit): ответ не разобран \
                         (фейл {}, пауза {}с): {} | тело: {}",
                        symbol, consec_fails, pause_secs, e, body_head(&body)
                    );
                    continue;
                }
            };

            // Проверяем Bybit V5 retCode
            let ret_code = v["retCode"].as_i64().unwrap_or(-1);
            if ret_code != 0 {
                consec_fails += 1;
                pause_secs = (10u64.saturating_mul(consec_fails.min(6))).min(60);
                tracing::warn!(
                    "📈 [{}] kline-поллер (Bybit): retCode={} msg={}",
                    symbol, ret_code,
                    v["retMsg"].as_str().unwrap_or("?")
                );
                continue;
            }

            let rows = match v["result"]["list"].as_array() {
                Some(a) => a,
                None => {
                    consec_fails += 1;
                    pause_secs = (10u64.saturating_mul(consec_fails.min(6))).min(60);
                    tracing::warn!(
                        "📈 [{}] kline-поллер (Bybit): result.list не массив: {}",
                        symbol, v
                    );
                    continue;
                }
            };

            // Успех
            consec_fails = 0;
            pause_secs = 0;

            // Bybit возвращает свечи В ОБРАТНОМ ПОРЯДКЕ (новейшие первыми).
            // Разворачиваем для последовательной обработки.
            let mut sorted_rows: Vec<&serde_json::Value> = rows.iter().collect();
            sorted_rows.reverse();

            for row in sorted_rows {
                // Bybit kline row format:
                // [start (sec), open, high, low, close, volume, turnover]
                // 0     1     2     3    4      5       6
                let Some(start_str) = row[0].as_str() else { continue };
                let Ok(start_raw) = start_str.parse::<i64>() else { continue };
                let open_time_ms = if start_raw > 100_000_000_000 { start_raw } else { start_raw * 1000 }; // v3.6 FIX: Bybit TESTNET gives ms (13 digits), MAINNET gives sec - autodetect // sec → ms
                // 🔧 FIX BUG#1: было `+ 3600_000` (хардкод 1h — битые 4h-свечи!)
                let close_time_ms = open_time_ms + bybit_interval_ms;

                // Пропускаем незакрытые свечи
                if close_time_ms >= chrono::Utc::now().timestamp_millis() {
                    continue;
                }
                if open_time_ms <= last_open_time {
                    continue;
                }

                let p = |i: usize| row[i].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                last_open_time = open_time_ms;

                let close = p(4);
                hub.publish(WebSocketMessage::Kline(KlineMessage {
                    symbol: symbol.to_uppercase(),
                    open: p(1),
                    high: p(2),
                    low: p(3),
                    close,
                    volume: p(5),
                    taker_buy_volume: 0.0, // Bybit не отдаёт taker_buy в этом endpoint
                    timestamp: close_time_ms,
                    is_closed: true,
                    is_backfill: !backfilled,
                }));
                if backfilled {
                    tracing::info!(
                        "📈 [{}] kline-поллер (Bybit): свеча опубликована (open_time={}, close={})",
                        symbol, open_time_ms, close
                    );
                }
            }

            if !rows.is_empty() {
                if !backfilled {
                    tracing::info!(
                        "📈 [{}] kline-поллер (Bybit): backfill завершён ({} свечей истории)",
                        symbol, rows.len()
                    );
                }
                backfilled = true;
            }
        }
    });
}

/// 🔧 FIX: детект HTML-ответа (геоблок-страница Bybit) ДО попытки JSON-парсинга
fn looks_like_html(content_type: Option<&str>, body: &str) -> bool {
    if let Some(ct) = content_type {
        if ct.to_ascii_lowercase().contains("text/html") {
            return true;
        }
    }
    let head = body.trim_start().to_ascii_lowercase();
    head.starts_with("<!doctype html") || head.starts_with("<html")
}

/// 🔧 FIX: голова тела для диагностики (первые 160 символов, без переносов)
fn body_head(body: &str) -> String {
    body.chars()
        .take(160)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Секунды с последнего успешного REST (для строк диагностики)
fn rest_silence(last_ok_ms: i64) -> i64 {
    if last_ok_ms == 0 {
        return -1; // ещё ни одного успеха
    }
    chrono::Utc::now().timestamp_millis() / 1000 - last_ok_ms / 1000
}

/// v4.0: OKX V5 kline poller (public /api/v5/market/candles).
/// bar: 1m/3m/5m/15m/30m/1H/2H/4H/6H/12H. Rows NEWEST-first:
/// [ts, o, h, l, c, vol_contracts, vol_base, vol_quote, confirm]
pub fn spawn_okx(
    children: &mut tokio::task::JoinSet<()>,
    hub: Arc<WebSocketHub>,
    rest_base: String,
    symbol: String,
    interval: String,
) {
    children.spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .pool_idle_timeout(std::time::Duration::from_secs(20)).tcp_keepalive(std::time::Duration::from_secs(15))/* P19-POOL */.build()
            .unwrap_or_default();
        let base = rest_base.trim_end_matches('/').to_string();

        let bar = match interval.as_str() {
            "1h" => "1H".to_string(),
            "2h" => "2H".to_string(),
            "4h" => "4H".to_string(),
            "6h" => "6H".to_string(),
            "12h" => "12H".to_string(),
            other => other.to_string(),
        };
        let interval_ms: i64 = match bar.as_str() {
            "1m" => 60_000,
            "3m" => 180_000,
            "5m" => 300_000,
            "15m" => 900_000,
            "30m" => 1_800_000,
            "1H" => 3_600_000,
            "2H" => 7_200_000,
            "4H" => 14_400_000,
            "6H" => 21_600_000,
            "12H" => 43_200_000,
            _ => 3_600_000,
        };

        let inst_id = okx_inst_id_from_symbol(&symbol);

        let mut last_open_time: i64 = 0;
        let mut backfilled = false;
        let mut consec_fails: u64 = 0;
        let mut poll = tokio::time::interval(Duration::from_secs(10));
        poll.tick().await;

        loop {
            poll.tick().await;

            let limit = if backfilled { 3 } else { 250 };
            let url = format!(
                "{}/api/v5/market/candles?instId={}&bar={}&limit={}",
                base, inst_id, bar, limit
            );

            let body = match client.get(&url).send().await {
                Ok(r) => match r.text().await {
                    Ok(b) => b,
                    Err(e) => {
                        consec_fails += 1;
                        let pause = (10u64 * consec_fails.min(6)).min(60);
                        tracing::warn!(
                            "[{}] kline-poller (OKX): body read failed (fail {}, pause {}s): {}",
                            symbol, consec_fails, pause, e
                        );
                        tokio::time::sleep(Duration::from_secs(pause)).await;
                        continue;
                    }
                },
                Err(e) => {
                    consec_fails += 1;
                    let pause = (10u64 * consec_fails.min(6)).min(60);
                    tracing::warn!(
                        "[{}] kline-poller (OKX): request failed (fail {}, pause {}s): {}",
                        symbol, consec_fails, pause, e
                    );
                    tokio::time::sleep(Duration::from_secs(pause)).await;
                    continue;
                }
            };

            if body.trim_start().starts_with('<') {
                tracing::error!(
                    "[{}] kline-poller (OKX): GEOBLOCK/HTML instead of JSON: {}",
                    symbol,
                    body.chars().take(120).collect::<String>()
                );
                tokio::time::sleep(Duration::from_secs(300)).await;
                continue;
            }

            let v: serde_json::Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(e) => {
                    consec_fails += 1;
                    let pause = (10u64 * consec_fails.min(6)).min(60);
                    tracing::warn!(
                        "[{}] kline-poller (OKX): parse failed (fail {}, pause {}s): {} | body: {}",
                        symbol,
                        consec_fails,
                        pause,
                        e,
                        body.chars().take(120).collect::<String>()
                    );
                    tokio::time::sleep(Duration::from_secs(pause)).await;
                    continue;
                }
            };

            if v["code"].as_str().unwrap_or("-1") != "0" {
                consec_fails += 1;
                tracing::warn!(
                    "[{}] kline-poller (OKX): code={} msg={}",
                    symbol,
                    v["code"].as_str().unwrap_or("?"),
                    v["msg"].as_str().unwrap_or("?")
                );
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }

            let rows = match v["data"].as_array() {
                Some(a) => a,
                None => {
                    consec_fails += 1;
                    tracing::warn!("[{}] kline-poller (OKX): data is not an array", symbol);
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    continue;
                }
            };

            consec_fails = 0;

            let mut sorted_rows: Vec<&serde_json::Value> = rows.iter().collect();
            sorted_rows.reverse(); // oldest first

            for row in sorted_rows {
                let Some(open_ms) = row[0].as_str().and_then(|s| s.parse::<i64>().ok()) else {
                    continue;
                };
                let close_ms = open_ms + interval_ms;
                if close_ms >= chrono::Utc::now().timestamp_millis() {
                    continue; // candle still forming
                }
                if open_ms <= last_open_time {
                    continue; // already published
                }
                let p = |i: usize| row[i].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                last_open_time = open_ms;

                hub.publish(WebSocketMessage::Kline(KlineMessage {
                    symbol: symbol.to_uppercase(),
                    open: p(1),
                    high: p(2),
                    low: p(3),
                    close: p(4),
                    volume: p(6), // volCcy = base-asset volume (Binance-like)
                    taker_buy_volume: 0.0,
                    timestamp: close_ms,
                    is_closed: true,
                    is_backfill: !backfilled,
                }));
            }

            if !rows.is_empty() {
                if !backfilled {
                    tracing::info!(
                        "[{}] kline-poller (OKX): backfill done ({} candles)",
                        symbol,
                        rows.len()
                    );
                }
                backfilled = true;
            }
        }
    });
}

/// v4.0: "XAUUSDT" -> "XAU-USDT-SWAP"
fn okx_inst_id_from_symbol(symbol: &str) -> String {
    let s = symbol.trim().to_uppercase();
    if s.contains('-') {
        if s.ends_with("-SWAP") { s } else { format!("{}-SWAP", s) }
    } else if let Some(base) = s.strip_suffix("USDT") {
        format!("{}-USDT-SWAP", base)
    } else {
        s
    }
}