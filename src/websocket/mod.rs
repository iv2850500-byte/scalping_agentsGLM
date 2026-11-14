use crate::metrics;
use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use crate::exchange::Exchange; // ✅ P25: типизированная биржа вместо contains-проверок URL

// ─────────────────────────────────────────────────────────────────────
// 🔧 FIX 23.10.2026 — телеметрия REST-свечей (BUG#2 закрыт как not-a-bug):
// fstream доказанно отдаёт только bookTicker, свечи приходят через
// REST-поллер (kline_poller.rs → hub.publish). Heartbeat раньше показывал
// «свечи=0» вечно — ложный сигнал поломки. Теперь поллерские публикации
// считаются отдельно: WsHealth.rest_klines → «свечи_REST» в heartbeat.
// Остальной код НЕ изменён.
// ─────────────────────────────────────────────────────────────────────

const WS_ENDPOINTS: &[&str] = &[
    "wss://stream.binance.com:9443",
    "wss://stream.binance.com:443",
    "wss://data-stream.binance.vision",
];

const RECONNECT_THRESHOLD_FOR_SWITCH: u32 = 5;

/// 🔧 ПАТЧ-TIMEOUT (23.10, живой лог): 10с не хватило Bybit testnet —
/// два handshake timeout подряд на холодном старте. 25с покрывает
/// медленный тестнет и не влияет на живые сессии.
const HANDSHAKE_TIMEOUT_SECS: u64 = 25;
const CLOCK_SKEW_THRESHOLD_MS: u64 = 500;
const PONG_DEAD_THRESHOLD_SEC: u64 = 60;
const MARKET_DATA_SILENCE_LOG_SEC: u64 = 60;

/// 🔧 ПАТЧ-BACKOFF: сессия, прожившая дольше этого порога, считается
/// успешной — счётчик сбоев сбрасывается и backoff не эскалирует.
/// Раньше ЛЮБОЙ разрыв (даже после часов жизни) считался фейлом,
/// и backoff монотонно рос до 30с — слепые окна для exit-проверок.
const WS_SESSION_OK_SECS: u64 = 60;

#[derive(Debug, Clone)]
pub enum WebSocketMessage {
    Trade(TradeMessage),
    OrderBook(OrderBookMessage),
    Kline(KlineMessage),
    BookTicker(BookTickerMessage),
}

#[derive(Debug, Clone)]
pub struct TradeMessage {
    pub symbol: String,
    pub price: f64,
    pub volume: f64,
    pub timestamp: i64,
    pub event_time: i64,
    pub is_buyer_maker: bool,
}

#[derive(Debug, Clone)]
pub struct OrderBookMessage {
    pub symbol: String,
    pub bids: Vec<(f64, f64)>,
    pub asks: Vec<(f64, f64)>,
}

#[derive(Debug, Clone)]
pub struct KlineMessage {
    pub symbol: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    /// ✅ Объём агрессивных покупок (taker buys) — нужен для OFI.
    pub taker_buy_volume: f64,
    /// ✅ close_time свечи (момент закрытия).
    pub timestamp: i64,
    pub is_closed: bool,
    /// ✅ П3: true = историческая свеча из backfill-пакета поллера.
    /// Кормит индикаторы стратегий, но игнорируется exit-проверками
    /// (старые цены не должны срабатывать SL/TP восстановленных позиций).
    pub is_backfill: bool,
}

#[derive(Debug, Clone)]
pub struct BookTickerMessage {
    pub symbol: String,
    pub best_bid: f64,
    pub best_ask: f64,
    pub bid_qty: f64,
    pub ask_qty: f64,
    /// ✅ К-20: event time из стрима. На futures есть поле E;
    /// на споте его может не быть — тогда локальное время получения.
    pub timestamp: i64,
}

// ✅ P41/W9: WebSocketMessage::exchange() удалён — метод всегда возвращал
// "binance" независимо от источника (дезинформация) и не имел ни одного
// вызывающего в кодовой базе (мёртвый API).

// ✅ P27: глобальный пульс bookTicker по ВСЕМ хабам (WsHealth::book_tickers —
// per-hub; этот суммарный — для heartbeat-строки в main.rs и внешнего мониторинга).
pub static BT_TOTAL: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
pub struct WsHealth {
    pub received: AtomicU64,
    pub parsed: AtomicU64,
    pub trades: AtomicU64,
    pub book_tickers: AtomicU64,
    pub klines: AtomicU64,
    /// 🔧 FIX: свечи, опубликованные REST-поллером через hub.publish()
    /// (для futures — ОСНОВНОЙ поток свечей, WS их не отдаёт).
    pub rest_klines: AtomicU64,
    /// ✅ DIAG v3.4.1: публикации при нулевых приёмниках (потеря backfill)
    pub publish_fails: AtomicU64,
    pub order_books: AtomicU64,
    pub reconnects: AtomicU64,
    pub total_silence_seconds: AtomicU64,
    pub latency_sum_ms: AtomicU64,
    pub latency_count: AtomicU64,
}

impl WsHealth {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn parse_rate(&self) -> f64 {
        let r = self.received.load(Ordering::Relaxed);
        if r == 0 {
            return 0.0;
        }
        self.parsed.load(Ordering::Relaxed) as f64 / r as f64
    }

    pub fn avg_latency_ms(&self) -> f64 {
        let c = self.latency_count.load(Ordering::Relaxed);
        if c == 0 {
            return 0.0;
        }
        self.latency_sum_ms.load(Ordering::Relaxed) as f64 / c as f64
    }
}

pub struct WebSocketHub {
    tx: broadcast::Sender<WebSocketMessage>,
    health: Arc<WsHealth>,
    /// ✅ v3.5: кэш последних REST-свечей (страховка прогрева).
    /// Broadcast теряет сообщения, опубликованные при нулевых приёмниках
    /// (старт бота задержан reconciliation-ретраями / рестарт супервизора).
    /// Кэш строго возрастает по timestamp — дубли не проходят.
    kline_cache: Arc<std::sync::Mutex<std::collections::VecDeque<KlineMessage>>>,
    // ✅ P41/W7: handle'ы хранятся и абортятся в Drop — раньше JoinHandle
    // был детачнут (_handle), при пересоздании хабов задачи текли.
    handle: JoinHandle<()>,
    daily_report: JoinHandle<()>,
}

impl Drop for WebSocketHub {
    fn drop(&mut self) {
        // ✅ P41/W7: гасим WS-задачу и суточный репорт вместе с хабом.
        self.handle.abort();
        self.daily_report.abort();
    }
}

/// ✅ v3.5: глубина кэша свечей хаба (250 backfill × 2 + запас живых свечей)
const KLINE_CACHE_CAP: usize = 600;

impl WebSocketHub {
    pub async fn new(ws_base_url: &str, symbol: &str) -> Result<Arc<Self>> {
        let sym = symbol.to_lowercase();

        let is_futures = ws_base_url.contains("fstream")
            || ws_base_url.contains("futures")
            || ws_base_url.contains("binancefuture");

        // ✅ 3г: fstream из нашего региона отдаёт только bookTicker (пробник, 2 сети).
        // Свечи — через REST-поллер.
        let streams = if is_futures {
            format!("{s}@bookTicker", s = sym)
        } else {
            format!(
                "{s}@aggTrade/{s}@bookTicker/{s}@depth20@100ms/{s}@kline_1m",
                s = sym
            )
        };

        tracing::info!(
            "📡 WebSocket Hub | {} | {}",
            if is_futures { "FUTURES" } else { "SPOT" },
            streams
        );

        // v3.7b FIX: Bybit v5 WS does NOT support Binance combined-stream URLs.
        // ws_base_url already holds the correct path (/v5/public/linear);
        // subscription is sent as JSON-op right after connect (see connect_and_process).
        
        // ✅ P25: типизированная биржа вместо to_lowercase().contains(...)
        let ex = Exchange::from_url(ws_base_url);
        let is_bybit = ex == Exchange::Bybit;
        let is_okx = ex == Exchange::Okx;
        let mut endpoints: Vec<String> = if is_bybit || is_okx {
            vec![ws_base_url.trim_end_matches('/').to_string()]
        } else {
            vec![format!(
                "{}/stream?streams={}",
                ws_base_url.trim_end_matches('/'),
                streams
            )]
        };

        // 🔧 ПАТЧ-FALLBACK (23.10, живой лог): Binance-fallback'и имеют смысл
        // ТОЛЬКО для Binance-первичных хабов. Для Bybit-символов (XAUUSDT и
        // т.п.) такого символа на Binance НЕТ — «backup» подключится (HTTP 101,
        // PONG ходит), но данных не отдаст: WS выглядит живым, рынок молчит —
        // маскировка реальной проблемы вместо честного переподключения.
        if !is_futures && ex != Exchange::Bybit { // ✅ P25: семантика прежняя
            for &ep in WS_ENDPOINTS {
                let full = format!("{}/stream?streams={}", ep.trim_end_matches('/'), streams);
                if !endpoints.contains(&full) {
                    endpoints.push(full);
                }
            }
        }

        let (tx, _rx) = broadcast::channel::<WebSocketMessage>(5000);
        let tx_clone = tx.clone();

        // v3.7b: Bybit JSON-op subscription, sent after EVERY (re)connect.
        // kline/orderbook intentionally NOT subscribed: 1m candles in the shared
        // stream could confuse strategies expecting 4h candles from the REST
        // poller; orderbook needs local snapshot+delta merge (v3.8). Live price
        // for DCA comes from tickers (best bid/ask).
        let subscribe_msg: Option<String> = if is_bybit {
            Some(format!(
                "{{\"op\":\"subscribe\",\"args\":[\"tickers.{s}\",\"publicTrade.{s}\"]}}",
                s = symbol.to_uppercase()
            ))
        } else if is_okx {
            let inst = okx_inst_id(&symbol);
            Some(format!(
                "{{\"op\":\"subscribe\",\"args\":[{{\"channel\":\"tickers\",\"instId\":\"{i}\"}},{{\"channel\":\"trades\",\"instId\":\"{i}\"}}]}}",
                i = inst
            ))
        } else {
            None
        };
        let health = WsHealth::new();
        let health_for_task = health.clone();
        let kline_cache = Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::with_capacity(KLINE_CACHE_CAP + 40),
        ));

        for (i, ep) in endpoints.iter().enumerate() {
            tracing::info!("   [{}] {}", i, ep);
        }

        let daily_report = spawn_daily_report(health.clone());

        let handle = tokio::spawn(async move {
            let mut endpoint_idx: usize = 0;
            let mut backoff_sec = 1u64;
            let mut consecutive_failures: u32 = 0;
            let mut last_switch = Instant::now();

            loop {
                let url = &endpoints[endpoint_idx];

                // ============================================================
                // 🔧 ПАТЧ-BACKOFF: connect_and_process возвращает время жизни
                // сессии. Долгая (≥ WS_SESSION_OK_SECS) — успех: backoff в 1с,
                // счётчик сбоев в ноль → после RST переподключение мгновенное.
                // Короткая — считается сбоем (тот же механизм, что и Err,
                // включая переключение endpoint'ов для спота).
                // ============================================================
                match connect_and_process(
                    url,
                    ex, // ✅ P25: биржа определена один раз в WebSocketHub::new
                    subscribe_msg.as_deref(),
                    tx_clone.clone(),
                    health_for_task.clone(),
                )
                .await
                {
                    Ok(lifetime_secs) => {
                        if lifetime_secs >= WS_SESSION_OK_SECS {
                            backoff_sec = 1;
                            consecutive_failures = 0;
                            last_switch = Instant::now();
                            tracing::info!(
                                "🔌 WS-сессия завершена чисто (прожила {}с) — backoff сброшен",
                                lifetime_secs
                            );
                        } else {
                            health_for_task.reconnects.fetch_add(1, Ordering::Relaxed);
                            metrics::WS_RECONNECTS.with_label_values(&["global"]).inc();
                            consecutive_failures += 1;

                            // 🔇 V3.2 шум-гейт: первые 3 сбоя логируем каждый,
                            // дальше каждый 10-й (тестнет может рвать сессии
                            // каждые ~60с — иначе лог разбухает).
                            if consecutive_failures <= 3 || consecutive_failures % 10 == 0 {
                                tracing::warn!(
                                    "🔌 WS-сессия коротка: {}с (сбой {}/{}) | переподключение через {} сек",
                                    lifetime_secs,
                                    consecutive_failures,
                                    RECONNECT_THRESHOLD_FOR_SWITCH,
                                    backoff_sec
                                );
                            }

                            let should_switch =
                                consecutive_failures >= RECONNECT_THRESHOLD_FOR_SWITCH
                                    && endpoints.len() > 1
                                    && last_switch.elapsed() > Duration::from_secs(60);

                            if should_switch {
                                endpoint_idx = (endpoint_idx + 1) % endpoints.len();
                                consecutive_failures = 0;
                                last_switch = Instant::now();
                                backoff_sec = 1;
                                tracing::warn!(
                                    "🔀 Переключение на backup endpoint [{}]: {}",
                                    endpoint_idx,
                                    endpoints[endpoint_idx]
                                );
                            } else {
                                sleep(Duration::from_secs(backoff_sec)).await;
                                backoff_sec = (backoff_sec * 2).min(30);
                            }
                        }
                    }
                    Err(e) => {
                        health_for_task.reconnects.fetch_add(1, Ordering::Relaxed);
                        metrics::WS_RECONNECTS.with_label_values(&["global"]).inc();
                        consecutive_failures += 1;

                        // 🔇 V3.2 шум-гейт: первые 3 фейла логируем каждый,
                        // дальше каждый 10-й. Bybit testnet может отдавать
                        // 504 часами — иначе 2 строки мусора в минуту.
                        if consecutive_failures <= 3 || consecutive_failures % 10 == 0 {
                            tracing::error!(
                                "🔌 Ошибка WS #{} (endpoint {}): {} | переподключение через {} сек",
                                consecutive_failures,
                                endpoint_idx,
                                e,
                                backoff_sec
                            );
                        }

                        let should_switch = consecutive_failures >= RECONNECT_THRESHOLD_FOR_SWITCH
                            && endpoints.len() > 1
                            && last_switch.elapsed() > Duration::from_secs(60);

                        if should_switch {
                            endpoint_idx = (endpoint_idx + 1) % endpoints.len();
                            consecutive_failures = 0;
                            last_switch = Instant::now();
                            backoff_sec = 1;
                            tracing::warn!(
                                "🔀 Переключение на backup endpoint [{}]: {}",
                                endpoint_idx,
                                endpoints[endpoint_idx]
                            );
                        } else {
                            sleep(Duration::from_secs(backoff_sec)).await;
                            backoff_sec = (backoff_sec * 2).min(30);
                        }
                    }
                }
            }
        });

        Ok(Arc::new(Self {
            tx,
            health,
            kline_cache,
            handle,
            daily_report,
        }))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<WebSocketMessage> {
        self.tx.subscribe()
    }

    /// ✅ v3.5: последние REST-свечи из кэша хаба — для гарантированного
    /// прогрева стратегии минуя broadcast (см. main.rs, блок v3.5).
    pub fn recent_klines(&self) -> Vec<KlineMessage> {
        self.kline_cache
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect()
    }

    pub fn health(&self) -> Arc<WsHealth> {
        self.health.clone()
    }

    /// ✅ 3г: инъекция сообщения в канал хаба (для REST-поллера свечей)
    pub fn publish(&self, msg: WebSocketMessage) {
        // 🔧 FIX: REST-свечи считаем отдельно от WS-klines — иначе heartbeat
        // для futures вечно показывает «свечи=0» (fstream kline не отдаёт).
        if let WebSocketMessage::Kline(k) = &msg {
            self.health.rest_klines.fetch_add(1, Ordering::Relaxed);
            // ✅ v3.5: параллельно кладём свечу в кэш хаба. Если приёмников
            // в момент публикации нет, send() ниже потеряет сообщение —
            // кэш позволит добрать историю при старте цикла бота.
            if let Ok(mut cache) = self.kline_cache.lock() {
                if cache.back().map_or(true, |last| k.timestamp > last.timestamp) {
                    cache.push_back(k.clone());
                    while cache.len() > KLINE_CACHE_CAP {
                        cache.pop_front();
                    }
                }
            }
        }
        // ✅ DIAG v3.4.1: раньше ошибка send() глоталась — теперь видно,
        // если свечи публикуются при нулевых приёмниках (потеря backfill).
        if let Err(e) = self.tx.send(msg) {
            let n = self.health.publish_fails.fetch_add(1, Ordering::Relaxed);
            if n < 3 || n % 100 == 0 {
                tracing::warn!(
                    "hub.publish: НЕТ приёмников — сообщение потеряно (потеря #{})",
                    n + 1
                );
                let _ = e; // RecvError без полезной нагрузки
            }
        }
    }
}

/// Возвращает Ok(секунд жизни сессии). Причина разрыва уже залогирована
/// внутри (RST / Close / PONG-dead) — супервизору важна только длительность.
async fn connect_and_process(
    url: &str,
    ex: Exchange, // ✅ P25
    subscribe_msg: Option<&str>,
    tx: broadcast::Sender<WebSocketMessage>,
    health: Arc<WsHealth>,
) -> Result<u64> {
    tracing::info!("Подключаюсь к {}", url);

    let connect_result = timeout(
        Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
        connect_async(url),
    )
    .await;
    let ws = match connect_result {
        Ok(Ok((ws, response))) => {
            tracing::info!("✅ WS ПОДКЛЮЧЁН | HTTP {}", response.status());
            ws
        }
        Ok(Err(e)) => return Err(anyhow!("WS handshake failed: {}", e)),
        Err(_) => {
            return Err(anyhow!(
                "WS handshake timeout ({}s)",
                HANDSHAKE_TIMEOUT_SECS
            ))
        }
    };

    // 🔧 ПАТЧ-BACKOFF: в scope ФУНКЦИИ (не match-arm) — нужен в точке возврата
    let started = Instant::now();

    let (write, mut read) = ws.split();
    let write = Arc::new(Mutex::new(write));

    // ✅ P25: биржа приходит параметром (раньше — эвристика по содержимому subscribe_msg).
    // Паритет: subscribe_msg был Some только для bybit/okx; okx-сообщение содержит instId.
    let is_okx = ex == Exchange::Okx;
    let is_bybit = ex == Exchange::Bybit;
    // v3.7b: cache of last bid/ask for Bybit tickers. Bybit deltas carry only
    // changed fields - without this cache BookTicker would be choppy.
    let mut bt_cache = BybitTickerCache::default();

    if let Some(sub) = subscribe_msg {
        let mut guard = write.lock().await;
        if let Err(e) = guard.send(Message::Text(sub.to_string())).await {
            // ✅ P41/W9: текст ошибки больше не называет биржу Bybit —
            // сообщение выводилось и для OKX (та же ветка кода).
            return Err(anyhow!("subscribe send failed: {}", e));
        }
        tracing::info!("[bybit-ws] subscription sent: {}", sub);
    }

    let mut ping_interval = tokio::time::interval(Duration::from_secs(20));
    // ✅ P41/D16: Skip вместо Burst — после простоя потока не догоняем
    // накопившиеся тики пачкой (Burst-шторм пингов ловился аудитом).
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ping_interval.tick().await;

    let mut heartbeat = tokio::time::interval(Duration::from_secs(60));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip); // ✅ P41/D16
    heartbeat.tick().await;

    let mut last_msg_at = Instant::now();
    let mut last_pong_at = Instant::now();

    loop {
        tokio::select! {

            // ✅ P41/W6: biased удалён — при непрерывном потоке read.next()
            // (первая ветка) голодал ping/heartbeat: сервер рвал сессию по
            // таймауту. tokio без biased опрашивает готовые ветки в случайном
            // порядке — пинги и данные живут одновременно.
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Ping(payload))) => {
                        let mut guard = write.lock().await;
                        if let Err(e) = guard.send(Message::Pong(payload)).await {
                            tracing::error!("Pong send: {}", e);
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {
                        last_pong_at = Instant::now();
                    }
                    Some(Ok(Message::Text(text))) => {
                        health.received.fetch_add(1, Ordering::Relaxed);
                        metrics::WS_MESSAGES_RECEIVED.with_label_values(&["global"]).inc();
                        last_msg_at = Instant::now();

                        // v3.7b: Bybit application pong counts as liveness.
                        if is_bybit && (text.contains("\"op\":\"pong\"") || text.contains("\"op\": \"pong\"")) {
                            last_pong_at = Instant::now();
                            continue;
                        }

                        let parsed_msg = if is_okx {
                            if text.trim().eq_ignore_ascii_case("pong") {
                                last_pong_at = Instant::now();
                                continue;
                            }
                            parse_okx_message(&text)
                        } else if is_bybit {
                            parse_bybit_message(&text, &mut bt_cache)
                        } else {
                            parse_message(&text)
                        };
                        if let Some(m) = parsed_msg {
                            health.parsed.fetch_add(1, Ordering::Relaxed);
                            metrics::WS_MESSAGES_PARSED.with_label_values(&["global"]).inc();

                            match &m {
                                WebSocketMessage::Trade(_) => { health.trades.fetch_add(1, Ordering::Relaxed); }
                                WebSocketMessage::BookTicker(_) => { health.book_tickers.fetch_add(1, Ordering::Relaxed); BT_TOTAL.fetch_add(1, Ordering::Relaxed); }
                                WebSocketMessage::Kline(_) => { health.klines.fetch_add(1, Ordering::Relaxed); }
                                WebSocketMessage::OrderBook(_) => { health.order_books.fetch_add(1, Ordering::Relaxed); }
                            }

                            if let WebSocketMessage::Trade(ref t) = m {
                                let local_ts_ms = chrono::Utc::now().timestamp_millis();
                                let raw_latency = (local_ts_ms - t.event_time).max(0) as u64;
                                if raw_latency < CLOCK_SKEW_THRESHOLD_MS {
                                    health.latency_sum_ms.fetch_add(raw_latency, Ordering::Relaxed);
                                    health.latency_count.fetch_add(1, Ordering::Relaxed);
                                    metrics::WS_LATENCY_MS
                                        .with_label_values(&[ex.as_str()]) // ✅ P25: source = реальная биржа сессии
                                        .observe(raw_latency as f64);
                                }
                            }
                            // ✅ P41/W9: latency и по bookTicker — на futures-хабе
                            // Trade-потока нет вовсе, метрика была всегда пустой.
                            if let WebSocketMessage::BookTicker(ref b) = m {
                                let local_ts_ms = chrono::Utc::now().timestamp_millis();
                                let raw_latency = (local_ts_ms - b.timestamp).max(0) as u64;
                                if raw_latency < CLOCK_SKEW_THRESHOLD_MS {
                                    health.latency_sum_ms.fetch_add(raw_latency, Ordering::Relaxed);
                                    health.latency_count.fetch_add(1, Ordering::Relaxed);
                                    metrics::WS_LATENCY_MS
                                        .with_label_values(&[ex.as_str()])
                                        .observe(raw_latency as f64);
                                }
                            }

                            let _ = tx.send(m);
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        tracing::warn!("Сервер закрыл: {:?}", frame);
                        break;
                    }
                    Some(Err(e)) => {
                        tracing::error!("WS read: {}", e);
                        break;
                    }
                    None => {
                        tracing::warn!("WS поток закончился");
                        break;
                    }
                    _ => {}
                }
            }

            _ = ping_interval.tick() => {
                let mut guard = write.lock().await;
                // v3.7b: Bybit requires APPLICATION-level JSON ping (op:ping)
                // every 20s; protocol RFC-ping stays for Binance.
                let ping_frame = if is_okx {
                    Message::Text("ping".to_string())
                } else if is_bybit {
                    Message::Text("{\"op\":\"ping\"}".to_string())
                } else {
                    Message::Ping(vec![])
                };
                if let Err(e) = guard.send(ping_frame).await {
                    tracing::error!("Ping send: {}", e);
                    break;
                }
            }

            _ = heartbeat.tick() => {
                let rate = health.parse_rate();
                let silence_sec = last_msg_at.elapsed().as_secs();
                let pong_age_sec = last_pong_at.elapsed().as_secs();

                metrics::WS_PARSE_RATE.with_label_values(&["global"]).set(rate);
                metrics::WS_SILENCE_SECONDS.with_label_values(&["global"]).set(silence_sec as f64);

                if pong_age_sec > PONG_DEAD_THRESHOLD_SEC {
                    health.total_silence_seconds.fetch_add(pong_age_sec, Ordering::Relaxed);
                    metrics::WS_TOTAL_SILENCE_SECONDS
                        .with_label_values(&["global"])
                        .inc_by(pong_age_sec as f64);
                    tracing::error!(
                        "💓 PONG не приходил {} сек — соединение мертво, переподключение",
                        pong_age_sec
                    );
                    break;
                }

                if silence_sec > MARKET_DATA_SILENCE_LOG_SEC {
                    tracing::info!(
                        "💓 Тишина в market data {} сек, но PONG был {} сек назад — соединение живо",
                        silence_sec,
                        pong_age_sec
                    );
                }

                // 🔧 FIX: добавлен счётчик свечи_REST — публикации REST-поллера.
                // Для futures это ОСНОВНОЙ поток свечей; «свечи=0» при растущем
                // свечи_REST — норма (WS kline не отдаётся), а не поломка.
                tracing::info!(
                    "💓 heartbeat: получено={} разобрано={} ({:.1}%) | трейды={} книги={} свечи={} свечи_REST={} | тишина={}с | PONG_age={}с | latency≈{:.0}мс",
                    health.received.load(Ordering::Relaxed),
                    health.parsed.load(Ordering::Relaxed),
                    rate * 100.0,
                    health.trades.load(Ordering::Relaxed),
                    health.book_tickers.load(Ordering::Relaxed),
                    health.klines.load(Ordering::Relaxed),
                    health.rest_klines.load(Ordering::Relaxed),
                    silence_sec,
                    pong_age_sec,
                    health.avg_latency_ms()
                );

                if health.received.load(Ordering::Relaxed) > 100 && rate < 0.95 {
                    tracing::warn!("⚠️ Низкий parse rate: {:.1}%", rate * 100.0);
                }
            }
        }
    }

    Ok(started.elapsed().as_secs())
}

fn spawn_daily_report(health: Arc<WsHealth>) -> JoinHandle<()> {
    // ✅ P41/W7: возвращаем JoinHandle — хаб абортит репорт в Drop.
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(24 * 3600));
        // ✅ P41/D16: Skip вместо Burst — после простоя не догоняем
        // накопившиеся тики пачкой.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;

            let received = health.received.load(Ordering::Relaxed);
            let parsed = health.parsed.load(Ordering::Relaxed);
            let rate = if received > 0 { parsed as f64 / received as f64 * 100.0 } else { 0.0 };
            let reconnects = health.reconnects.load(Ordering::Relaxed);
            let silence = health.total_silence_seconds.load(Ordering::Relaxed);
            let latency = health.avg_latency_ms();
            let rest_klines = health.rest_klines.load(Ordering::Relaxed);

            tracing::info!("╔═══════════════════════════════════════════════╗");
            tracing::info!("║   📊 СУТОЧНЫЙ ОТЧЁТ WebSocket                 ║");
            tracing::info!("╠═══════════════════════════════════════════════╣");
            tracing::info!("║  Получено сообщений: {:>15}          ║", received);
            tracing::info!("║  Разобрано:          {:>15} ({:.2}%) ║", parsed, rate);
            tracing::info!("║  Средняя латентность:{:>13.0} мс       ║", latency);
            tracing::info!("║  Переподключений:    {:>15}          ║", reconnects);
            tracing::info!("║  Общее время тишины: {:>13} с          ║", silence);
            tracing::info!("║  Свечей от REST:     {:>15}          ║", rest_klines);
            tracing::info!("╚═══════════════════════════════════════════════╝");

            health.received.store(0, Ordering::Relaxed);
            health.parsed.store(0, Ordering::Relaxed);
            health.reconnects.store(0, Ordering::Relaxed);
            health.total_silence_seconds.store(0, Ordering::Relaxed);
            health.latency_sum_ms.store(0, Ordering::Relaxed);
            health.latency_count.store(0, Ordering::Relaxed);
            health.rest_klines.store(0, Ordering::Relaxed);
        }
    }) // P41/W7: хвост — выражение, не оператор (E0308)
}

// v3.7b: cache of last best bid/ask for converting Bybit tickers
// (snapshot + delta) into full BookTickerMessage values.
#[derive(Default, Clone, Copy)]
struct BybitTickerCache {
    bid: f64,
    ask: f64,
    bid_qty: f64,
    ask_qty: f64,
}

// v3.7b: Bybit v5 envelope parser {"topic":...,"type":...,"ts":...,"data":...}.
// Handles exactly what we subscribe to: tickers.XAUUSDT (best bid/ask) and
// publicTrade.XAUUSDT (trade tape). Everything else is silently ignored.
fn parse_bybit_message(
    text: &str,
    bt_cache: &mut BybitTickerCache,
) -> Option<WebSocketMessage> {
    let root: Value = serde_json::from_str(text).ok()?;
    let topic = root.get("topic")?.as_str()?.to_string();
    let root_ts = root.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
    let data = root.get("data")?.clone();

    if topic.starts_with("tickers.") {
        // data is an object; delta carries only the changed fields.
        let get_f = |key: &str| -> Option<f64> {
            data.get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse::<f64>().ok())
        };
        if let Some(v) = get_f("bid1Price") {
            bt_cache.bid = v;
        }
        if let Some(v) = get_f("ask1Price") {
            bt_cache.ask = v;
        }
        if let Some(v) = get_f("bid1Size") {
            bt_cache.bid_qty = v;
        }
        if let Some(v) = get_f("ask1Size") {
            bt_cache.ask_qty = v;
        }
        if bt_cache.bid <= 0.0 || bt_cache.ask <= 0.0 {
            return None; // one side still unknown (wait for full snapshot)
        }
        let mut timestamp = data.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
        if timestamp <= 0 {
            timestamp = root_ts;
        }
        if timestamp <= 0 {
            timestamp = chrono::Utc::now().timestamp_millis();
        }
        return Some(WebSocketMessage::BookTicker(BookTickerMessage {
            symbol: data
                .get("symbol")
                .and_then(|v| v.as_str())
                .unwrap_or("UNKNOWN")
                .to_string(),
            best_bid: bt_cache.bid,
            best_ask: bt_cache.ask,
            bid_qty: bt_cache.bid_qty,
            ask_qty: bt_cache.ask_qty,
            timestamp,
        }));
    }

    if topic.starts_with("publicTrade.") {
        // data is an array: [{"T":ms,"s":"XAUUSDT","S":"Buy|Sell","v":..,"p":..}]
        let arr = data.as_array()?;
        // ✅ P41/W5: берём последнюю ВАЛИДНУЮ сделку пачки (раньше .last()
        // брал последнюю запись даже с битыми числами) и проверяем
        // конечность/положительность. Остаточное ограничение: из пачки
        // публикуется одна сделка (объём/OFI по пачке занижены) — сигнатура
        // парсера одно-сообщная; в текущем конфиге Bybit-трейды не
        // используются стратегиями (keltner_dca живёт на bookTicker+Kline).
        let t = arr
            .iter()
            .rev()
            .find(|t| {
                t.get("p").and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok())
                    .map(|p| p.is_finite() && p > 0.0)
                    .unwrap_or(false)
                    && t.get("v").and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok())
                        .map(|v| v.is_finite() && v >= 0.0)
                        .unwrap_or(false)
            })?;
        let price = t.get("p")?.as_str()?.parse::<f64>().ok()?;
        let volume = t.get("v")?.as_str()?.parse::<f64>().ok()?;
        let symbol = t.get("s")?.as_str()?.to_string();
        let mut timestamp = t.get("T").and_then(|v| v.as_i64()).unwrap_or(0);
        if timestamp <= 0 {
            timestamp = root_ts;
        }
        let event_time = if root_ts > 0 { root_ts } else { timestamp };
        // Bybit S="Buy" means aggressive buyer (taker buy) -> buyer is NOT maker.
        let is_buyer_maker = t.get("S").and_then(|v| v.as_str()) == Some("Sell");
        return Some(WebSocketMessage::Trade(TradeMessage {
            symbol,
            price,
            volume,
            timestamp,
            event_time,
            is_buyer_maker,
        }));
    }

    None
}

fn parse_message(text: &str) -> Option<WebSocketMessage> {
    let root: Value = serde_json::from_str(text).ok()?;

    let (stream, data) = if let Some(s) = root.get("stream").and_then(|s| s.as_str()) {
        (s.to_string(), root.get("data")?.clone())
    } else {
        (String::new(), root.clone())
    };

    if stream.contains("@aggTrade") {
        let symbol = data.get("s")?.as_str()?.to_string();
        let price = data.get("p")?.as_str()?.parse::<f64>().ok()?;
        let volume = data.get("q")?.as_str()?.parse::<f64>().ok()?;
        let timestamp = data.get("T")?.as_i64().unwrap_or(0);
        let event_time = data.get("E").and_then(|v| v.as_i64()).unwrap_or(timestamp);
        let is_buyer_maker = data.get("m").and_then(|m| m.as_bool()).unwrap_or(false);
        // ✅ P41/W5: парсеры валидируют конечность/положительность —
        // NaN/inf/отрицательные значения больше не доходят до стратегий.
        if !price.is_finite() || price <= 0.0 || !volume.is_finite() || volume < 0.0 {
            return None;
        }
        return Some(WebSocketMessage::Trade(TradeMessage {
            symbol, price, volume, timestamp, event_time, is_buyer_maker,
        }));
    }

    if stream.contains("@bookTicker") {
        let symbol = data.get("s")?.as_str()?.to_string();
        let best_bid = data.get("b")?.as_str()?.parse::<f64>().ok()?;
        let best_ask = data.get("a")?.as_str()?.parse::<f64>().ok()?;
        let bid_qty = data.get("B").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let ask_qty = data.get("A").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let timestamp = data
            .get("E")
            .and_then(|v| v.as_i64())
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        // ✅ P41/W5: конечность, положительность и bid<=ask — перекрёстный
        // стакан/NaN не должен становиться «ценой» позиции (StateAgent
        // проверял только >0).
        if !best_bid.is_finite() || !best_ask.is_finite() || best_bid <= 0.0 || best_ask < best_bid {
            return None;
        }
        return Some(WebSocketMessage::BookTicker(BookTickerMessage {
            symbol, best_bid, best_ask, bid_qty, ask_qty, timestamp,
        }));
    }

    if stream.contains("@depth") {
        let symbol = data.get("s").and_then(|s| s.as_str()).unwrap_or("").to_string();
        let bids = parse_levels(data.get("bids")?)?;
        let asks = parse_levels(data.get("asks")?)?;
        return Some(WebSocketMessage::OrderBook(OrderBookMessage { symbol, bids, asks }));
    }

    if stream.contains("@kline") {
        let k = data.get("k")?;
        let symbol = k.get("s")?.as_str()?.to_string();
        let open = k.get("o")?.as_str()?.parse::<f64>().ok()?;
        let high = k.get("h")?.as_str()?.parse::<f64>().ok()?;
        let low = k.get("l")?.as_str()?.parse::<f64>().ok()?;
        let close = k.get("c")?.as_str()?.parse::<f64>().ok()?;
        let volume = k.get("v")?.as_str()?.parse::<f64>().ok()?;
        // ✅ taker buy base asset volume — k.V
        let taker_buy_volume = k
            .get("V")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        // ✅ close_time — k.T
        // ✅ P41/W5: timestamp=0 ломал дедуп по last_kline_ts — fallback на
        // локальное время (как у bookTicker).
        let timestamp = k
            .get("T")
            .and_then(|v| v.as_i64())
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let is_closed = k.get("x").and_then(|v| v.as_bool()).unwrap_or(false);
        // ✅ P41/W5: OHLCV-валидация (конечность/положительность, high>=low).
        if !(open.is_finite() && high.is_finite() && low.is_finite() && close.is_finite()
            && volume.is_finite())
            || open <= 0.0
            || high <= 0.0
            || low <= 0.0
            || close <= 0.0
            || high < low
        {
            return None;
        }
        return Some(WebSocketMessage::Kline(KlineMessage {
            symbol, open, high, low, close, volume, taker_buy_volume, timestamp, is_closed,
            is_backfill: false,
        }));
    }

    None
}

fn parse_levels(v: &Value) -> Option<Vec<(f64, f64)>> {
    Some(
        v.as_array()?
            .iter()
            .filter_map(|level| {
                let arr = level.as_array()?;
                let price = arr.first()?.as_str()?.parse::<f64>().ok()?;
                let qty = arr.get(1)?.as_str()?.parse::<f64>().ok()?;
                Some((price, qty))
            })
            .collect(),
    )
}

pub struct WebSocketConnection;
pub async fn create_connection(
    _ws_base_url: &str,
    _symbol: &str,
    _bot_name: &str,
) -> Result<(tokio::sync::mpsc::Receiver<WebSocketMessage>, JoinHandle<()>)> {
    Err(anyhow!("create_connection устарел — используйте WebSocketHub"))
}

// v4.0: OKX v5 envelope {"arg":{"channel":...,"instId":...},"data":[...]}.
// tickers = full snapshot (bidPx/askPx/bidSz/askSz); trades = tape.
fn parse_okx_message(text: &str) -> Option<WebSocketMessage> {
    let root: serde_json::Value = serde_json::from_str(text).ok()?;
    let channel = root.get("arg")?.get("channel")?.as_str()?.to_string();
    let data = root.get("data")?.clone();

    if channel == "tickers" {
        let t = data.as_array()?.first()?;
        let bid = t.get("bidPx").and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok());
        let ask = t.get("askPx").and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok());
        let (bid, ask) = match (bid, ask) {
            (Some(b), Some(a)) if b > 0.0 && a > 0.0 => (b, a),
            _ => return None, // one side absent - wait for next snapshot
        };
        let bid_qty = t.get("bidSz").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let ask_qty = t.get("askSz").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let timestamp = t
            .get("ts")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let symbol = okx_symbol_from_inst_id(t.get("instId").and_then(|v| v.as_str()).unwrap_or(""));
        return Some(WebSocketMessage::BookTicker(BookTickerMessage {
            symbol,
            best_bid: bid,
            best_ask: ask,
            bid_qty,
            ask_qty,
            timestamp,
        }));
    }

    if channel == "trades" {
        let t = data.as_array()?.last()?;
        let price = t.get("px").and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok())?;
        let volume = t.get("sz").and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok())?;
        let ts = t.get("ts").and_then(|v| v.as_str()).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        let symbol = okx_symbol_from_inst_id(t.get("instId").and_then(|v| v.as_str()).unwrap_or(""));
        let is_buyer_maker = t.get("side").and_then(|v| v.as_str()) == Some("sell");
        return Some(WebSocketMessage::Trade(TradeMessage {
            symbol,
            price,
            volume,
            timestamp: ts,
            event_time: ts,
            is_buyer_maker,
        }));
    }

    None
}

// v4.0: "XAU-USDT-SWAP" -> "XAUUSDT" (engine canonical symbol)
fn okx_symbol_from_inst_id(inst_id: &str) -> String {
    let s = inst_id.trim().to_uppercase();
    if let Some(base) = s.strip_suffix("-USDT-SWAP") {
        format!("{}USDT", base)
    } else if let Some(base) = s.strip_suffix("-SWAP") {
        base.to_string()
    } else {
        s
    }
}

// v4.0: "XAUUSDT" -> "XAU-USDT-SWAP"
fn okx_inst_id(symbol: &str) -> String {
    let s = symbol.trim().to_uppercase();
    if s.contains('-') {
        if s.ends_with("-SWAP") { s } else { format!("{}-SWAP", s) }
    } else if let Some(base) = s.strip_suffix("USDT") {
        format!("{}-USDT-SWAP", base)
    } else {
        s
    }
}

// ============================================================
// ✅ P41/W5: юнит-тесты парсеров — конечность/положительность/bid<=ask,
// kline-timestamp fallback. Приватные фн доступны изнутри модуля.
// ============================================================
#[cfg(test)]
mod p41_parser_tests {
    use super::*;

    #[test]
    fn bookticker_valid_passes() {
        let m = parse_message(
            r#"{"stream":"taousdt@bookTicker","data":{"s":"TAOUSDT","b":"100.5","a":"100.6","B":"1","A":"2","E":1700000000000}}"#,
        );
        assert!(matches!(m, Some(WebSocketMessage::BookTicker(_))));
    }

    #[test]
    fn bookticker_crossed_book_rejected() {
        let m = parse_message(
            r#"{"stream":"taousdt@bookTicker","data":{"s":"TAOUSDT","b":"100.7","a":"100.6","E":1700000000000}}"#,
        );
        assert!(m.is_none(), "перекрёстный стакан должен отбрасываться (P41/W5)");
    }

    #[test]
    fn aggtrade_nan_price_rejected() {
        let m = parse_message(
            r#"{"stream":"taousdt@aggTrade","data":{"s":"TAOUSDT","p":"NaN","q":"1","T":1700000000000,"m":false}}"#,
        );
        assert!(m.is_none(), "NaN-цена не должна доходить до стратегий");
    }

    #[test]
    fn kline_missing_timestamp_falls_back_to_now() {
        let m = parse_message(
            r#"{"stream":"taousdt@kline_1m","data":{"k":{"s":"TAOUSDT","o":"1","h":"2","l":"0.5","c":"1.5","v":"10","x":true}}}"#,
        );
        match m {
            Some(WebSocketMessage::Kline(k)) => {
                assert!(k.timestamp > 1_600_000_000_000, "ts=0 больше недопустим (P41/W5)");
            }
            _ => panic!("валидная свеча должна парситься"),
        }
    }

    #[test]
    fn kline_high_below_low_rejected() {
        let m = parse_message(
            r#"{"stream":"taousdt@kline_1m","data":{"k":{"s":"TAOUSDT","o":"1","h":"0.5","l":"2","c":"1.5","v":"10","T":1700000000000,"x":true}}}"#,
        );
        assert!(m.is_none(), "high<low — битая свеча");
    }

    #[test]
    fn bybit_trade_invalid_entries_skipped_last_valid_taken() {
        let msg = r#"{"topic":"publicTrade.XAUUSDT","ts":1700000000000,"data":[{"T":1700000000001,"s":"XAUUSDT","S":"Buy","v":"-5","p":"0"},{"T":1700000000002,"s":"XAUUSDT","S":"Buy","v":"2","p":"2000.5"}]}"#;
        let m = parse_bybit_message(msg, &mut BybitTickerCache::default());
        match m {
            Some(WebSocketMessage::Trade(t)) => {
                assert!((t.price - 2000.5).abs() < 1e-9, "берётся последняя ВАЛИДНАЯ сделка пачки");
                assert!(t.volume >= 0.0);
            }
            _ => panic!("bybit-сделка должна парситься"),
        }
    }
}
