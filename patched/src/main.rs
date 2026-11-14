#![allow(async_fn_in_trait)]
#![allow(dead_code)]

use anyhow::Result;
use rust_decimal::prelude::ToPrimitive;
use std::collections::HashMap;
use std::sync::Arc;
use tracing_subscriber::{fmt, EnvFilter};

mod agents;
mod config;
mod exchange;
mod execution;
mod metrics;
mod risk;
mod state;
mod strategies;
mod utils;
mod webui;
mod websocket;

/// ✅ P53/H9: событие «БИРЖА закрыла позицию» (филл STOP_MARKET/TAKE_PROFIT_MARKET)
/// — от смотрителя (user-stream) в главный цикл бота, где живёт риск-учёт.
#[derive(Debug, Clone)]
struct ExchangeClose {
    symbol: String,
    pnl: f64,
    exit_price: f64,
    qty: f64,
    /// STOP_MARKET / TAKE_PROFIT_MARKET — что именно исполнила биржа.
    kind: String,
    /// PnL от самой биржи (поле rp) — кросс-чек расчёта по state.
    exchange_pnl: f64,
}

fn init_logger() -> Result<()> {
    use tracing_subscriber::prelude::*;

    let file_appender = tracing_appender::rolling::daily("logs", "bot.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    std::mem::forget(guard);

    let console_layer = fmt::layer()
        .with_target(true)
        .with_writer(std::io::stdout);

    let file_layer = fmt::layer()
        .with_target(true)
        .with_ansi(false)
        .with_writer(non_blocking);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,scalping_agents=debug"));

    tracing_subscriber::registry()
        .with(filter)
        .with(console_layer)
        .with(file_layer)
        .init();

    Ok(())
}

/// ✅ Стартовая устойчивость: тестнет флаковый, а сеть после логона Windows
/// поднимается через десятки секунд — автозагрузка стартует бота раньше сети.
async fn load_filters_retrying(
    url: &str,
    is_futures: bool,
    symbols: &[String],
    max_attempts: u32,
) -> Result<Arc<exchange::FilterRegistry>> {
    let mut last_err = None;
    for attempt in 1..=max_attempts {
        match exchange::FilterRegistry::load(url, is_futures, symbols).await {
            Ok(r) => return Ok(Arc::new(r)),
            Err(e) => {
                tracing::warn!(
                    "📊 фильтры ({}): цикл {}/{} не удался: {}",
                    url, attempt, max_attempts, e
                );
                last_err = Some(e);
                if attempt < max_attempts {
                    let pause = (attempt as u64).min(6) * 10; // 10,20,...,60 сек
                    tracing::warn!("📊 фильтры: повтор через {} с", pause);
                    tokio::time::sleep(std::time::Duration::from_secs(pause)).await;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("фильтры не загружены")))
}

/// ✅ V3.3: UTF-8 в консоли Windows изнутри бота — костыли `chcp 65001`
/// и `[Console]::OutputEncoding` в PowerShell больше не нужны.
/// Кодовая страница — свойство самой КОНСОЛИ, её может менять любой
/// процесс этой консоли, включая наш.
#[cfg(windows)]
fn setup_windows_console() {
    use windows_sys::Win32::System::Console::{SetConsoleCP, SetConsoleOutputCP};
    unsafe {
        SetConsoleOutputCP(65001); // вывод: эмодзи + кириллица без кракозябр
        SetConsoleCP(65001); // ввод (на случай интерактива)
    }
}

#[cfg(not(windows))]
fn setup_windows_console() {}

#[tokio::main]
// P64/E5b: процессный boundary — любой Err из real_main классифицируется
// AppError (utils/app_error.rs) и превращается в канонический exit-код:
// 1 = фатальная (конфиг/ключи — рестарт бессмыслен), 2 = временная
// (рестарт с backoff), 3 = watchdog stall. supervisor.ps1 (P64) читает
// эти коды; паники дают 101 (дефолт runtime) — тоже рестарт.
async fn main() -> std::process::ExitCode {
    match real_main().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // P65/E5c: downcast мигрированных AppError (config/allow_live -> exit 1)
            let app_err = utils::app_error::classify(e);
            tracing::error!(
                "фатальная ошибка: {} | exit={} (supervisor: {})",
                app_err,
                app_err.exit_code(),
                if app_err.is_fatal() { "НЕ рестартовать" } else { "рестарт с backoff" }
            );
            std::process::ExitCode::from(app_err.exit_code() as u8)
        }
    }
}

async fn real_main() -> Result<()> {
    setup_windows_console();
    dotenvy::dotenv().ok();
    init_logger()?;

    // ✅ 4а (К-2): паника любого таска пишется в лог
    let default_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("💥 PANIC: {}", info);
        default_panic_hook(info);
    }));
tracing::info!("🚀 AVRORA v0.3.0 | multi-agent");

    // P65/E5c: конфиг = фатальный класс (exit 1 — supervisor НЕ рестартует)
    let config = config::AppConfig::load_from("config.toml")
        .map_err(|e| utils::app_error::AppError::Config(format!("config.toml: {e}")))?;
    tracing::info!("✅ Конфигурация загружена | ботов: {}", config.bots.len());

    // ✅ P37/K4: явный барьер PROD-торговли (дефолт allow_live=false).
    // "testnet" в имени хоста — не защита, а совпадение; теперь live требует
    // ЯВНОГО подтверждения владельца в конфиге.
    if !config.global.allow_live {
        for bot in &config.bots {
            let url = bot
                .rest_url_override
                .as_ref()
                .unwrap_or(&config.global.rest_url);
            let is_test = url.contains("testnet")
                || url.contains("binance.vision")
                || url.contains("demo");
            if !is_test {
                // P65/E5c: allow_live-барьер = конфиг-класс (exit 1)
                return Err(utils::app_error::AppError::Config(format!(
                    "[{}] REST URL '{}' не выглядит как testnet/demo, а global.allow_live=false.                      Для PROD-торговли задайте явно allow_live = true в [global] config.toml",
                    bot.name, url
                ))
                .into());
            }
        }
    }

    let global = config.global.clone();
    let global_symbol = global.symbol.clone();

    // ============================================================
    // HFT-канон: один TimeSync на процесс
    // ============================================================
    let time_sync = exchange::create_time_sync();

    // ✅ П1: ретрай первичного sync_time
    if let Some(first_bot) = config.bots.first() {
        if let Ok(probe) = exchange::create_client(&global, first_bot, time_sync.clone()) {
            let mut synced = false;
            for attempt in 1..=5u32 {
                match probe.sync_time().await {
                    Ok(()) => { synced = true; break; }
                    Err(e) => {
                        tracing::warn!("⏰ sync_time попытка {}/5: {}", attempt, e);
                        tokio::time::sleep(std::time::Duration::from_secs(2 * attempt as u64)).await;
                    }
                }
            }
            if !synced {
                tracing::error!("⏰ sync_time не удался — возможны отказы -1021. Продолжаю.");
            }
        }
    }

    // ============================================================
    // ✅ Фаза 2: единый источник фильтров символов (exchangeInfo)
    // ============================================================
    let mut spot_symbols: Vec<String> = Vec::new();
    let mut binance_fut_symbols: Vec<String> = Vec::new();
    let mut bybit_fut_symbols: Vec<String> = Vec::new();
    let mut okx_fut_symbols: Vec<String> = Vec::new();
    for bot in &config.bots {
        let sym = bot.symbol.clone().unwrap_or_else(|| global_symbol.clone());
        if bot.market_type != "futures" {
            if !spot_symbols.contains(&sym) { spot_symbols.push(sym); }
            continue;
        }
        let url = bot.rest_url_override.as_ref().unwrap_or(&global.rest_url);
        // ✅ P25: enum Exchange вместо строковых проверок URL.
        // ✅ P41/W9: from_url приоритетнее global.exchange — URL без маркера
        // биржи молча давал Binance, даже когда global.exchange="bybit"/"okx".
        // Приоритет: маркер в URL → global.exchange → Binance.
        match exchange::Exchange::detect(url, &global.exchange)
            .unwrap_or(exchange::Exchange::Binance)
        {
            exchange::Exchange::Bybit => {
                if !bybit_fut_symbols.contains(&sym) { bybit_fut_symbols.push(sym); }
            }
            exchange::Exchange::Okx => {
                if !okx_fut_symbols.contains(&sym) { okx_fut_symbols.push(sym); }
            }
            exchange::Exchange::Binance => {
                if !binance_fut_symbols.contains(&sym) { binance_fut_symbols.push(sym); }
            }
        }
    }

    let spot_filters = if spot_symbols.is_empty() { None } else {
        Some(load_filters_retrying(&global.rest_url, false, &spot_symbols, 6).await?)
    };
    let binance_fut_filters = if binance_fut_symbols.is_empty() { None } else {
        Some(load_filters_retrying("https://fapi.binance.com", true, &binance_fut_symbols, 6).await?)
    };
    // v4.0: OKX instruments via public REST (same host for demo and prod)
    let okx_fut_filters = if okx_fut_symbols.is_empty() { None } else {
        let okx_rest = config.bots.iter()
            // ✅ P25: enum Exchange вместо строковой проверки URL
            .find(|b| b.rest_url_override.as_ref().map(|u| exchange::Exchange::from_url(u) == exchange::Exchange::Okx).unwrap_or(false))
            .map(|b| b.rest_url_override.clone().unwrap_or_default())
            .unwrap_or_else(|| global.rest_url.clone());
        Some(Arc::new(exchange::FilterRegistry::load_okx(&okx_rest, &okx_fut_symbols).await?))
    };
    let bybit_fut_filters = if bybit_fut_symbols.is_empty() { None } else {
        Some(Arc::new(exchange::FilterRegistry::load_bybit("https://api.bybit.com", &bybit_fut_symbols).await?))
    };

    // ============================================================
    // WebSocket Hubs
    // ============================================================
    let mut hubs: HashMap<String, Arc<websocket::WebSocketHub>> = HashMap::new();

    // ✅ v3.5: COMMON-хаб создаём ТОЛЬКО если есть хотя бы один не-изолированный
    // бот. Раньше создавался всегда: при полностью изолированном портфеле это
    // лишнее WS-соединение (в логе 12:50 — двойной хаб для TAOUSDT).
    let has_common_bots = config.bots.iter().any(|b| !b.isolated);
    if has_common_bots {
        let spot_hub = websocket::WebSocketHub::new(&global.ws_url, &global_symbol).await?;
        tracing::info!("📡 [COMMON] WebSocket Hub для {} (spot) запущен", global_symbol);
        hubs.insert(format!("common:{}", global_symbol), spot_hub);
    } else {
        tracing::info!("📡 [COMMON] хаб не нужен — все боты изолированы (минус одно WS-соединение)");
    }

    for bot in &config.bots {
        if !bot.isolated {
            continue;
        }
        let sym = bot.symbol.as_ref().unwrap_or(&global_symbol).clone();
        let ws_url = bot.ws_url_override.as_ref().unwrap_or(&global.ws_url).clone();

        let key = format!("{}:{}", bot.name, sym);
        if hubs.contains_key(&key) {
            continue;
        }

        let hub = websocket::WebSocketHub::new(&ws_url, &sym).await?;
        // ✅ v3.5: честный ярлык рынка — testnet.binancefuture.com НЕ содержит
        // "fstream", из-за чего futures-хабы метились как "(spot)".
        // ✅ P25: bybit/okx — через enum Exchange (семантика прежняя),
        // fstream/binancefuture — это признак futures-хоста Binance, остаётся строкой.
        let market_label = if ws_url.contains("fstream")
            || ws_url.contains("binancefuture")
            || exchange::Exchange::from_url(&ws_url) != exchange::Exchange::Binance
        {
            "futures"
        } else {
            "spot"
        };
        tracing::info!(
            "📡 [ISOLATED] WebSocket Hub для {} ({}) запущен",
            sym,
            market_label
        );
        hubs.insert(key, hub);
    }

    // Broadcast-канал для shutdown
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(4);

    // ✅ WebUI: дашборд + API + Prometheus на одном порту
    // ✅ v4: + api_key/api_secret (для запроса баланса, файл webui/mod.rs)
    let webui_bots: Vec<webui::BotInfo> = config.bots.iter().map(|b| webui::BotInfo {
        name: b.name.clone(),
        symbol: b.symbol.clone().unwrap_or_else(|| global_symbol.clone()),
        strategy: b.strategy.name.clone(),
        market: b.market_type.clone(),
        simulate: b.simulate,
        interval: b.kline_interval.clone(),
        db_path: b.db_path_override.clone().unwrap_or_else(|| global.db_path.clone()),
        rest_url: b.rest_url_override.clone().unwrap_or_else(|| global.rest_url.clone()),
        // ✅ P40/K1: в дашборд-снапшот ключи проходят через expose()
        // (BotInfo хранит String, наружу через API не отдаётся)
        api_key: b.api_key.expose().to_string(),
        api_secret: b.api_secret.expose().to_string(),
        // ✅ P41/UI2: дневной лимит для колец рисков дашборда
        max_daily_loss: b.max_daily_loss_amount.to_f64().unwrap_or(0.0),
    }).collect();
    let metrics_handle = webui::spawn(webui_bots, global.db_path.clone());
    // [P11:tg] Telegram alerts poller (journal CLOSEs + suspensions; env-gated)
    {
        let poller_bots: Vec<(String, String)> = config
            .bots
            .iter()
            .map(|b| {
                (
                    b.name.clone(),
                    b.db_path_override
                        .clone()
                        .unwrap_or_else(|| global.db_path.clone()),
                )
            })
            .collect();
        crate::utils::telegram::spawn_poller(poller_bots, global.db_path.clone());
    }
    tracing::info!("📊 Дашборд: http://localhost:9090");

    // ✅ 4б (К-1): ОДИН риск-менеджер на весь процесс
    // ✅ B2: rate-limiter — глобальный статик (utils::rate_limiter::LIMITER),
    // доступен из kline_poller напрямую, протягивать параметр не нужно.
    let global_risk = Arc::new(risk::GlobalRiskManager::new());
    // PATCH P15: portfolio equity base + restore today daily PnL
    // ✅ P54/HALT-1: база портфельного стопа = account_capital_usdt (K6) —
    // ТОТ ЖЕ base, что у preflight check_k6.ps1. Раньше база = сумма
    // initial_balance ботов: лимит 5% считался от неправильного основания
    // и никогда не срабатывал — стоп был «мягким» фактически (warn-only).
    // Приоритет: [global].account_capital_usdt → env P15_EQUITY_OVERRIDE →
    // сумма initial_balance (старое поведение).
    let p15_equity: f64 = config
        .global
        .account_capital_usdt
        .and_then(|d| d.to_f64())
        .filter(|v| *v > 0.0)
        .or_else(|| {
            std::env::var("P15_EQUITY_OVERRIDE")
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| *v > 0.0)
        })
        .unwrap_or_else(|| {
            config.bots.iter().map(|b| b.initial_balance.to_f64().unwrap_or(0.0)).sum()
        });
    global_risk.set_equity_base(p15_equity);
    global_risk.load_persisted_day();
    tracing::info!(
        "[P54/HALT] GlobalRisk: equity_base={:.2} (K6 base) | HARD HALT при -{:.1}% = -{:.2} USDT | kill_file=KILL",
        p15_equity,
        global_risk.stop_pct(),
        p15_equity * global_risk.stop_pct() / 100.0
    );

    let mut handles = Vec::new();
    for bot_config in config.bots {
        let bot_name = bot_config.name.clone();
        let is_isolated = bot_config.isolated;

        let symbol = bot_config
            .symbol
            .clone()
            .unwrap_or_else(|| global_symbol.clone());

        let global_cfg = global.clone();
        let ts = time_sync.clone();
        let shutdown_tx_bot = shutdown_tx.clone();
        let global_risk_bot = global_risk.clone();

        let hub_key = if is_isolated {
            format!("{}:{}", bot_name, symbol)
        } else {
            format!("common:{}", global_symbol)
        };

        let hub = hubs
            .get(&hub_key)
            .ok_or_else(|| anyhow::anyhow!("Hub не найден: {}", hub_key))?
            .clone();

        tracing::info!(
            "[{}] Символ: {} | Hub: {} | изолирован: {}",
            bot_name, symbol, hub_key, is_isolated
        );

        let bot_rest_url = bot_config.rest_url_override.as_ref().unwrap_or(&global.rest_url).clone();
        // ✅ P25: enum Exchange вместо строковых проверок URL
        // ✅ P41/W9: см. комментарий выше — URL → global.exchange → Binance.
        let filters = if bot_config.market_type == "futures" {
            match exchange::Exchange::detect(&bot_rest_url, &global.exchange)
                .unwrap_or(exchange::Exchange::Binance)
            {
                exchange::Exchange::Bybit => bybit_fut_filters.clone(),
                exchange::Exchange::Okx => okx_fut_filters.clone(),
                exchange::Exchange::Binance => binance_fut_filters.clone(),
            }
        } else {
            spot_filters.clone()
        };

        // ============================================================
        // ✅ 4а (К-2): СУПЕРВИЗОР
        // ============================================================
        let handle = tokio::spawn(async move {
            const MAX_ATTEMPTS: u32 = u32::MAX; // PATCH P15: unlimited restarts
            // AUDIT-P6: сторож супервизора. Раньше цикл был 1..=MAX_ATTEMPTS:
            // после 3-й попытки супервизор умирал (return) до конца процесса —
            // бот мёртв, открытые позиции без exit-логики/трейлинга. Теперь
            // после 3 быстрых попыток — вечный slow-retry каждые 600 сек
            // (тяжёлый startup run_bot не спамим), transient-сбои биржи/WS
            // само-восстанавливаются без ручного вмешательства.
            const WATCHDOG_RETRY_SEC: u64 = 600;
            for attempt in 1..=u32::MAX {
                let ws_rx = hub.subscribe();
                let shutdown_rx = shutdown_tx_bot.subscribe();

                let inner = tokio::spawn(run_bot(
                    bot_config.clone(),
                    symbol.clone(),
                    global_cfg.clone(),
                    ws_rx,
                    ts.clone(),
                    shutdown_rx,
                    filters.clone(),
                    hub.clone(),
                    global_risk_bot.clone(),
                ));

                match inner.await {
                    Ok(Ok(())) => return,
                    Ok(Err(e)) => {
                        tracing::error!(
                            "[{}] ❌ бот упал (попытка {}/{}): {:?}",
                            bot_name, attempt.min(MAX_ATTEMPTS), MAX_ATTEMPTS, e // AUDIT-P6: cap для slow-retry
                        );
                    }
                    Err(join_err) => {
                        tracing::error!(
                            "[{}] 💥 задача бота завершена аварийно (паника: {}, попытка {}/{})",
                            bot_name, join_err.is_panic(), attempt, MAX_ATTEMPTS
                        );
                    }
                }

                metrics::ERRORS_TOTAL.with_label_values(&[&bot_name]).inc();
                metrics::BOTS_ACTIVE.with_label_values(&[&bot_name]).set(0);

                if attempt == MAX_ATTEMPTS { // AUDIT-P6: сторож — return убран, цикл вечный
                    tracing::error!(
                        "[{}] 🚨 быстрые попытки ({}) исчерпаны — включаю СТОРОЖ: \
                         retry каждые {} сек до успеха. ПОКА БОТ МЁРТВ — \
                         ОТКРЫТЫЕ ПОЗИЦИИ БЕЗ ПРИСМОТРА, СВЕРЬ С БИРЖЕЙ",
                        bot_name, MAX_ATTEMPTS, WATCHDOG_RETRY_SEC
                    );
                }
                // AUDIT-P6: после исчерпания быстрых попыток — slow-retry.
                // PATCH P15: fast retries first, then 5 min, cap 30 min
                let backoff = std::time::Duration::from_secs(
                    if attempt <= 2 { 5 * attempt as u64 }
                    else if attempt <= 6 { 300 }
                    else { 1800 }
                );
                tracing::warn!("[{}] перезапуск через {:?}...", bot_name, backoff);
                tokio::time::sleep(backoff).await;
            }
        });
        handles.push(handle);
    }

    // ============================================================
    // ✅ P27: heartbeat — одна строка INFO каждые HEARTBEAT_SEC (дефолт 60,
    // HEARTBEAT_SEC=0 отключает). Лог всегда свежий → внешний watchdog.ps1
    // (P26) больше не путает «тихий рынок» (кандидаты режутся фильтрами
    // раньше diag) с зомби-процессом и не рестартует здорового бота.
    // bt/min — глобальный пульс bookTicker по всем хабам (websocket::BT_TOTAL).
    // ============================================================
    {
        let hb_risk = global_risk.clone();
        let hb_agents = handles.len();
        let hb_period = std::env::var("HEARTBEAT_SEC")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(60);
        if hb_period > 0 {
            tokio::spawn(async move {
                let started = std::time::Instant::now();
                let mut last_bt =
                    websocket::BT_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
                let mut iv =
                    tokio::time::interval(std::time::Duration::from_secs(hb_period));
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip); // ✅ P41/D16
                iv.tick().await; // первый тик interval мгновенный — пропускаем
                loop {
                    iv.tick().await;
                    let bt_now =
                        websocket::BT_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
                    let bt_min = bt_now.saturating_sub(last_bt);
                    last_bt = bt_now;
                    let up = started.elapsed().as_secs();
                    tracing::info!(
                        "[heartbeat] up={}m{}s | agents={} | pos={} | bt/min={}",
                        up / 60,
                        up % 60,
                        hb_agents,
                        hb_risk.active_positions_count(),
                        bt_min
                    );
                }
            });
            tracing::info!(
                "[P27] heartbeat запущен (период {} сек; HEARTBEAT_SEC=0 отключает)",
                hb_period
            );
        } else {
            tracing::info!("[P27] heartbeat отключён (HEARTBEAT_SEC=0)");
        }
    }
    // ============ GRACEFUL SHUTDOWN ============
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
    }
    tracing::warn!("🛑 Получен сигнал остановки — graceful shutdown (до 120 с: maker-выходы в плюс, потом маркет)...");

    let _ = shutdown_tx.send(());

    // ✅ МЯГКИЙ SHUTDOWN: дедлайн 120 с
    let all = futures_util::future::join_all(handles);
    match tokio::time::timeout(std::time::Duration::from_secs(120), all).await {
        Ok(_) => tracing::info!("✅ все боты завершились корректно"),
        Err(_) => tracing::error!(
            "⏱ 120с истекли — часть ботов не завершилась, процесс завершается. СВЕРЬ ПОЗИЦИИ НА БИРЖЕ"
        ),
    }

    metrics_handle.abort();
    tracing::info!("✅ Завершение завершено");
    Ok(())
}

/// ✅ 4а: дети бота живут ровно столько, сколько живёт бот.
async fn run_bot(
    config: config::BotConfig,
    symbol: String,
    global: config::GlobalConfig,
    ws_rx: tokio::sync::broadcast::Receiver<websocket::WebSocketMessage>,
    time_sync: exchange::SharedTimeSync,
    shutdown_rx: tokio::sync::broadcast::Receiver<()>,
    filters: Option<Arc<exchange::FilterRegistry>>,
    hub: Arc<websocket::WebSocketHub>,
    global_risk: Arc<risk::GlobalRiskManager>,
) -> Result<()> {
    let mut children = tokio::task::JoinSet::new();
    let result = run_bot_inner(
        config,
        symbol,
        global,
        ws_rx,
        time_sync,
        shutdown_rx,
        filters,
        hub,
        global_risk,
        &mut children,
    )
    .await;
    children.abort_all();
    result
}

async fn run_bot_inner(
    config: config::BotConfig,
    symbol: String,
    global: config::GlobalConfig,
    mut ws_rx: tokio::sync::broadcast::Receiver<websocket::WebSocketMessage>,
    time_sync: exchange::SharedTimeSync,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
    filters: Option<Arc<exchange::FilterRegistry>>,
    hub: Arc<websocket::WebSocketHub>,
    global_risk: Arc<risk::GlobalRiskManager>,
    children: &mut tokio::task::JoinSet<()>,
) -> Result<()> {
    let bot_name = config.name.clone();
    let is_isolated = config.isolated;

    let exchange_client = exchange::create_client(&global, &config, time_sync)?;

    // ✅ P25: enum Exchange вместо строковых проверок URL по rest_url_override.
    // ВАЖНО (прежнее поведение сохранено): здесь смотрится ТОЛЬКО override,
    // без fallback на global.rest_url — как в исходном коде.
    let bot_exchange = config.rest_url_override.as_deref()
        .map(exchange::Exchange::from_url)
        .unwrap_or(exchange::Exchange::Binance);

    // ✅ P37/K3: плечо и режим маржи выставляются НА БИРЖЕ при старте.
    // Раньше `leverage` и `isolated` жили только в конфиге: флаг isolated
    // НЕ означал isolated margin (только изоляцию WS/БД), а фактическое
    // плечо на бирже могло быть любым — наследие ручных настроек.
    // Warn-only: сбой не мешает боту работать, но виден в логе.
    if config.market_type == "futures" && bot_exchange == exchange::Exchange::Binance {
        if let Err(e) = exchange_client
            .apply_margin_settings(&symbol, config.leverage, config.isolated)
            .await
        {
            tracing::warn!(
                "[{}] margin/leverage не применены (сверьте вручную на бирже): {}",
                bot_name, e
            );
        }
    }

    // v4.0: OKX has no Binance-style user-data stream; fills via REST polling.
    // ✅ P53/H9-2: подписываемся ДВАЖДЫ — второй приёмник уходит смотрителю
    // закрытий биржей (H9), первый — движку (place_and_wait / комиссии).
    let (order_events, mut h9_events) = if config.market_type == "futures" && !config.simulate && bot_exchange == exchange::Exchange::Binance {
        let rest_base = config
            .rest_url_override
            .as_ref()
            .unwrap_or(&global.rest_url)
            .trim_end_matches('/')
            .to_string();
        let ws_base = config.ws_url_override.clone().unwrap_or(global.ws_url.clone());
        // ✅ P40/K1: ключ — expose()
        match exchange::user_stream::UserStream::start(children, config.api_key.expose(), &rest_base, &ws_base).await {
            Ok(us) => {
                tracing::info!("[{}] 🔑 user-data stream запущен (события исполнения + факт-комиссии)", bot_name);
                metrics::UDS_ACTIVE.with_label_values(&[&bot_name]).set(1);
                (
                    Some(exchange::user_stream::OrderEventsRx::new(us.subscribe())),
                    Some(us.subscribe()), // ✅ P53/H9-2: канал смотрителя закрытий бирж
                )
            }
            Err(e) => {
                tracing::warn!(
                    "[{}] user-data stream недоступен ({}) — подтверждение через REST-поллинг, комиссии в fallback-оценке",
                    bot_name, e
                );
                metrics::UDS_ACTIVE.with_label_values(&[&bot_name]).set(0);
                (None, None)
            }
        }
    } else {
        metrics::UDS_ACTIVE.with_label_values(&[&bot_name]).set(0);
        (None, None)
    };

    // ✅ Фаза 3г + 6: свечи для futures через REST
    if config.market_type == "futures" {
        let rest_base = config
            .rest_url_override
            .as_ref()
            .unwrap_or(&global.rest_url)
            .trim_end_matches('/')
            .to_string();
        // ✅ P25: enum Exchange вместо строковых проверок URL.
        // ВАЖНО: enum вынесен в переменную ДО match — временный заём &rest_base
        // в scrutinee жил бы до конца match, а руки переносят rest_base (E0505).
        // ✅ P41/W9: URL → global.exchange → Binance (URL без маркера биржи
        // больше не форсирует Binance при global.exchange="bybit"/"okx").
        let poller_ex = exchange::Exchange::detect(&rest_base, &global.exchange)
            .unwrap_or(exchange::Exchange::Binance);
        match poller_ex {
            exchange::Exchange::Bybit => exchange::kline_poller::spawn_bybit(
                children,
                hub.clone(),
                rest_base,
                symbol.clone(),
                config.kline_interval.clone(),
            ),
            exchange::Exchange::Okx => {
                // v4.0: OKX V5 candles via public REST
                exchange::kline_poller::spawn_okx(
                    children,
                    hub.clone(),
                    rest_base,
                    symbol.clone(),
                    config.kline_interval.clone(),
                )
            }
            exchange::Exchange::Binance => exchange::kline_poller::spawn(
                children,
                hub.clone(),
                rest_base,
                symbol.clone(),
                config.kline_interval.clone(),
            ),
        }
        tracing::info!(
            "[{}] 📈 kline-поллер запущен (REST, интервал {})",
            bot_name, config.kline_interval
        );
    }

    let state_manager = Arc::new(state::StateManager::new(&config, &global).await?);
    let trade_journal =
        state::trade_journal::TradeJournal::new(&global.db_path, &bot_name).await?;

    // ============================================================
    // ✅ 4г: RECONCILIATION при старте (5 попыток V3.2, sync_qty, усыновление)
    // ============================================================
    if !config.simulate && config.market_type == "futures" {
        let mut fetched: Option<Vec<crate::exchange::traits::ExchangePosition>> = None;
        for attempt in 1..=5u32 {
            match exchange_client.get_positions().await {
                Ok(all) => { fetched = Some(all); break; }
                Err(e) => {
                    // ✅ V3.2: 5 попыток с эскалацией 2/4/6/8с — тестнеты
                    // отдают 5xx пачками, 3 попытки по 2с не всегда хватало.
                    tracing::warn!("[{}] reconciliation попытка {}/5: {}", bot_name, attempt, e);
                    if attempt < 5 {
                        let pause = 2u64.saturating_mul(attempt as u64);
                        tokio::time::sleep(std::time::Duration::from_secs(pause)).await;
                    }
                }
            }
        }
        match fetched {
            Some(all) => {
                let exchange_positions: Vec<_> = all
                    .into_iter()
                    .filter(|p| p.symbol.eq_ignore_ascii_case(&symbol))
                    .collect();
                let db_positions = state_manager.get_positions();

                // 1) Фантомы
                for pos in &db_positions {
                    let on_exchange = exchange_positions
                        .iter()
                        .find(|p| p.symbol.eq_ignore_ascii_case(&pos.symbol));
                    match on_exchange {
                        None => {
                            tracing::error!(
                                "[{}] 🚨 RECONCILIATION: {} есть в БД, но НЕТ на бирже — фантом",
                                bot_name, pos.symbol
                            );
                            state_manager.purge_position(&pos.symbol).await;
                            global_risk.unregister_position(&bot_name, &pos.symbol);
                        }
                        Some(ex) => {
                            let ex_qty = ex.qty.abs();
                            if (ex_qty - pos.quantity).abs() > pos.quantity * 0.05 {
                                tracing::warn!(
                                    "[{}] 🚨 RECONCILIATION: {} в БД {:.5}, на бирже {:.5} — синхронизирую по бирже",
                                    bot_name, pos.symbol, pos.quantity, ex_qty
                                );
                                state_manager.sync_position_qty(&ex.symbol, ex_qty, ex.entry_price).await;
                            }
                        }
                    }
                }

                // 2) Сироты — усыновляем
                for ex in &exchange_positions {
                    let in_db = db_positions
                        .iter()
                        .any(|p| p.symbol.eq_ignore_ascii_case(&ex.symbol));
                    if !in_db {
                    let (side_str, qty_abs) = if ex.qty > 0.0 {
                        ("BUY".to_string(), ex.qty)
                    } else {
                        ("SELL".to_string(), ex.qty.abs())
                    };

                                       // ✅ ПРАВКА #5: synthetic SL для усыновлённой позиции.
                    // На бирже SL нет — без синтетического стопа state_manager
                    // не сможет закрыть позицию до max_position_duration_sec.
                    // 2% от entry — аварийный стоп, согласуется с risk_per_trade_pct.
                    const EMERGENCY_SL_PCT: f64 = 0.02;
                    let synthetic_sl = if ex.qty > 0.0 {
                        ex.entry_price * (1.0 - EMERGENCY_SL_PCT)
                    } else {
                        ex.entry_price * (1.0 + EMERGENCY_SL_PCT)
                    };

                    // ✅ P22: перехват живого биржевого STOP_MARKET при усыновлении
                    // (roadmap #7). Если на бирже уже стоит защитный STOP_MARKET
                    // (closePosition=true → qty=0) на сторону ЗАКРЫТИЯ — берём его
                    // цену как SL вместо синтетики и запоминаем sl_order_id:
                    // SL-фолбэк (AUDIT-P7) не будет ставить дубликат.
                    let close_side = if ex.qty > 0.0 { "SELL" } else { "BUY" };
                    let adopted_sl: Option<(f64, u64)> = match exchange_client
                        .get_open_orders(&ex.symbol)
                        .await
                    {
                        Ok(oo) => oo
                            .iter()
                            .find(|o| {
                                o.side.eq_ignore_ascii_case(close_side)
                                    && o.price > 0.0
                                    && o.quantity == 0.0
                            })
                            .map(|o| (o.price, o.order_id)),
                        Err(_) => None,
                    };
                    let (eff_sl, eff_src) = match adopted_sl {
                        Some((p, id)) => (p, format!("биржевой STOP_MARKET #{}", id)),
                        None => (synthetic_sl, format!("synthetic {}%", EMERGENCY_SL_PCT * 100.0)),
                    };

                    // ✅ ПРАВКА #3.1: два масштаба риска.
                    //   per_unit_risk  → add_position_with_risk (для R-multiple в логах)
                    //   total_risk     → global_risk.register_position (для max_concurrent_risk_pct)
                    let per_unit_risk = (ex.entry_price - eff_sl).abs();
                    let total_risk = qty_abs * per_unit_risk;

                    tracing::warn!(
                        "[{}] 🚨 RECONCILIATION: позиция {} ({:.5}) есть на бирже, но нет в БД — \
                         УСЫНОВЛЯЮ с SL={:.8} ({}), риск/ед={:.8}, \
                         тотал={:.4} USDT, таймер {}с закроет",
                        bot_name, ex.symbol, ex.qty, eff_sl, eff_src,
                        per_unit_risk, total_risk,
                        config.max_position_duration_sec
                    );

                    state_manager
                        .add_position_with_risk(
                            ex.symbol.clone(),
                            side_str,
                            ex.entry_price,
                            qty_abs,
                            Some(eff_sl),
                            None,
                            "Reconciled".to_string(),
                            per_unit_risk,  // ← было reconciled_risk (total)
                            0.0,
                            0.0,
                        )
                        .await;
                    // ✅ P22: фиксируем id перехваченного биржевого стопа.
                    if let Some((_, id)) = adopted_sl {
                        state_manager.set_sl_order_id(&ex.symbol, id.to_string()).await;
                    }
                    global_risk.register_position(
                        &bot_name,
                        &ex.symbol,
                        total_risk,  // ← total, для max_concurrent_risk_pct
                    );
                    }
                }

                tracing::info!(
                    "[{}] ✅ reconciliation: в БД {}, на бирже {} — сверка проведена",
                    bot_name, db_positions.len(), exchange_positions.len()
                );
            }
            None => {
                // ✅ v3.5: честно — попыток было 5 (ретраи 2/4/6/8с)
                tracing::warn!(
                    "[{}] reconciliation недоступен после 5 попыток — продолжаем по данным БД",
                    bot_name
                );
            }
        }
    }

    // ✅ Decorator: устойчивый reconcile (retry + timeout + circuit + DB fallback)
    //    ВАЖНО: после startup-reconcile (строгий обмен с биржей для фантомов),
    //    но до periodic-reconciler и ExecutionEngine — теперь все запросы
    //    get_positions() идут через обёртку.
    let exchange_client: Arc<dyn exchange::ExchangeClient> = Arc::new(
        // PATCH P15: strict by default on prod (non-testnet URL)
        exchange::resilient_client::ResilientClient::new(
            exchange_client.clone(),
            state_manager.clone(),
            !config
                .rest_url_override
                .clone()
                .unwrap_or_else(|| global.rest_url.clone())
                .to_lowercase()
                .contains("testnet"),
        ),
    ) as Arc<dyn exchange::ExchangeClient>;

    // ============================================================
    // ✅ bk10: фоновое начисление funding
    // ============================================================
    if config.market_type == "futures" {
        let sm = state_manager.clone();
        let fut_rest = config
            .rest_url_override
            .as_ref()
            .unwrap_or(&global.rest_url)
            .clone();
        let tracker = exchange::FundingTracker::new(&fut_rest);
        let market_sym = symbol.clone();
        let bot_tag = bot_name.clone();

        children.spawn(async move {
            const POLL_SECS: u64 = 300;
            const SLOTS_IN_8H: f64 = 8.0 * 3600.0 / POLL_SECS as f64;

            let mut interval = tokio::time::interval(
                std::time::Duration::from_secs(POLL_SECS),
            );
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip); // ✅ P41/D16
            interval.tick().await;

            loop {
                interval.tick().await;

                let mut symbols = sm.open_symbols();
                if !symbols.contains(&market_sym) {
                    symbols.push(market_sym.clone());
                }

                for sym in symbols {
                    match tracker.get_rate(&sym).await {
                        Ok(rate_info) => {
                            let positions = sm.get_positions();
                            for pos in positions.iter().filter(|p| p.symbol == sym) {
                                let notional = pos.quantity * pos.current_price;
                                let raw = rate_info.rate * notional / SLOTS_IN_8H;
                                let signed = match pos.side {
                                    crate::state::state_manager::PositionSide::Long => -raw,
                                    crate::state::state_manager::PositionSide::Short => raw,
                                };
                                sm.accrue_funding(&sym, signed);
                            }
                        }
                        Err(e) => {
                            tracing::debug!(
                                "[{}] funding rate для {} недоступен: {}",
                                bot_tag, sym, e
                            );
                        }
                    }
                }
            }
        });

        tracing::info!(
            "[{}] 💸 Funding tracker запущен (poll 5 мин, символ {})",
            bot_name, symbol
        );
    }

    // ============================================================
    // ✅ Периодический reconciliation (5 мин, двойное подтверждение)
    // ============================================================
    if !config.simulate && config.market_type == "futures" {
        let sm = state_manager.clone();
        let client = exchange_client.clone();
        let gr = global_risk.clone();
        let bot = bot_name.clone();
        let sym = symbol.clone();
        let max_dur = config.max_position_duration_sec;
        children.spawn(async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_secs(300));
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip); // ✅ P41/D16
            iv.tick().await;
            let mut pending_purge: std::collections::HashSet<String> = Default::default();
            let mut pending_adopt: std::collections::HashSet<String> = Default::default();
            let mut p22_fail_streak: u32 = 0; // ✅ P22: сбои get_positions подряд
            loop {
                iv.tick().await;
                // ✅ P22: сбой get_positions больше не тихий. Раньше
                // `else { continue }` молча пропускал тик: бот выглядел живым,
                // а периодическая сверка могла не работать неопределённо долго.
                // ✅ P55/D2-1: АВТОРИТЕТНЫЙ вызов — БЕЗ fallback на локальный
                // стейт. Раньше get_positions() на сбое positionRisk (тестнет
                // STRICT=0) возвращал локальные позиции: сверка всегда
                // «сходилась», фантомы/сироты/дрейф были невидимы, C4/H9-дырки
                // не закрывались никогда.
                let all = match client.get_positions_authoritative().await {
                    Ok(v) => {
                        if p22_fail_streak > 0 {
                            tracing::info!(
                                "[{}] 🔄 (период.) reconcile восстановился после {} сбоев подряд",
                                bot, p22_fail_streak
                            );
                        }
                        p22_fail_streak = 0;
                        v
                    }
                    Err(e) => {
                        p22_fail_streak += 1;
                        tracing::warn!(
                            "[{}] ⚠ (период.) reconcile: positionRisk недоступен ({} подряд): {} — тик пропущен",
                            bot, p22_fail_streak, e
                        );
                        continue;
                    }
                };
                let ex_pos: Vec<_> = all.into_iter()
                    .filter(|p| p.symbol.eq_ignore_ascii_case(&sym))
                    .collect();
                let db_pos = sm.get_positions();

                let mut purges_now: std::collections::HashSet<String> = Default::default();
                let mut adopts_now: std::collections::HashSet<String> = Default::default();
                for pos in &db_pos {
                    if !ex_pos.iter().any(|p| p.symbol.eq_ignore_ascii_case(&pos.symbol)) {
                        purges_now.insert(pos.symbol.clone());
                    }
                }
                for ex in &ex_pos {
                    if !db_pos.iter().any(|p| p.symbol.eq_ignore_ascii_case(&ex.symbol)) {
                        adopts_now.insert(ex.symbol.clone());
                    }
                }

                for s in purges_now.intersection(&pending_purge) {
                    tracing::warn!("[{}] 🧹 (период.) фантом {} подтверждён дважды — удаляю из state", bot, s);
                    sm.purge_position(s).await;
                    gr.unregister_position(&bot, s);
                }
                pending_purge = purges_now;

                for s in adopts_now.intersection(&pending_adopt) {
                    if let Some(ex) = ex_pos.iter().find(|p| p.symbol.eq_ignore_ascii_case(s)) {
                        let (side_str, qty_abs) = if ex.qty > 0.0 {
                            ("BUY".to_string(), ex.qty)
                        } else {
                            ("SELL".to_string(), ex.qty.abs())
                        };

                        // ✅ ПРАВКА #5 (период.): synthetic SL для усыновлённой сироты.
                        const EMERGENCY_SL_PCT: f64 = 0.02;
                        let synthetic_sl = if ex.qty > 0.0 {
                            ex.entry_price * (1.0 - EMERGENCY_SL_PCT)
                        } else {
                            ex.entry_price * (1.0 + EMERGENCY_SL_PCT)
                        };

                        // ✅ P22: перехват живого биржевого STOP_MARKET при усыновлении
                        // (roadmap #7). get_open_orders отдаёт stopPrice в поле price
                        // только для STOP_MARKET (см. binance_futures.rs) с qty=0
                        // (closePosition=true — так ставит сам бот). Стоп найден —
                        // берём ЕГО цену как SL и запоминаем sl_order_id, чтобы
                        // SL-фолбэк (AUDIT-P7) не ставил дублирующий стоп.
                        let close_side = if ex.qty > 0.0 { "SELL" } else { "BUY" };
                        let adopted_sl: Option<(f64, u64)> = match client.get_open_orders(s).await {
                            Ok(oo) => oo
                                .iter()
                                .find(|o| {
                                    o.side.eq_ignore_ascii_case(close_side)
                                        && o.price > 0.0
                                        && o.quantity == 0.0
                                })
                                .map(|o| (o.price, o.order_id)),
                            Err(_) => None,
                        };
                        let (eff_sl, eff_src) = match adopted_sl {
                            Some((p, id)) => (p, format!("биржевой STOP_MARKET #{}", id)),
                            None => (synthetic_sl, format!("synthetic {}%", EMERGENCY_SL_PCT * 100.0)),
                        };

                        // ✅ ПРАВКА #3.1: два масштаба (per_unit vs total).
                        let per_unit_risk = (ex.entry_price - eff_sl).abs();
                        let total_risk = qty_abs * per_unit_risk;

                        tracing::warn!(
                            "[{}] 🚨 (период.) сирота {} ({:.0}) подтверждена дважды — \
                             УСЫНОВЛЯЮ с SL={:.8} ({}), риск/ед={:.8}, \
                             тотал={:.4}, таймер {}с закроет",
                            bot, s, qty_abs, eff_sl, eff_src,
                            per_unit_risk, total_risk, max_dur
                        );

                        sm.add_position_with_risk(
                            s.clone(), side_str, ex.entry_price, qty_abs,
                            Some(eff_sl), None,
                            "Reconciled".to_string(),
                            per_unit_risk,
                            0.0, 0.0,
                        ).await;
                        // ✅ P22: биржевой стоп уже стоит — фиксируем его id,
                        // чтобы периодический SL-фолбэк не плодил дубликаты.
                        if let Some((_, id)) = adopted_sl {
                            sm.set_sl_order_id(s, id.to_string()).await;
                        }
                        gr.register_position(&bot, s, total_risk);
                    }
                }
                pending_adopt = adopts_now;

                // AUDIT-P7: ретрай биржевого SL. sl_order_id=None при живой
                // позиции = STOP_MARKET НЕ стоит на бирже (постановка 2/2
                // сорвалась при входе, либо сирота усыновлена — ей биржевой
                // стоп вообще не ставился). Софт-защита умирает вместе с
                // процессом — потому каждые 5 мин догонаю стоп на биржу.
                // Читаем СВЕЖИЙ список: фантомы этого тика не получат стоп,
                // свежие сироты получат сразу. NB: возможен дубль со старым
                // стопом прежней сессии (оба closePosition=true — безвредно),
                // чистка зомби-ордеров — территория P8.
                for pos in sm.get_positions().into_iter().filter(|p| p.sl_order_id.is_none()) {
                    let Some(sl) = pos.stop_loss else { continue };
                    let close_side = if pos.side == state::state_manager::PositionSide::Long { "SELL" } else { "BUY" };
                    match client
                        .place_protective_order(&pos.symbol, close_side, true, sl, None)
                        .await
                    {
                        Ok(o) => {
                            sm.set_sl_order_id(&pos.symbol, o.order_id.to_string()).await;
                            tracing::warn!(
                                "[{}] 🛡 (период.) SL-фолбэк: STOP_MARKET #{} @ {:.8} догнан для {}",
                                bot, o.order_id, sl, pos.symbol
                            );
                        }
                        Err(e) => {
                            tracing::error!(
                                "[{}] 🛡 (период.) SL-фолбэк для {} снова не удался: {}",
                                bot, pos.symbol, e
                            );
                        }
                    }
                }

                // ============================================================
                // ✅ P53/C4-4: сверка ЗАЩИТ через openAlgoOrders (C4).
                // (1) Позиция жива, а SL/TP пропал с биржи (исполнен, снят
                //     вручную, серверная чистка) → WARN + восстановление защиты.
                // (2) Алго-защиты БЕЗ позиции (сироты после закрытия) → снятие.
                // Классический openOrders алго-ордера не видит — потому
                // отдельный эндпоинт; недоступность = C4 отключается
                // (однократный warn), поведение прежнее не ломается.
                // ============================================================
                match client.get_open_algo_orders(&sym).await {
                    Ok(algo_orders) => {
                        static C4_SUPPORTED: std::sync::atomic::AtomicBool =
                            std::sync::atomic::AtomicBool::new(true);
                        C4_SUPPORTED.store(true, std::sync::atomic::Ordering::Relaxed);
                        for pos in sm.get_positions() {
                            let close_side = if pos.side == state::state_manager::PositionSide::Long { "SELL" } else { "BUY" };
                            let working: Vec<_> = algo_orders.iter().filter(|o| o.is_working()).collect();
                            let sl_live = working.iter().any(|o| o.is_stop(&close_side));
                            let tp_live = working.iter().any(|o| o.is_take_profit(&close_side));
                            // (1a) SL должен стоять, но его нет → восстановить
                            if !sl_live && pos.stop_loss.is_some() {
                                tracing::warn!(
                                    "[{}] 🛡 (период.) P53/C4: SL отсутствует на Algo API при живой позиции {} ({:.5}) — восстанавливаю STOP_MARKET @ {:.8}",
                                    bot, pos.symbol, pos.quantity, pos.stop_loss.unwrap_or(0.0)
                                );
                                if let Some(sl) = pos.stop_loss {
                                    match client.place_protective_order(&pos.symbol, close_side, true, sl, None).await {
                                        Ok(o) => {
                                            sm.set_sl_order_id(&pos.symbol, o.order_id.to_string()).await;
                                            tracing::warn!("[{}] 🛡 (период.) P53/C4: SL восстановлен #{} @ {:.8}", bot, o.order_id, sl);
                                        }
                                        Err(e) => tracing::error!("[{}] 🚨 (период.) P53/C4: восстановление SL не удалось: {}", bot, e),
                                    }
                                }
                            }
                            // (1b) TP должен стоять, но его нет → восстановить
                            if !tp_live && pos.take_profit.is_some() {
                                tracing::warn!(
                                    "[{}] 🛡 (период.) P53/C4: TP отсутствует на Algo API при живой позиции {} ({:.5}) — восстанавливаю TAKE_PROFIT_MARKET @ {:.8}",
                                    bot, pos.symbol, pos.quantity, pos.take_profit.unwrap_or(0.0)
                                );
                                if let Some(tp) = pos.take_profit {
                                    match client.place_protective_order(&pos.symbol, close_side, false, tp, None).await {
                                        Ok(o) => {
                                            sm.set_tp_order_id(&pos.symbol, o.order_id.to_string()).await;
                                            tracing::warn!("[{}] 🛡 (период.) P53/C4: TP восстановлен #{} @ {:.8}", bot, o.order_id, tp);
                                        }
                                        Err(e) => tracing::error!("[{}] 🚨 (период.) P53/C4: восстановление TP не удалось: {}", bot, e),
                                    }
                                }
                            }
                        }
                        // (2) сироты: алго-защиты символа без живой позиции в state
                        let live_syms: Vec<String> =
                            sm.get_positions().iter().map(|p| p.symbol.to_uppercase()).collect();
                        for o in &algo_orders {
                            if o.is_working() && !live_syms.contains(&o.symbol.to_uppercase()) {
                                match client.cancel_order(&o.symbol, o.algo_id).await {
                                    Ok(_) => tracing::warn!(
                                        "[{}] 🧹 (период.) P53/C4: сирота-защита #{} ({}) снята — позиции {} нет в state",
                                        bot, o.algo_id, o.algo_type, o.symbol
                                    ),
                                    Err(e) => tracing::warn!(
                                        "[{}] 🧹 (период.) P53/C4: сирота #{} не снята: {}",
                                        bot, o.algo_id, e
                                    ),
                                }
                            }
                        }
                    }
                    Err(e) => {
                        static C4_DISABLED_LOGGED: std::sync::atomic::AtomicBool =
                            std::sync::atomic::AtomicBool::new(false);
                        if !C4_DISABLED_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                            tracing::warn!(
                                "[{}] P53/C4: openAlgoOrders недоступен ({}), сверка защит отключена — поведение прежнее",
                                bot, e
                            );
                        }
                    }
                }
            }
        });
        tracing::info!("[{}] 🔄 периодический reconciliation запущен (5 мин, двойное подтверждение)", bot_name);
    }
        // ✅ 4б: риск-менеджер — ОДИН на весь процесс
    let risk_manager = risk::RiskManager::new(&config, &global, global_risk.clone()).await?;

    let execution_engine = execution::ExecutionEngine::new(
        &config,
        exchange_client.clone(),
        state_manager.clone(),
        global_risk.clone(),
        filters,
        order_events,
    );

    let mut regime_detector = strategies::market_regime::MarketRegimeDetector::new(400, 0.30, 0.05);

    let mut strategy_agent = agents::StrategyAgent::new(&config);
    let mut risk_agent = agents::RiskAgent::new(risk_manager);
    let mut execution_agent = agents::ExecutionAgent::new(execution_engine);
    let state_agent = agents::StateAgent::new(state_manager.clone(), exchange_client.clone());

    // ============================================================
    // ✅ P53/H9-2: СМОТРИТЕЛЬ ЗАКРЫТИЙ БИРЖ (ORDER_TRADE_UPDATE защит).
    // Когда STOP_MARKET/TAKE_PROFIT_MARKET исполнен биржей, позиция закрыта
    // БЕЗ участия движка. Раньше это обнаруживалось 5-минутным reconcile,
    // который PURGE'ил позицию БЕЗ записи PnL: риск-учёт (daily_loss,
    // realized_pnl, портфельный стоп) биржевые закрытия НЕ ВИДЕЛ.
    // Разделение труда: эта задача только закрывает позицию в state и
    // снимает выжившую защиту; ВЕСЬ учёт (risk_agent, метрики, журнал,
    // P15/HARD HALT) делает основной цикл через канал ExchangeClose.
    // ============================================================
    let mut h9_notify_rx: Option<tokio::sync::mpsc::UnboundedReceiver<ExchangeClose>> = None;
    if let Some(h9_rx) = h9_events.take() {
        let sm = state_manager.clone();
        let client = exchange_client.clone();
        let bot = bot_name.clone();
        let (h9_tx, h9_rx_notify) = tokio::sync::mpsc::unbounded_channel::<ExchangeClose>();
        h9_notify_rx = Some(h9_rx_notify);
        children.spawn(async move {
            let mut rx = h9_rx;
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        // фильтр: ТОЛЬКО филлы защитных ордеров. Защиты всегда
                        // имеют клиент-id "pr_..."; тип приходит в "ot". Прочие
                        // события (наши MARKET-входы/выходы) обслуживает движок.
                        let protective = ev.order_type.eq_ignore_ascii_case("STOP_MARKET")
                            || ev.order_type.eq_ignore_ascii_case("TAKE_PROFIT_MARKET")
                            || ev.client_order_id.starts_with("pr_");
                        if !protective || ev.status != "FILLED" {
                            continue;
                        }
                        // дубль-гейт: движок уже мог закрыть позицию маркетом
                        // (гонка SL на бирже vs софт-стоп) — тогда close_position
                        // вернул бы PnL=0 по фаному; не учитываем.
                        if !sm.has_position(&ev.symbol) {
                            continue;
                        }
                        let fill_price = if ev.avg_price > 0.0 { ev.avg_price } else { ev.last_filled_price };
                        let state_qty = sm
                            .get_positions()
                            .iter()
                            .find(|p| p.symbol.eq_ignore_ascii_case(&ev.symbol))
                            .map(|p| p.quantity)
                            .unwrap_or(0.0);
                        // closePosition=true: z = фактически закрытый объём.
                        let fill_qty = if ev.cum_filled_qty > 0.0 { ev.cum_filled_qty } else { state_qty };
                        if fill_price <= 0.0 || fill_qty <= 0.0 {
                            tracing::warn!(
                                "[{}] P53/H9: филл защитного ордера без цены/объёма (id={}, ot={}, z={}, ap={}) — жду периодический reconcile",
                                bot, ev.order_id, ev.order_type, ev.cum_filled_qty, ev.avg_price
                            );
                            continue;
                        }
                        // ids защит читаем ДО закрытия (close_position сотрёт запись).
                        let (sl_id, tp_id) = sm.get_protective_ids(&ev.symbol);
                        let pnl = sm.close_position(&ev.symbol, fill_price, fill_qty).await;
                        tracing::warn!(
                            "[{}] 🏁 P53/H9: БИРЖА ЗАКРЫЛА ПОЗИЦИЮ | {} | {} | id={} | exit={:.8} qty={:.5} | state PnL={:+.4} | биржа rp={:+.4}",
                            bot, ev.symbol, ev.order_type, ev.order_id,
                            fill_price, fill_qty, pnl, ev.realized_pnl
                        );
                        // Выжившая защита: SL исполнен → снять TP (и наоборот).
                        // Сам исполненный ордер уже FINISHED — cancel по нему
                        // вернёт false, не трогаем.
                        let survivor = if ev.order_type.eq_ignore_ascii_case("STOP_MARKET") { tp_id } else { sl_id };
                        if let Some(s) = survivor {
                            let oid: u64 = s.parse().unwrap_or(0);
                            if oid > 0 {
                                match client.cancel_order(&ev.symbol, oid).await {
                                    Ok(_) => tracing::info!(
                                        "[{}] P53/H9: выжившая защита #{} снята (позиция закрыта биржей)",
                                        bot, oid
                                    ),
                                    Err(e) => tracing::warn!(
                                        "[{}] P53/H9: снять выжившую защиту #{} не удалось: {} — дочистит периодический reconcile",
                                        bot, oid, e
                                    ),
                                }
                            }
                        }
                        let _ = h9_tx.send(ExchangeClose {
                            symbol: ev.symbol.clone(),
                            pnl,
                            exit_price: fill_price,
                            qty: fill_qty,
                            kind: ev.order_type.clone(),
                            exchange_pnl: ev.realized_pnl,
                        });
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(
                            "[{}] P53/H9: стрим отстал на {} событий — пропущенные филлы закроет периодический reconcile",
                            bot, n
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // ✅ P53/H9-2: НЕ break. Если watcher завершится, h9_tx
                        // умрёт, mpsc закроется, а recv() в главном цикле станет
                        // возвращать None мгновенно — busy-loop. Практически
                        // недостижимо (WS-таск жив до abort_all), но паркуемся
                        // навсегда, держа канал открытым до конца бота.
                        std::future::pending::<()>().await;
                        break;
                    }
                }
            }
        });
        tracing::info!(
            "[{}] 🏁 P53/H9: смотритель закрытий биржи запущен (ORDER_TRADE_UPDATE защит)",
            bot_name
        );
    }

    let ob_analyzer = strategies::OrderBookAnalyzer::new(
        global.orderbook_density_threshold.to_f64().unwrap_or(5.0),
    );
    let ob_state = Arc::new(std::sync::RwLock::new(strategies::OrderBookState::default()));
    let mut strategy = strategies::create_strategy(&config, &symbol, ob_state, ob_analyzer)?;

    tracing::info!(
        "[{}] Стратегия: {} | {}",
        bot_name,
        strategy.name(),
        if is_isolated { "🔒 ИЗОЛИРОВАН" } else { "🔗 ОБЩИЙ" }
    );

    metrics::BOTS_ACTIVE.with_label_values(&[&bot_name]).set(1);
    metrics::SUSPENDED.with_label_values(&[&bot_name]).set(0);
    tracing::info!("[{}] ✅ Бот запущен", bot_name);

    // ============================================================
    // ✅ v3.5: ГАРАНТИРОВАННЫЙ ПРОГРЕВ СТРАТЕГИИ СВЕЧАМИ
    // ============================================================
    // Живой лог 12:50 (gold_dca): backfill (249 свечей) опубликован в
    // broadcast в 12:50:33, а цикл бота стартовал читать канал только в
    // 12:50:44 — 5 флаковых reconciliation-попыток (2/4/6/8с) держали
    // старт ~21с. Приёмник на момент публикации отсутствовал → tokio
    // broadcast молча выбросил все 250 свечей → keltner навсегда холодный
    // (candles=0, ema200=n/a; DIAG «backfill дошла до цикла» так и не
    // появился, хотя keltner_dca.rs v3.4 был на месте).
    // Лечение двойное:
    //   (1) вычитать из broadcast-буфера всё, что уже накопилось для нас;
    //   (2) если буфер пуст/тонкий — добрать историю из кэша хаба
    //       (последние 600 REST-свечей), МИНУЯ broadcast.
    // Сигналы из прогрева НЕ исполняются (возврат process() игнорируется),
    // дубли исключены строгим возрастанием timestamp.
    let mut warm_fed: u64 = 0;
    let mut warm_from_cache: u64 = 0;
    let mut warm_signals_dropped: u64 = 0;
    let mut warm_books_dropped: u64 = 0;
    // ✅ v3.5: общий гард дедупа между тремя путями доставки свечей
    // (буфер broadcast / кэш хаба / сам broadcast-цикл) — кто первый
    // отдал свечу стратегии, тот и прав; копии отсекаются по timestamp.
    let mut last_fed_candle_ts: i64 = 0;

    // (1) буфер broadcast: всё, что успело накопиться, пока шёл старт бота
    loop {
        match ws_rx.try_recv() {
            Ok(message) => {
                if let websocket::WebSocketMessage::Kline(k) = &message {
                    if !k.is_closed || k.timestamp <= last_fed_candle_ts {
                        continue;
                    }
                    if k.is_backfill && !strategy.wants_backfill() {
                        continue;
                    }
                    last_fed_candle_ts = k.timestamp;
                    match strategy_agent.process(&message, strategy.as_mut()).await {
                        Ok(Some(_)) => warm_signals_dropped += 1,
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                "[{}] прогрев (буфер): стратегия отклонила свечу: {}",
                                bot_name, e
                            );
                            break;
                        }
                    }
                    warm_fed += 1;
                } else {
                    // книги/трейды на старте не нужны — цикл получит свежие
                    warm_books_dropped += 1;
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                tracing::warn!(
                    "[{}] прогрев: буфер отстал на {} сообщений — потерянную часть доберём из кэша",
                    bot_name, n
                );
            }
        }
    }

    // (2) добор из кэша хаба: случай «приёмника не было в момент публикации»
    // (старт задержан reconciliation-ретраями; рестарт супервизора на попытке
    // 2/3 — поллер backfill повторно НЕ делает, кэш хаба остаётся).
    if warm_fed < 200 {
        for k in hub.recent_klines() {
            if !k.is_closed || k.timestamp <= last_fed_candle_ts {
                continue;
            }
            if !strategy.wants_backfill() {
                continue;
            }
            last_fed_candle_ts = k.timestamp;
            // кэш-свеча трактуется как история: сигналы по ней запрещены
            let mut hist = k.clone();
            hist.is_backfill = true;
            let msg = websocket::WebSocketMessage::Kline(hist);
            match strategy_agent.process(&msg, strategy.as_mut()).await {
                Ok(Some(_)) => warm_signals_dropped += 1,
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        "[{}] прогрев (кэш): стратегия отклонила свечу: {}",
                        bot_name, e
                    );
                    break;
                }
            }
            warm_from_cache += 1;
        }
    }

    if warm_fed > 0 || warm_from_cache > 0 || warm_books_dropped > 0 {
        tracing::info!(
            "[{}] 🔥 прогрев стратегии: {} свечей из буфера + {} из кэша хаба{}{}",
            bot_name,
            warm_fed,
            warm_from_cache,
            if warm_signals_dropped > 0 {
                format!(" | сигналов истории отклонено: {}", warm_signals_dropped)
            } else {
                String::new()
            },
            if warm_books_dropped > 0 {
                format!(" | книг/трейдов мимо цикла: {}", warm_books_dropped)
            } else {
                String::new()
            }
        );
    }

    let mut log_interval = tokio::time::interval(std::time::Duration::from_secs(30));
    log_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip); // ✅ P41/D16
    log_interval.tick().await;

    let mut position_check_interval = tokio::time::interval(std::time::Duration::from_secs(5));
    position_check_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip); // ✅ P41/D16
    position_check_interval.tick().await;

    // ✅ DIAG v3.4.1: трассировка доставки сообщений/backfill до цикла бота.
    // Временный инструментальный пакет — убирается после диагностики.
    let mut diag_backfill_seen: u64 = 0;
    let mut diag_first_msg_logged: bool = false;
    let mut diag_gate_skips: u64 = 0;

    loop {
        tokio::select! {
            // ✅ P41/W6: biased удалён — при непрерывном bookTicker-потоке
            // ветка ws_rx голодала position_check_interval (5с: реконсиляция
            // и синхронизация защит). Без biased tokio опрашивает готовые
            // ветки случайно — шатдаун/данные/периодика живут одновременно.
            // ============================================================
            // SHUTDOWN — мягкий: maker-цена «безубыток+», без партиалов
            // ============================================================
            _ = shutdown_rx.recv() => {
                tracing::warn!("[{}] 🛑 Shutdown — закрываю позиции терпеливо (maker в плюс, таймаут, потом маркет)", bot_name);
                // 🔧 ПАТЧ-SWEEP: гасим висячие ордера ДО закрытия позиций —
                // иначе graceful shutdown уходит, оставив живые GTX на книге
                // (кейс 2026-10-07: 890191413/890191847 пережили shutdown).
                // 🔧 ЭТАП B: при живой позиции защитные ордера (pr_*) ОСТАЮТСЯ
                // сторожить её на бирже; при флэте — снимается всё.
                let positions_alive = !state_manager.get_positions().is_empty();
                if let Ok(oo) = exchange_client.get_open_orders(&symbol).await {
                    for o in oo {
                        if positions_alive {
                            tracing::warn!(
                                "[{}] sweep-пропуск: ордер {} оставлен (позиция жива — защиты сторожат)",
                                bot_name, o.order_id
                            );
                            continue;
                        }
                        match exchange_client.cancel_order(&symbol, o.order_id).await {
                            Ok(_) => tracing::warn!(
                                "[{}] sweep: отменён висячий ордер {} ({})",
                                bot_name, o.order_id, o.status
                            ),
                            Err(e) => tracing::error!(
                                "[{}] sweep: отмена {} не удалась: {}",
                                bot_name, o.order_id, e
                            ),
                        }
                    }
                }
                if let Ok(all) = exchange_client.get_positions().await {
                    for p in all.iter().filter(|p| p.symbol.eq_ignore_ascii_case(&symbol)) {
                        if p.qty.abs() > 0.0 {
                            tracing::error!(
                                "[{}] 🚨 sweep: на бирже позиция {:.5} {} при завершении — СВЕРЬ ВРУЧНУЮ",
                                bot_name, p.qty, p.symbol
                            );
                        }
                    }
                }
                let positions = state_manager.get_positions();
                for pos in positions {
                    // ✅ ПРАВКА #5: skip maker для убыточных shutdown-позиций.
                    // Раньше всегда пытались maker через max(current, breakeven_target).
                    // Для loss-позиций breakeven_target выше рынка — maker не заполнится
                    // за 10с, потом маркет по хуже цене = slippage $0.31+/позиция.
                    // Теперь: profit → maker в плюс (Exit_Shutdown_Maker);
                    //          loss  → сразу маркет (Exit_Shutdown_Market).
                    let close_signal = {
                        let fee_rt = match pos.origin_strategy.as_str() {
                            // PATCH P15: SpreadCapture fee arm removed (pepe cleanup)
                            _ => 0.0005_f64 * 2.0,
                        };
                        let is_long = pos.side == state::state_manager::PositionSide::Long;
                        let breakeven_target = if is_long {
                            pos.entry_price * (1.0 + fee_rt * 1.5)
                        } else {
                            pos.entry_price * (1.0 - fee_rt * 1.5)
                        };
                        let is_profitable = if is_long {
                            pos.current_price >= breakeven_target
                        } else {
                            pos.current_price <= breakeven_target
                        };

                        let (price, strategy_name) = if is_profitable {
                            // profit → maker в плюс (цель = breakeven+, заполнится если цена дойдёт)
                            (breakeven_target, "Exit_Shutdown_Maker")
                        } else {
                            // loss → маркет сразу (signal.price не важен для маркет-пути)
                            (pos.current_price, "Exit_Shutdown_Market")
                        };

                        strategies::Signal {
                            symbol: pos.symbol.clone(),
                            side: if is_long { "SELL".to_string() } else { "BUY".to_string() },
                            price,
                            quantity: pos.quantity,
                            strategy_name: strategy_name.to_string(),
                            leverage: 1.0,
                            stop_loss: None,
                            take_profit: None,
                            atr_hint: pos.atr_hint,
                            entry_spread: pos.entry_spread,
                        }
                    };
                    match execution_agent.execute(close_signal.clone()).await {
                        Ok(execution::ExecOutcome::Exit(pnl)) => {
                            risk_agent.record_trade(&close_signal.symbol, pnl);
                            metrics::SUSPENDED.with_label_values(&[&bot_name]).set(
                                if risk_agent.is_suspended() { 1 } else { 0 }
                            );
                            let _ = trade_journal
                                .record_close(&close_signal, pnl)
                                .await;
                            global_risk.add_closed_pnl(pnl); // PATCH P15
                            tracing::info!(
                                "[{}] Shutdown-close {} | PnL={:+.4}",
                                bot_name, close_signal.symbol, pnl
                            );
                        }
                        Ok(execution::ExecOutcome::ExitPartial(pnl)) => {
                            // ✅ P38/2c: при остановке закрылось частично —
                            // остаток записан в state и подхватится при старте.
                            risk_agent.record_trade(&close_signal.symbol, pnl);
                            let _ = trade_journal
                                .record_close(&close_signal, pnl)
                                .await;
                            global_risk.add_closed_pnl(pnl); // PATCH P15
                            tracing::warn!(
                                "[{}] Shutdown-close {}: ЧАСТИЧНО (PnL={:+.4}) — остаток остался, подхватится при старте",
                                bot_name, close_signal.symbol, pnl
                            );
                        }
                        Ok(execution::ExecOutcome::ExitUnconfirmed) => {
                            tracing::error!(
                                "[{}] Shutdown-close {}: НЕ подтверждено — позиция осталась на бирже, СВЕРЬ ВРУЧНУЮ",
                                bot_name, close_signal.symbol
                            );
                        }
                        Ok(other) => tracing::warn!("[{}] Shutdown-close: {:?}", bot_name, other),
                        Err(e) => tracing::error!("[{}] Shutdown-close: {:?}", bot_name, e),
                    }
                }
                state_manager.persist_positions().await;
                risk_agent.flush_stats().await;
                tracing::info!(
                    "[{}] 📊 ФИНАЛ СЕССИИ | WR={:.1}% | PF={:.2} | trades={} | realized_pnl={:+.4} | suspended={}",
                    bot_name,
                    risk_agent.win_rate() * 100.0,
                    risk_agent.profit_factor(),
                    risk_agent.total_trades(),
                    risk_agent.realized_pnl(),
                    if risk_agent.is_suspended() { "ДА" } else { "нет" }
                );
                break;
            }

            // ============================================================
            // WS MESSAGES
            // ============================================================
            msg_result = ws_rx.recv() => {
                let message = match msg_result {
                    Ok(m) => m,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("[{}] Отстал на {} сообщений", bot_name, n);
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::error!("[{}] WS hub закрыт", bot_name);
                        break;
                    }
                };

                metrics::MESSAGES_TOTAL.with_label_values(&[&bot_name]).inc();

                // ✅ DIAG v3.4.1: что реально доходит до цикла бота
                if !diag_first_msg_logged {
                    diag_first_msg_logged = true;
                    let kind = match &message {
                        websocket::WebSocketMessage::Kline(k) => {
                            if k.is_backfill { "Kline(backfill)" } else { "Kline(live)" }
                        }
                        websocket::WebSocketMessage::Trade(_) => "Trade",
                        websocket::WebSocketMessage::OrderBook(_) => "OrderBook",
                        websocket::WebSocketMessage::BookTicker(_) => "BookTicker",
                    };
                    tracing::info!("[{}] 🔍 DIAG: первое сообщение из хаба: {}", bot_name, kind);
                }
                if let websocket::WebSocketMessage::Kline(k) = &message {
                    if k.is_backfill {
                        diag_backfill_seen += 1;
                        if diag_backfill_seen == 1 || diag_backfill_seen % 100 == 0 {
                            tracing::info!(
                                "[{}] 🔍 DIAG: backfill #{} дошла до цикла | sym={} is_closed={} ts={}",
                                bot_name, diag_backfill_seen, k.symbol, k.is_closed, k.timestamp
                            );
                        }
                    }
                }

                // ✅ v3.5: дедуп свечей между broadcast-путём и кэш-добором.
                // Если закрытая свеча уже ушла в стратегию из кэша (топ-ап),
                // её копия из broadcast здесь пропускается — и наоборот.
                if let websocket::WebSocketMessage::Kline(k) = &message {
                    if k.is_closed {
                        if k.timestamp <= last_fed_candle_ts {
                            continue;
                        }
                        last_fed_candle_ts = k.timestamp;
                    }
                }

                // ============ EXIT-CHECK (с партиал-веткой) ============
                // ✅ П3: backfill-свечи (история от kline-поллера) не проходят
                // exit-проверки: старая цена могла бы «закрыть» восстановленную
                // позицию по несуществующему SL/TP.
                let is_backfill = matches!(
                    &message,
                    websocket::WebSocketMessage::Kline(k) if k.is_backfill
                );
                if !is_backfill {
                if let Some(exit_signal) = state_agent.update(&message).await? {
                    match execution_agent.execute(exit_signal.clone()).await {
                        Ok(execution::ExecOutcome::Exit(pnl)) => {
                            // ✅ КНИЖНЫЙ МЕНЕДЖМЕНТ: Exit_Partial — фиксация ЧАСТИ.
                            // notify_position_closed НЕ зовём — иначе SignalFilter уйдёт
                            // в Cooldown, «я в позиции» слетит → двойной вход.
                            if exit_signal.strategy_name == "Exit_Partial" {
                                risk_agent.record_trade(&exit_signal.symbol, pnl);
                                let _ = trade_journal
                                    .record_close(&exit_signal, pnl)
                                    .await;
                                global_risk.add_closed_pnl(pnl); // PATCH P15
                                metrics::PNL_TOTAL
                                    .with_label_values(&[&bot_name])
                                    .add(pnl);
                                tracing::info!(
                                    "💵 [{}] PARTIAL ЗАФИКСИРОВАН | PnL={:+.4} USDT | остаток ведём трейлингом",
                                    bot_name, pnl
                                );
                                continue;
                            }

                            // полный выход
                            strategy.notify_position_closed(pnl);
                            risk_agent.record_trade(&exit_signal.symbol, pnl);

                            metrics::SUSPENDED.with_label_values(&[&bot_name]).set(
                                if risk_agent.is_suspended() { 1 } else { 0 }
                            );

                            let result = if pnl > 0.0 { "win" }
                                         else if pnl < 0.0 { "loss" }
                                         else { "flat" };
                            metrics::TRADES_CLOSED
                                .with_label_values(&[&bot_name, result])
                                .inc();
                            if pnl != 0.0 {
                                metrics::PNL_TOTAL
                                    .with_label_values(&[&bot_name])
                                    .add(pnl);
                            }
                            let _ = trade_journal
                                .record_close(&exit_signal, pnl)
                                .await;
                            global_risk.add_closed_pnl(pnl); // PATCH P15

                            metrics::WIN_RATE
                                .with_label_values(&[&bot_name])
                                .set(risk_agent.win_rate());
                            metrics::PROFIT_FACTOR
                                .with_label_values(&[&bot_name])
                                .set(risk_agent.profit_factor());

                            tracing::info!(
                                "🔔 [{}] СДЕЛКА ЗАКРЫТА | результат={} | PnL={:+.4} USDT",
                                bot_name, result, pnl
                            );
                        }
                        Ok(execution::ExecOutcome::ExitPartial(pnl)) => {
                            // ✅ P38/2c: закрылось частично — остаток жив
                            // (SL/TP на бирже активны, state уменьшен).
                            // Кулдаун НЕ включаем, позицию ведём дальше.
                            risk_agent.record_trade(&exit_signal.symbol, pnl);
                            let _ = trade_journal
                                .record_close(&exit_signal, pnl)
                                .await;
                            global_risk.add_closed_pnl(pnl); // PATCH P15
                            metrics::PNL_TOTAL
                                .with_label_values(&[&bot_name])
                                .add(pnl);
                            tracing::warn!(
                                "⚠️ [{}] ВЫХОД ЧАСТИЧЕН | PnL={:+.4} | остаток ведём дальше",
                                bot_name, pnl
                            );
                            continue;
                        }
                        Ok(execution::ExecOutcome::ExitUnconfirmed) => {
                            tracing::warn!(
                                "[{}] закрытие не подтверждено — повтор на следующем тике",
                                bot_name
                            );
                            // 🔧 САМОСОЗНАНИЕ: судьба позиции неизвестна
                            strategy.on_position_unknown();
                        }
                        Ok(other) => {
                            if exit_signal.strategy_name == "Exit_UpdateSL" {
                                // qty=0 — только синхронизация биржевого стопа, не закрытие
                                tracing::info!("[{}] 🛡 биржевой стоп синхронизирован ({:?})", bot_name, other);
                            } else {
                                tracing::warn!("[{}] закрытие: неожиданный исход {:?}", bot_name, other);
                            }
                        }
                        Err(e) => {
                            if exit_signal.strategy_name == "Exit_UpdateSL" {
                                tracing::warn!("[{}] 🛡 синхронизация стопа не удалась: {:?}", bot_name, e);
                            } else {
                                tracing::error!("[{}] ❌ ошибка исполнения закрытия: {:?}", bot_name, e);
                            }
                        }
                    }
                    continue;
                }
                } // ✅ П3: конец if !is_backfill

                // ============ REGIME ============
                let regime = regime_detector.detect_from_msg(&message);
                metrics::MARKET_REGIME
                    .with_label_values(&[&bot_name, &format!("{:?}", regime)])
                    .set(1.0);

                // ============ SIGNAL ============
                // ✅ П3: backfill кормит только стратегии, попросившие историю
                // ✅ P41/clean: wants_backfill() — сегодня BounceTrader и keltner_dca, сигналы по
                // backfill у него запрещены внутри process()).
                let signal = if is_backfill && !strategy.wants_backfill() {
                    diag_gate_skips += 1;
                    if diag_gate_skips == 1 {
                        tracing::warn!(
                            "[{}] 🔍 DIAG: backfill отсечён ГЕЙТОМ — стратегия wants_backfill()=false (старый файл стратегии без блока V3.4?)",
                            bot_name
                        );
                    }
                    Ok(None)
                } else {
                    strategy_agent
                        .process(&message, strategy.as_mut())
                        .await
                };
                let signal = match signal {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!("[{}] Strategy: {}", bot_name, e);
                        continue;
                    }
                };

                if let Some(sig) = signal {
                    // ✅ P39/N3: собственный ВЫХОД стратегии (Exit_*+SELL) не
                    // гейтится «позиция уже открыта»: у keltner_dca in_position()
                    // =true именно когда Exit_KeltnerDca срабатывает, и guard
                    // глотал его — лестница не могла закрыться сама.
                    let is_strategy_exit =
                        sig.strategy_name.starts_with("Exit_") && sig.side == "SELL";
                    // ✅ ФИКС двойного входа: два источника истины
                    if !is_strategy_exit
                        && ((state_manager.has_position(&sig.symbol) && !strategy.can_dca())
                            || strategy.in_position())
                    {
                        // 🔧 САМОСОЗНАНИЕ: сигнал против открытой позиции —
                        // разворот запрещён (неттинг закрыл бы LONG частично),
                        // ждём Timeout/BE/SL — они закроют, после чего SELL пройдёт
                        tracing::info!(
                            "[{}] 🔇 сигнал {} проигнорирован — позиция {} уже открыта",
                            bot_name, sig.side, sig.symbol
                        );
                        strategy.on_entry_failed("позиция уже открыта — разворот запрещён");
                        continue;
                    }

                    metrics::SIGNALS_TOTAL
                        .with_label_values(&[&bot_name, &sig.side])
                        .inc();

                    // ✅ P39/N3: гейты входов не действуют на выход —
                    // KILL/suspend/риск-лимит не должны блокировать СНИЖЕНИЕ риска.
                    if !is_strategy_exit {
                        if risk_agent.is_suspended() {
                            metrics::SIGNALS_REJECTED
                                .with_label_values(&[&bot_name])
                                .inc();

                            static SUSPEND_LOG_COUNTER: std::sync::atomic::AtomicU64 =
                                std::sync::atomic::AtomicU64::new(0);
                            let n = SUSPEND_LOG_COUNTER
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                            if n % 100 == 0 {
                                tracing::warn!(
                                    "[{}] ⛔ SUSPENDED: {}",
                                    bot_name,
                                    risk_agent.suspend_reason().unwrap_or("EV ≤ 0")
                                );
                            }
                            continue;
                        }

                        if !risk_agent.check(&sig).await {
                            metrics::SIGNALS_REJECTED
                                .with_label_values(&[&bot_name])
                                .inc();
                            tracing::warn!("[{}] ⛔ Отклонён Risk Manager", bot_name);
                            continue;
                        }
                    }

                    tracing::info!(
                        "[{}] 🎯 СИГНАЛ: {} {:.5} {} @ {:.6}",
                        bot_name, sig.side, sig.quantity, sig.symbol, sig.price
                    );
                    metrics::SIGNALS_APPROVED
                        .with_label_values(&[&bot_name])
                        .inc();
                    // PATCH P15: KILL-switch + portfolio daily stop (entries-only gate)
                    // ✅ P39/N3: выход от гейта входов не зависит
                    if !is_strategy_exit {
                        if let Err(block_reason) = global_risk.entries_allowed() {
                            metrics::SIGNALS_REJECTED
                                .with_label_values(&[&bot_name])
                                .inc();
                            tracing::warn!(
                                "[{}] entries BLOCKED (KILL/daily-stop): {}",
                                bot_name, block_reason
                            );
                            continue;
                        }
                    }
                    let _ = trade_journal.record_signal(&sig).await;

                    match execution_agent.execute(sig.clone()).await {
                        Ok(execution::ExecOutcome::Entry { price, qty, .. }) => {
                            metrics::ORDERS_PLACED
                                .with_label_values(&[&bot_name])
                                .inc();
                            let _ = trade_journal
                                .record_execution(&sig, "PLACED")
                                .await;
                            strategy.notify_position_open();
                            tracing::info!(
                                "[{}] ✅ ВХОД: {} {:.5} @ {:.8}",
                                bot_name, sig.side, qty, price
                            );
                        }
                        Ok(execution::ExecOutcome::Exit(pnl)) if is_strategy_exit => {
                            // ✅ P39/N3: стратегический выход дошёл до движка —
                            // полный учёт. Раньше Exit(pnl) падал в Ok(other)
                            // с одним warn: без notify_position_closed, журнала,
                            // метрик и PnL-учёта (аудит N3).
                            strategy.notify_position_closed(pnl);
                            risk_agent.record_trade(&sig.symbol, pnl);
                            metrics::SUSPENDED.with_label_values(&[&bot_name]).set(
                                if risk_agent.is_suspended() { 1 } else { 0 }
                            );
                            let result = if pnl > 0.0 { "win" }
                                         else if pnl < 0.0 { "loss" }
                                         else { "flat" };
                            metrics::TRADES_CLOSED
                                .with_label_values(&[&bot_name, result])
                                .inc();
                            metrics::PNL_TOTAL
                                .with_label_values(&[&bot_name])
                                .add(pnl);
                            metrics::WIN_RATE
                                .with_label_values(&[&bot_name])
                                .set(risk_agent.win_rate());
                            metrics::PROFIT_FACTOR
                                .with_label_values(&[&bot_name])
                                .set(risk_agent.profit_factor());
                            let _ = trade_journal.record_close(&sig, pnl).await;
                            global_risk.add_closed_pnl(pnl); // PATCH P15
                            tracing::info!(
                                "🔔 [{}] СДЕЛКА ЗАКРЫТА (стратегия) | результат={} | PnL={:+.4} USDT",
                                bot_name, result, pnl
                            );
                        }
                        Ok(execution::ExecOutcome::ExitPartial(pnl)) if is_strategy_exit => {
                            // ✅ P39/N3 + P38/2c: частичное закрытие — остаток жив,
                            // кулдаун не включаем, позицию ведём дальше.
                            risk_agent.record_trade(&sig.symbol, pnl);
                            let _ = trade_journal.record_close(&sig, pnl).await;
                            global_risk.add_closed_pnl(pnl); // PATCH P15
                            metrics::PNL_TOTAL
                                .with_label_values(&[&bot_name])
                                .add(pnl);
                            tracing::warn!(
                                "⚠️ [{}] ВЫХОД ЧАСТИЧЕН (стратегия) | PnL={:+.4} | остаток ведём дальше",
                                bot_name, pnl
                            );
                        }
                        Ok(execution::ExecOutcome::Missed(reason)) => {
                            let _ = trade_journal
                                .record_execution(&sig, "MISSED")
                                .await;
                            tracing::info!("[{}] ⏭ вход не состоялся: {}", bot_name, reason);
                            // 🔧 САМОСОЗНАНИЕ: вернуться в FLAT с причиной
                            strategy.on_entry_failed(&reason);
                        }
                        Ok(other) => {
                            tracing::warn!("[{}] вход: неожиданный исход {:?}", bot_name, other);
                        }
                        Err(e) => {
                            tracing::error!("[{}] ❌ ошибка исполнения входа: {:?}", bot_name, e);
                            // ✅ P39/F2: откат оптимистичной мутации лестницы
                            strategy.on_entry_failed("execute error");
                        }
                    }
                }
            }

            // ============================================================
            // ✅ P53/H9-3: учёт закрытия БИРЖЕЙ — полный цикл, как у обычного
            // выхода: риск-статистика, метрики, журнал, P15 HARD HALT, сброс
            // фильтра стратегии (иначе in_position висит до рестарта).
            // ============================================================
            h9 = async {
                match h9_notify_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending::<Option<ExchangeClose>>().await,
                }
            } => {
                let Some(close) = h9 else {
                    // практически недостижимо (watcher паркуется, канал не
                    // закрывается до конца бота) — просто пропускаем тик
                    continue;
                };
                strategy.notify_position_closed(close.pnl);
                risk_agent.record_trade(&close.symbol, close.pnl);
                metrics::SUSPENDED.with_label_values(&[&bot_name]).set(
                    if risk_agent.is_suspended() { 1 } else { 0 }
                );
                let result = if close.pnl > 0.0 { "win" }
                             else if close.pnl < 0.0 { "loss" }
                             else { "flat" };
                metrics::TRADES_CLOSED
                    .with_label_values(&[&bot_name, result])
                    .inc();
                if close.pnl != 0.0 {
                    metrics::PNL_TOTAL
                        .with_label_values(&[&bot_name])
                        .add(close.pnl);
                }
                metrics::WIN_RATE
                    .with_label_values(&[&bot_name])
                    .set(risk_agent.win_rate());
                metrics::PROFIT_FACTOR
                    .with_label_values(&[&bot_name])
                    .set(risk_agent.profit_factor());
                let journal_sig = strategies::Signal {
                    symbol: close.symbol.clone(),
                    side: "CLOSE".to_string(),
                    price: close.exit_price,
                    quantity: close.qty,
                    strategy_name: format!("Exit_Exchange_{}", close.kind),
                    leverage: 1.0,
                    stop_loss: None,
                    take_profit: None,
                    atr_hint: 0.0,
                    entry_spread: 0.0,
                };
                let _ = trade_journal.record_close(&journal_sig, close.pnl).await;
                // ✅ P53/H9-4: портфельный HARD HALT (P54) теперь видит
                // биржевые лоссы немедленно, а не никогда.
                global_risk.add_closed_pnl(close.pnl);
                global_risk.unregister_position(&bot_name, &close.symbol);
                tracing::info!(
                    "🔔 [{}] СДЕЛКА ЗАКРЫТА БИРЖЕЙ (SL/TP исполнен) | результат={} | PnL={:+.4} USDT",
                    bot_name, result, close.pnl
                );
            }

            // ============================================================
            // POSITION CHECK (с партиал-веткой)
            // ============================================================
            _ = position_check_interval.tick() => {
                let positions = state_manager.get_positions();
                for pos in &positions {
                    if let Some(exit_signal) = state_manager
                        .check_and_update_position(&pos.symbol, pos.current_price)
                    {
                        match execution_agent.execute(exit_signal.clone()).await {
                            Ok(execution::ExecOutcome::Exit(pnl)) => {
                                if exit_signal.strategy_name == "Exit_Partial" {
                                    risk_agent.record_trade(&exit_signal.symbol, pnl);
                                    let _ = trade_journal
                                        .record_close(&exit_signal, pnl)
                                        .await;
                                    global_risk.add_closed_pnl(pnl); // PATCH P15
                                    metrics::PNL_TOTAL
                                        .with_label_values(&[&bot_name])
                                        .add(pnl);
                                    tracing::info!(
                                        "💵 [{}] PARTIAL ЗАФИКСИРОВАН (таймер) | PnL={:+.4} USDT",
                                        bot_name, pnl
                                    );
                                    continue;
                                }

                                strategy.notify_position_closed(pnl);
                                risk_agent.record_trade(&exit_signal.symbol, pnl);

                                metrics::SUSPENDED.with_label_values(&[&bot_name]).set(
                                    if risk_agent.is_suspended() { 1 } else { 0 }
                                );

                                let result = if pnl > 0.0 { "win" }
                                             else if pnl < 0.0 { "loss" }
                                             else { "flat" };
                                metrics::TRADES_CLOSED
                                    .with_label_values(&[&bot_name, result])
                                    .inc();
                                metrics::PNL_TOTAL
                                    .with_label_values(&[&bot_name])
                                    .add(pnl);
                                metrics::WIN_RATE
                                    .with_label_values(&[&bot_name])
                                    .set(risk_agent.win_rate());
                                metrics::PROFIT_FACTOR
                                    .with_label_values(&[&bot_name])
                                    .set(risk_agent.profit_factor());

                                let _ = trade_journal
                                    .record_close(&exit_signal, pnl)
                                    .await;
                                global_risk.add_closed_pnl(pnl); // PATCH P15

                                tracing::info!(
                                    "🔔 [{}] СДЕЛКА ЗАКРЫТА (по таймеру) | результат={} | PnL={:+.4}",
                                    bot_name, result, pnl
                                );
                            }
                            Ok(execution::ExecOutcome::ExitPartial(pnl)) => {
                                // ✅ P38/2c: закрылось частично — остаток жив,
                                // кулдаун НЕ включаем, позицию ведём дальше.
                                risk_agent.record_trade(&exit_signal.symbol, pnl);
                                let _ = trade_journal
                                    .record_close(&exit_signal, pnl)
                                    .await;
                                global_risk.add_closed_pnl(pnl); // PATCH P15
                                metrics::PNL_TOTAL
                                    .with_label_values(&[&bot_name])
                                    .add(pnl);
                                tracing::warn!(
                                    "⚠️ [{}] ВЫХОД ЧАСТИЧЕН (таймер) | PnL={:+.4} | остаток ведём дальше",
                                    bot_name, pnl
                                );
                                continue;
                            }
                            Ok(execution::ExecOutcome::ExitUnconfirmed) => {
                                tracing::warn!("[{}] закрытие (таймер) не подтверждено — повтор", bot_name);
                                // 🔧 САМОСОЗНАНИЕ: судьба позиции неизвестна
                                strategy.on_position_unknown();
                            }
                            Ok(other) => {
                                if exit_signal.strategy_name == "Exit_UpdateSL" {
                                    // qty=0 — только синхронизация биржевого стопа, не закрытие
                                    tracing::info!("[{}] 🛡 биржевой стоп синхронизирован ({:?})", bot_name, other);
                                } else {
                                    tracing::error!("[{}] закрытие (таймер): неожидаемый исход {:?}", bot_name, other);
                                }
                            }
                            Err(e) => {
                                if exit_signal.strategy_name == "Exit_UpdateSL" {
                                    tracing::warn!("[{}] 🛡 синхронизация стопа не удалась (повтор на следующем тике): {:?}", bot_name, e);
                                } else {
                                    tracing::error!("[{}] закрытие (таймер): {:?}", bot_name, e);
                                }
                            }
                        }
                    }
                }
            }

            // ============================================================
            // LOG STATE
            // ============================================================
            _ = log_interval.tick() => {
                state_manager.log_state();
                state_manager.persist_positions().await;
                                // 🔧 ЭТАП C: GROWTH/SURVIVAL/HALT → множитель → сайзинг стратегии
                // ✅ P60/EQ-2: живое equity (initial + realized PnL) → сайзинг
                // стратегий. Раньше канал P16 был мёртв: set_live_equity никто
                // не звал, стратегии сайзились от статического initial_balance
                // (после просадки риск в % от фактического капитала РОС).
                strategy.set_live_equity(risk_agent.live_equity());
                strategy.set_risk_multiplier(risk_agent.risk_multiplier());
                risk_agent.flush_stats().await;
                if let Some(diag) = strategy.diagnostics() {
                    tracing::info!("[{}] {}", bot_name, diag);
                }

                // ✅ v3.5: топ-ап из кэша хаба — добор свечей, не дошедших
                // по broadcast (норма: 0 каждый тик). Гарантирует актуальность
                // индикаторов даже при деградации broadcast-пути.
                let mut topup_fed: u64 = 0;
                for k in hub.recent_klines() {
                    if !k.is_closed || k.timestamp <= last_fed_candle_ts {
                        continue;
                    }
                    if k.is_backfill && !strategy.wants_backfill() {
                        continue;
                    }
                    last_fed_candle_ts = k.timestamp;
                    let msg = websocket::WebSocketMessage::Kline(k.clone());
                    match strategy_agent.process(&msg, strategy.as_mut()).await {
                        Ok(Some(_)) => {} // сигнал из добора не исполняется
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!("[{}] топ-ап: стратегия отклонила свечу: {}", bot_name, e);
                            break;
                        }
                    }
                    topup_fed += 1;
                }
                if topup_fed > 0 {
                    tracing::warn!(
                        "[{}] 🔁 топ-ап из кэша хаба: {} свечей (мимо broadcast — индикаторы актуальны)",
                        bot_name, topup_fed
                    );
                }
            }
        }
    }

    metrics::BOTS_ACTIVE.with_label_values(&[&bot_name]).set(0);
    Ok(())
}
