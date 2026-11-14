use once_cell::sync::Lazy;
use prometheus::{
    Encoder, GaugeVec, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts,
    Registry, TextEncoder, CounterVec,
};

pub static REGISTRY: Lazy<Registry> = Lazy::new(|| Registry::new());

pub static MESSAGES_TOTAL: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(Opts::new("bot_messages_total", "Total WS messages"), &["bot"]).unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

pub static SIGNALS_TOTAL: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(Opts::new("bot_signals_total", "Signals generated"), &["bot", "side"]).unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

pub static SIGNALS_APPROVED: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(Opts::new("bot_signals_approved", "Signals approved"), &["bot"]).unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

pub static SIGNALS_REJECTED: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(Opts::new("bot_signals_rejected", "Signals rejected"), &["bot"]).unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

pub static ORDERS_PLACED: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(Opts::new("bot_orders_placed", "Orders placed"), &["bot"]).unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

pub static TRADES_CLOSED: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(Opts::new("bot_trades_closed", "Trades closed"), &["bot", "result"]).unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

/// ✅ 6 (В-31): Gauge вместо Counter — Counter не умеет уменьшаться,
/// и убытки были невидимы метрикам (смоук: −7.29 не отразился бы).
pub static PNL_TOTAL: Lazy<GaugeVec> = Lazy::new(|| {
    let g = GaugeVec::new(
        Opts::new("bot_pnl_total", "Cumulative PnL (может быть отрицательным)"),
        &["bot"],
    ).unwrap();
    REGISTRY.register(Box::new(g.clone())).unwrap();
    g
});

pub static WIN_RATE: Lazy<GaugeVec> = Lazy::new(|| {
    let c = GaugeVec::new(Opts::new("bot_win_rate", "Win rate"), &["bot"]).unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

pub static PROFIT_FACTOR: Lazy<GaugeVec> = Lazy::new(|| {
    let c = GaugeVec::new(Opts::new("bot_profit_factor", "Profit factor"), &["bot"]).unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

pub static BOTS_ACTIVE: Lazy<IntGaugeVec> = Lazy::new(|| {
    let c = IntGaugeVec::new(Opts::new("bot_active", "Bot active"), &["bot"]).unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

pub static ERRORS_TOTAL: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(Opts::new("bot_errors_total", "Errors"), &["bot"]).unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

pub static MARKET_REGIME: Lazy<GaugeVec> = Lazy::new(|| {
    let c = GaugeVec::new(Opts::new("bot_market_regime", "Current regime"), &["bot", "regime"]).unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

// ============================================================
// ✅ НОВЫЕ МЕТРИКИ WebSocket HEALTH (правки 5, 7, 8)
// ============================================================

/// Всего получено сообщений от WS (до парсинга)
pub static WS_MESSAGES_RECEIVED: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(
        Opts::new("ws_messages_received_total", "WS messages received (raw)"),
        &["source"],
    ).unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

/// Всего успешно распарсено сообщений
pub static WS_MESSAGES_PARSED: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(
        Opts::new("ws_messages_parsed_total", "WS messages parsed successfully"),
        &["source"],
    ).unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

/// Текущий parse rate (0..1)
pub static WS_PARSE_RATE: Lazy<GaugeVec> = Lazy::new(|| {
    let c = GaugeVec::new(
        Opts::new("ws_parse_rate", "WS parse rate (0..1)"),
        &["source"],
    ).unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

/// Секунд с последнего сообщения — детектирует тишину
pub static WS_SILENCE_SECONDS: Lazy<GaugeVec> = Lazy::new(|| {
    let c = GaugeVec::new(
        Opts::new("ws_silence_seconds", "Seconds since last WS message"),
        &["source"],
    ).unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

/// Счётчик переподключений WS
pub static WS_RECONNECTS: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(
        Opts::new("ws_reconnects_total", "WS reconnect count"),
        &["source"],
    ).unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

/// Латентность Binance → бот (мс)
pub static WS_LATENCY_MS: Lazy<HistogramVec> = Lazy::new(|| {
    let h = HistogramVec::new(
        HistogramOpts::new("ws_latency_ms", "WS message latency in ms")
            .buckets(vec![5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 5000.0]),
        &["source"],
    ).unwrap();
    REGISTRY.register(Box::new(h.clone())).unwrap();
    h
});

/// Суммарное время тишины WS (секунды)
pub static WS_TOTAL_SILENCE_SECONDS: Lazy<CounterVec> = Lazy::new(|| {
    let c = CounterVec::new(
        Opts::new("ws_total_silence_seconds", "Total silence seconds"),
        &["source"],
    ).unwrap();
    REGISTRY.register(Box::new(c.clone())).unwrap();
    c
});

/// ✅ bk9: флаг авто-стоп по EV (1 = suspended, 0 = торгуем)
pub static SUSPENDED: Lazy<IntGaugeVec> = Lazy::new(|| {
    let g = IntGaugeVec::new(
        Opts::new("bot_suspended", "Bot suspended due to negative EV (1=yes, 0=no)"),
        &["bot"],
    ).unwrap();
    REGISTRY.register(Box::new(g.clone())).unwrap();
    g
});

/// ✅ Ф4: источник подтверждений исполнений: 1 = user-data stream,
/// 0 = REST-поллинг (fallback). Дашборд показывает бейдж UDS/ПОЛЛИНГ.
pub static UDS_ACTIVE: Lazy<IntGaugeVec> = Lazy::new(|| {
    let g = IntGaugeVec::new(
        Opts::new("bot_uds_active", "User-data stream active (1=yes, 0=polling)"),
        &["bot"],
    ).unwrap();
    REGISTRY.register(Box::new(g.clone())).unwrap();
    g
});

// ============================================================

pub fn spawn_metrics_server() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("0.0.0.0:9090").await.unwrap();
        tracing::info!("Metrics server on 0.0.0.0:9090");
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buffer).await;
            let mut body = Vec::new();
            let encoder = TextEncoder::new();
            let metric_families = REGISTRY.gather();
            encoder.encode(&metric_families, &mut body).unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
        }
    })
}