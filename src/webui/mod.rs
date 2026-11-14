//! ✅ WebUI: дашборд + JSON API + Prometheus на одном порту (9090).
//! v2: /api/klines (прокси свечей prod/testnet), /api/resume (команда боту),
//! Chart.js зашит в бинарник — дашборд работает без интернета.
//! v3: closes для графиков, fees в позициях.
//! v4: ✅ Баланс — availableBalance USDT с биржи, кеш 30с (один запрос
//!     на аккаунт, не на бота — ключи у трёх ботов одного аккаунта).
//! v5: ✅ /api/tvmarkers — закрытые сделки для TradingView-маркеров.

use crate::metrics::REGISTRY;
use prometheus::Encoder;
use serde_json::json;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

pub struct BotInfo {
    pub name: String,
    pub symbol: String,
    pub strategy: String,
    pub market: String,
    pub simulate: bool,
    pub interval: String,
    pub db_path: String,
    pub rest_url: String,
    // ✅ v4: для запроса баланса (тот же процесс, ключи уже в конфиге —
    // риск нулевой; наружу через API не отдаются)
    pub api_key: String,
    pub api_secret: String,
    // ✅ P41/UI2: дневной лимит бота — уходит в дашборд (кольца рисков
    // берут реальные лимиты из конфига, а не зашитые числа).
    pub max_daily_loss: f64,
}

struct Shared {
    bots: Vec<BotInfo>,
    journal_db: String,
    pools: Mutex<HashMap<String, SqlitePool>>,
    /// "market|SYMBOL|interval" → (момент кеширования, тело ответа)
    klines_cache: Mutex<HashMap<String, (std::time::Instant, String)>>,
    /// ✅ v4: api_key → (момент, balance). 30с — один запрос на аккаунт.
    balance_cache: Mutex<HashMap<String, (std::time::Instant, Option<f64>)>>,
}

// ============================================================
// AUDIT-P2: токен-гейт управляющих POST-команд (/api/resume).
// WEBUI_TOKEN задан -> команда только с верным ?token=...
// WEBUI_TOKEN не задан -> разрешаем, но ОДИН раз предупреждаем в лог
// (кнопка Resume в дашборде шлёт запрос без токена — не ломаем UX).
// Включить защиту: setx WEBUI_TOKEN <секрет>  (латиница/цифры,
// без спецсимволов — токен идёт в URL query), потом перезапуск бота.
// ============================================================
static RESUME_UNPROTECTED_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn webui_token_ok(query: &str) -> bool {
    use std::sync::atomic::Ordering;
    let expected = match std::env::var("WEBUI_TOKEN") {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            if !RESUME_UNPROTECTED_WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!("WebUI: WEBUI_TOKEN не задан — /api/resume БЕЗ защиты (drive-by CSRF возможен). Включить: setx WEBUI_TOKEN <секрет>");
            }
            return true;
        }
    };
    let given = query.split("&").find_map(|kv| {
        kv.split_once("=")
            .filter(|(k, _)| *k == "token")
            .map(|(_, v)| v.to_string())
    });
    match given {
        Some(g) if g == expected => true,
        _ => {
            tracing::warn!("WebUI: POST отклонён — token отсутствует/неверен (ожидался WEBUI_TOKEN)");
            false
        }
    }
}

pub fn spawn(bots: Vec<BotInfo>, journal_db: String) -> JoinHandle<()> {
    tokio::spawn(async move {
        let shared = Arc::new(Shared {
            bots,
            journal_db,
            pools: Mutex::new(HashMap::new()),
            klines_cache: Mutex::new(HashMap::new()),
            balance_cache: Mutex::new(HashMap::new()),
        });

        let listener = match TcpListener::bind("127.0.0.1:9090").await { // AUDIT-P2: только localhost
            Ok(l) => l,
            Err(e) => {
                tracing::error!("WebUI: не удалось занять порт 9090: {}", e);
                return;
            }
        };
        tracing::info!("WebUI server on 127.0.0.1:9090 (AUDIT-P2: доступ только с этой машины)");

        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => { tracing::error!("WebUI accept: {}", e); continue; }
            };
            let shared = shared.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();

                let first_line = req.lines().next().unwrap_or("");
                let mut parts = first_line.split(' ');
                let method = parts.next().unwrap_or("GET").to_string();
                let raw_path = parts.next().unwrap_or("/").to_string();
                let (path, query) = match raw_path.split_once('?') {
                    Some((p, q)) => (p.to_string(), q.to_string()),
                    None => (raw_path, String::new()),
                };

                let (body, ctype) = if method == "POST" {
                    // AUDIT-P2: токен-гейт управляющих POST-команд (drive-by CSRF:
                    // сторонняя страница в браузере может POST-ить на localhost:9090).
                    if !webui_token_ok(&query) {
                        (r#"{"ok":false,"error":"unauthorized: WEBUI_TOKEN"}"#.into(), "application/json")
                    } else {
                        match path.as_str() {
                            "/api/resume" => (resume_command(&shared, &query).await, "application/json"),
                            _ => (r#"{"ok":false,"error":"not found"}"#.into(), "application/json"),
                        }
                    }
                } else {
                    match path.as_str() {
                        "/metrics" => {
                            let mut out = Vec::new();
                            let _ = REGISTRY.register(Box::new(prometheus::IntGaugeVec::clone(&crate::metrics::BOTS_ACTIVE)));
                            let encoder = prometheus::TextEncoder::new();
                            encoder.encode(&REGISTRY.gather(), &mut out).unwrap_or_default();
                            (String::from_utf8_lossy(&out).to_string(), "text/plain; version=0.0.4")
                        }
                        "/api/dashboard" => (dashboard_json(&shared).await, "application/json"),
                        "/api/tvmarkers" => (tv_markers_json(&shared).await, "application/json"),
                        "/api/klines" => (klines_json(&shared, &query).await, "application/json"),
                        "/prism" => (include_str!("prism.html").to_string(), "text/html; charset=utf-8"),
                        "/chart.umd.min.js" => {
                            (include_str!("chart.umd.min.js").to_string(), "application/javascript; charset=utf-8")
                        }
                        _ => (include_str!("dashboard.html").to_string(), "text/html; charset=utf-8"),
                    }
                };

                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
                    ctype,
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    })
}

/// ✅ Ф3: команда resume для бота.
async fn resume_command(shared: &Shared, query: &str) -> String {
    let mut bot_name = String::new();
    for kv in query.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            if k == "bot" { bot_name = v.to_string(); }
        }
    }
    if bot_name.is_empty() {
        return r#"{"ok":false,"error":"bot required"}"#.into();
    }
    let Some(b) = shared.bots.iter().find(|b| b.name == bot_name) else {
        return r#"{"ok":false,"error":"unknown bot"}"#.into();
    };
    let url = format!("sqlite:{}?mode=rwc", b.db_path);
    let pool = match SqlitePool::connect(&url).await {
        Ok(p) => p,
        Err(e) => return format!(r#"{{"ok":false,"error":"db: {}"}}"#, e),
    };
    let _ = sqlx::query(
        "CREATE TABLE IF NOT EXISTS bot_commands (\
         id INTEGER PRIMARY KEY AUTOINCREMENT, \
         command TEXT NOT NULL, \
         created_at TEXT NOT NULL, \
         consumed INTEGER NOT NULL DEFAULT 0)",
    )
    .execute(&pool)
    .await;
    match sqlx::query("INSERT INTO bot_commands (command, created_at) VALUES ('resume', ?)")
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&pool)
        .await
    {
        Ok(_) => r#"{"ok":true}"#.into(),
        Err(e) => format!(r#"{{"ok":false,"error":"{}"}}"#, e),
    }
}

// ============================================================
// ✅ v4: Баланс — availableBalance USDT с фьючерсного REST.
// Та же подпись HMAC-SHA256, что в PowerShell-сверках и клиентах.
// None при любой ошибке → дашборд покажет «—».
// ============================================================
async fn binance_futures_balance(api_key: &str, api_secret: &str, rest_url: &str) -> Option<f64> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let ts = chrono::Utc::now().timestamp_millis();
    let query = format!("timestamp={}&recvWindow=5000", ts);
    let mut mac = Hmac::<Sha256>::new_from_slice(api_secret.as_bytes()).ok()?;
    mac.update(query.as_bytes());
    let sig: String = mac.finalize().into_bytes().iter().map(|b| format!("{:02x}", b)).collect();
    let url = format!("{}/fapi/v2/balance?{}&signature={}",
        rest_url.trim_end_matches('/'), query, sig);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8)).pool_idle_timeout(std::time::Duration::from_secs(20)).tcp_keepalive(std::time::Duration::from_secs(15))/* P19-POOL */.build().ok()?;
    let resp: serde_json::Value = client.get(&url)
        .header("X-MBX-APIKEY", api_key).send().await.ok()?
        .json().await.ok()?;
    resp.as_array()?
        .iter().find(|b| b["asset"].as_str() == Some("USDT"))?
        ["availableBalance"].as_str()?.parse().ok()
}

/// ✅ Ф1: прокси свечей Binance Futures.
// ============================================================
// [OKX-MIG] OKX support: config probe, balance, klines proxy.
// Reads config.toml directly: exchange = "okx" + [okx] section
// (api_key / api_secret / passphrase). One OKX account for all
// bots - same idea as the shared Binance balance cache. When
// config.toml says binance (or [okx] absent) everything falls
// back to the original Binance behavior.
// ============================================================
fn read_okx_cfg() -> (bool, Option<(String, String, String)>) {
    // P46/W10: warn instead of silently serving an empty config panel
let txt = std::fs::read_to_string("config.toml").unwrap_or_else(|e| {
    tracing::warn!("W10: config.toml unreadable ({e}) - config panel served empty");
    String::new()
});
    let mut exchange = String::new();
    let mut k = String::new();
    let mut s = String::new();
    let mut p = String::new();
    let mut in_okx = false;
    for line in txt.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') { continue; }
        if t.starts_with('[') { in_okx = t.to_lowercase().starts_with("[okx"); continue; }
        if let Some((key, raw)) = t.split_once('=') {
            let key = key.trim().to_lowercase();
            let raw = raw.trim();
            let val = if raw.starts_with('"') {
                raw[1..].split('"').next().unwrap_or("").to_string()
            } else {
                raw.split('#').next().unwrap_or("").trim().to_string()
            };
            if in_okx {
                match key.as_str() {
                    "api_key" | "key" => k = val,
                    "api_secret" | "secret" => s = val,
                    "passphrase" | "pass" => p = val,
                    _ => {}
                }
            } else if key == "exchange" && exchange.is_empty() {
                exchange = val;
            }
        }
    }
    let creds = if k.is_empty() || s.is_empty() || p.is_empty() { None } else { Some((k, s, p)) };
    (exchange == "okx", creds)
}

/// [OKX-MIG] minimal base64 encoder (keeps Cargo.toml dependency-free).
fn okx_b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// [OKX-MIG] availableBalance USDT from OKX futures account.
/// Sign = base64(hmac_sha256(timestamp + "GET" + requestPath, secret)).
async fn okx_balance(api_key: &str, api_secret: &str, passphrase: &str) -> Option<f64> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    const BASE: &str = "https://www.okx.com";
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    let path = "/api/v5/account/balance?ccy=USDT";
    let prehash = format!("{}GET{}", ts, path);
    let mut mac = Hmac::<Sha256>::new_from_slice(api_secret.as_bytes()).ok()?;
    mac.update(prehash.as_bytes());
    let sig = okx_b64(&mac.finalize().into_bytes());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8)).pool_idle_timeout(std::time::Duration::from_secs(20)).tcp_keepalive(std::time::Duration::from_secs(15))/* P19-POOL */.build().ok()?;
    let resp: serde_json::Value = client.get(&format!("{}{}", BASE, path))
        .header("OK-ACCESS-KEY", api_key)
        .header("OK-ACCESS-SIGN", sig)
        .header("OK-ACCESS-TIMESTAMP", ts)
        .header("OK-ACCESS-PASSPHRASE", passphrase)
        .header("Content-Type", "application/json")
        .send().await.ok()?.json().await.ok()?;
    if resp["code"].as_str() != Some("0") { return None; }
    let details = resp["data"][0]["details"].as_array()?;
    for d in details {
        if d["ccy"].as_str() == Some("USDT") {
            let v = d["availEq"].as_str().or_else(|| d["availBal"].as_str())?;
            return v.parse::<f64>().ok();
        }
    }
    None
}

/// [OKX-MIG] OKX candles converted to Binance kline format,
/// so dashboard.html needs no changes. Cache: same klines_cache.
async fn okx_klines(symbol: &str, interval: &str, shared: &Shared, cache_key: &str) -> String {
    use std::time::{Duration, Instant};
    let inst = match symbol.strip_suffix("USDT") {
        Some(s) => format!("{}-USDT-SWAP", s),
        None => return format!("{{\"error\":\"bad okx symbol: {}\"}}", symbol),
    };
    let bar = match interval {
        "1h" => "1H", "2h" => "2H", "4h" => "4H", "6h" => "6H",
        "12h" => "12H", "1d" => "1D", other => other,
    };
    let ms: i64 = match interval {
        "1m" => 60_000, "3m" => 180_000, "5m" => 300_000, "15m" => 900_000,
        "30m" => 1_800_000, "1h" => 3_600_000, "2h" => 7_200_000,
        "4h" => 14_400_000, "6h" => 21_600_000, "12h" => 43_200_000,
        "1d" => 86_400_000, _ => 900_000,
    };
    let url = format!("https://www.okx.com/api/v5/market/candles?instId={}&bar={}&limit=200", inst, bar);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10)).pool_idle_timeout(std::time::Duration::from_secs(20)).tcp_keepalive(std::time::Duration::from_secs(15))/* P19-POOL */.build().unwrap_or_default();
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
            Ok(v) if v["code"].as_str() == Some("0") => {
                let mut out: Vec<serde_json::Value> = Vec::new();
                if let Some(rows) = v["data"].as_array() {
                    for r in rows {
                        if let Some(a) = r.as_array() {
                            if a.len() < 8 { continue; }
                            let g = |i: usize| a[i].as_str().unwrap_or("0").to_string();
                            let ts0: i64 = a[0].as_str().unwrap_or("0").parse().unwrap_or(0);
                            // OKX row: [ts,o,h,l,c,vol(contracts),volCcy(base),volCcyQuote,confirm]
                            // Binance: [openTime,o,h,l,c,vol(base),closeTime,quoteVol,...]
                            out.push(json!([
                                ts0, g(1), g(2), g(3), g(4), g(6),
                                ts0 + ms - 1, g(7), 0, "0", "0", "0"
                            ]));
                        }
                    }
                }
                out.reverse();
                let body = serde_json::to_string(&out).unwrap_or_else(|_| "[]".to_string());
                shared.klines_cache.lock().unwrap()
                    .insert(cache_key.to_string(), (Instant::now(), body.clone()));
                body
            }
            Ok(v) => format!("{{\"error\":\"okx code: {}\"}}", v["code"].as_str().unwrap_or("?")),
            Err(_) => "{\"error\":\"okx body read failed\"}".to_string(),
        },
        _ => "{\"error\":\"okx upstream unavailable\"}".to_string(),
    }
}

async fn klines_json(shared: &Shared, query: &str) -> String {
    use std::time::{Duration, Instant};

    let mut symbol = String::new();
    let mut interval = "15m".to_string();
    let mut market = "prod".to_string();
    for kv in query.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            match k {
                "symbol" => symbol = v.to_uppercase(),
                "interval" => interval = v.to_string(),
                "market" => market = v.to_lowercase(),
                _ => {}
            }
        }
    }

    if symbol.is_empty() {
        return r#"{"error":"symbol required"}"#.into();
    }
    if !["1m","3m","5m","15m","30m","1h","2h","4h","6h","12h","1d"].contains(&interval.as_str()) {
        return r#"{"error":"bad interval"}"#.into();
    }

    let cache_key = format!("{}|{}|{}", market, symbol, interval);
    {
        let cache = shared.klines_cache.lock().unwrap();
        if let Some((t, body)) = cache.get(&cache_key) {
            if t.elapsed() < Duration::from_secs(30) {
                return body.clone();
            }
        }
    }

    // [OKX-MIG] route to OKX candles (converted to Binance format) when config.toml says okx
    let (okx_on, _) = read_okx_cfg();
    if okx_on {
        return okx_klines(&symbol, &interval, shared, &cache_key).await;
    }

    let base = match market.as_str() {
        "testnet" => shared.bots.iter()
            .find(|b| b.market == "futures")
            .map(|b| b.rest_url.trim_end_matches('/').to_string())
            .unwrap_or_else(|| "https://testnet.binancefuture.com".to_string()),
        _ => "https://fapi.binance.com".to_string(),
    };

    let url = format!("{}/fapi/v1/klines?symbol={}&interval={}&limit=200", base, symbol, interval);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .pool_idle_timeout(std::time::Duration::from_secs(20)).tcp_keepalive(std::time::Duration::from_secs(15))/* P19-POOL */.build()
        .unwrap_or_default();

    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.text().await {
            Ok(body) => {
                shared.klines_cache.lock().unwrap().insert(cache_key, (Instant::now(), body.clone()));
                body
            }
            Err(_) => r#"{"error":"body read failed"}"#.into(),
        },
        _ => r#"{"error":"upstream unavailable"}"#.into(),
    }
}

/// 🔧 v5: TradingView-маркеры — закрытые сделки из журнала
/// в формате [{bot, time, symbol, side, price, pnl}].
/// Используется вкладкой «График» (TV-режим) для точек поверх графика.
async fn tv_markers_json(shared: &Shared) -> String {
    let mut out = Vec::new();
    if let Some(pool) = pool_for(shared, &shared.journal_db).await {
        if let Ok(rows) = sqlx::query(
            "SELECT bot_name, timestamp, symbol, side, price, pnl \
             FROM trade_journal WHERE event = 'CLOSE' AND price > 0 \
             ORDER BY id DESC LIMIT 200",
        )
        .fetch_all(&pool)
        .await
        {
            for r in rows {
                out.push(json!({
                    "bot": r.try_get::<String, _>("bot_name").unwrap_or_default(),
                    "time": r.try_get::<String, _>("timestamp").unwrap_or_default(),
                    "symbol": r.try_get::<String, _>("symbol").unwrap_or_default(),
                    "side": r.try_get::<String, _>("side").unwrap_or_default(),
                    "price": r.try_get::<f64, _>("price").unwrap_or(0.0),
                    "pnl": r.try_get::<Option<f64>, _>("pnl").ok().flatten().unwrap_or(0.0),
                }));
            }
        }
    }
    json!(out).to_string()
}

/// Пул к БД с кешированием.
async fn pool_for(shared: &Shared, db: &str) -> Option<SqlitePool> {
    {
        let pools = shared.pools.lock().unwrap();
        if let Some(p) = pools.get(db) {
            return Some(p.clone());
        }
    }
    let url = format!("sqlite:{}?mode=rw", db);
    match SqlitePool::connect(&url).await {
        Ok(p) => {
            shared.pools.lock().unwrap().insert(db.to_string(), p.clone());
            Some(p)
        }
        Err(_) => None,
    }
}

async fn dashboard_json(shared: &Shared) -> String {
    let mut bots = Vec::new();
    // ✅ P41/UI3: балансы по аккаунтам (api_key → баланс) для честного equity:
    // боты с общим ключом раньше суммировались ×N.
    let mut account_balances: Vec<(String, f64)> = Vec::new();
    let (okx_on, okx_creds) = read_okx_cfg();

    for b in &shared.bots {
        let mut positions = Vec::new();
        if let Some(pool) = pool_for(shared, &b.db_path).await {
            if let Ok(rows) = sqlx::query(
                "SELECT symbol, side, entry_price, quantity, current_price, stop_loss, \
                 take_profit, opened_at, origin_strategy, funding_accrued, fees_paid \
                 FROM positions WHERE bot_name = ?",
            )
            .bind(&b.name)
            .fetch_all(&pool)
            .await
            {
                for r in rows {
                    let side: String = r.try_get("side").unwrap_or_default();
                    let entry: f64 = r.try_get("entry_price").unwrap_or(0.0);
                    let cur: f64 = r.try_get("current_price").unwrap_or(entry);
                    let qty: f64 = r.try_get("quantity").unwrap_or(0.0);
                    let upnl = if side == "BUY" { (cur - entry) * qty } else { (entry - cur) * qty };
                    positions.push(json!({
                        "symbol": r.try_get::<String, _>("symbol").unwrap_or_default(),
                        "side": side,
                        "entry": entry,
                        "qty": qty,
                        "current": cur,
                        "sl": r.try_get::<Option<f64>, _>("stop_loss").ok().flatten(),
                        "tp": r.try_get::<Option<f64>, _>("take_profit").ok().flatten(),
                        "opened_at": r.try_get::<String, _>("opened_at").unwrap_or_default(),
                        "origin": r.try_get::<String, _>("origin_strategy").unwrap_or_default(),
                        "funding": r.try_get::<f64, _>("funding_accrued").unwrap_or(0.0),
                        "fees": r.try_get::<f64, _>("fees_paid").unwrap_or(0.0),
                        "upnl": upnl,
                    }));
                }
            }
        }

        let mut risk = json!({});
        if let Some(pool) = pool_for(shared, &b.db_path).await {
            if let Ok(row) = sqlx::query(
                "SELECT realized_pnl, total_trades, win_trades, loss_trades, daily_loss, \
                 suspended, suspend_reason FROM risk_stats WHERE bot_name = ?",
            )
            .bind(&b.name)
            .fetch_one(&pool)
            .await
            {
                let pnl: String = row.try_get("realized_pnl").unwrap_or_else(|_| "0".into());
                let dl: String = row.try_get("daily_loss").unwrap_or_else(|_| "0".into());
                let total: i64 = row.try_get("total_trades").unwrap_or(0);
                let wins: i64 = row.try_get("win_trades").unwrap_or(0);
                let wr = if total > 0 { wins as f64 / total as f64 * 100.0 } else { 0.0 };
                risk = json!({
                    "pnl": pnl.parse::<f64>().unwrap_or(0.0),
                    "trades": total,
                    "wins": wins,
                    "losses": total - wins,
                    "win_rate": wr,
                    "daily_loss": dl.parse::<f64>().unwrap_or(0.0),
                    "suspended": row.try_get::<i64, _>("suspended").unwrap_or(0) != 0,
                    "suspend_reason": row.try_get::<String, _>("suspend_reason").unwrap_or_default(),
                });
            }
        }

        // Баланс — кеш 30с ПО api_key
        let balance = {
            let cached = {
                let c = shared.balance_cache.lock().unwrap();
                c.get(&b.api_key)
                    .filter(|(t, _)| t.elapsed() < std::time::Duration::from_secs(30))
                    .map(|(_, v)| *v)
            };
            match cached {
                Some(v) => v,
                None => {
                    let v = if b.market == "futures" && !b.simulate {
                        if okx_on {
                            match &okx_creds {
                                Some((k, s, p)) => okx_balance(k, s, p).await,
                                None => None,
                            }
                        } else {
                            binance_futures_balance(&b.api_key, &b.api_secret, &b.rest_url).await
                        }
                    } else { None };
                    shared.balance_cache.lock().unwrap()
                        .insert(b.api_key.clone(), (std::time::Instant::now(), v));
                    v
                }
            }
        };

        if let Some(v) = balance {
            // один аккаунт может быть у нескольких ботов — берём максимум
            // (одно и то же хранилище, разные боты видят один баланс)
            match account_balances.iter_mut().find(|(k, _)| *k == b.api_key) {
                Some((_, v0)) => {
                    if v > *v0 {
                        *v0 = v;
                    }
                }
                None => account_balances.push((b.api_key.clone(), v)),
            }
        }

        bots.push(json!({
            "name": b.name,
            "symbol": b.symbol,
            "strategy": b.strategy,
            "market": b.market,
            "simulate": b.simulate,
            "interval": b.interval,
            "balance": balance,
            "positions": positions,
            "risk": risk,
        }));
    }

    // журнал: trades (150, все события) + closes (500, только CLOSE)
    let mut trades = Vec::new();
    let mut closes = Vec::new();
    if let Some(pool) = pool_for(shared, &shared.journal_db).await {
        if let Ok(rows) = sqlx::query(
            "SELECT bot_name, timestamp, event, symbol, side, price, quantity, pnl \
             FROM trade_journal ORDER BY id DESC LIMIT 150",
        )
        .fetch_all(&pool)
        .await
        {
            for r in rows {
                trades.push(json!({
                    "bot": r.try_get::<String, _>("bot_name").unwrap_or_default(),
                    "time": r.try_get::<String, _>("timestamp").unwrap_or_default(),
                    "event": r.try_get::<String, _>("event").unwrap_or_default(),
                    "symbol": r.try_get::<String, _>("symbol").unwrap_or_default(),
                    "side": r.try_get::<String, _>("side").unwrap_or_default(),
                    "price": r.try_get::<f64, _>("price").unwrap_or(0.0),
                    "qty": r.try_get::<f64, _>("quantity").unwrap_or(0.0),
                    "pnl": r.try_get::<Option<f64>, _>("pnl").ok().flatten(),
                }));
            }
        }
        if let Ok(rows) = sqlx::query(
            "SELECT bot_name, timestamp, symbol, side, price, quantity, pnl \
             FROM trade_journal WHERE event = 'CLOSE' ORDER BY id DESC LIMIT 500",
        )
        .fetch_all(&pool)
        .await
        {
            for r in rows {
                closes.push(json!({
                    "bot": r.try_get::<String, _>("bot_name").unwrap_or_default(),
                    "time": r.try_get::<String, _>("timestamp").unwrap_or_default(),
                    "event": "CLOSE",
                    "symbol": r.try_get::<String, _>("symbol").unwrap_or_default(),
                    "side": r.try_get::<String, _>("side").unwrap_or_default(),
                    "price": r.try_get::<f64, _>("price").unwrap_or(0.0),
                    "qty": r.try_get::<f64, _>("quantity").unwrap_or(0.0),
                    "pnl": r.try_get::<Option<f64>, _>("pnl").ok().flatten(),
                }));
            }
        }
    }

    // ✅ P41/UI3: equity = сумма по УНИКАЛЬНЫМ аккаунтам (не по ботам).
    let equity: f64 = account_balances.iter().map(|(_, v)| v).sum();

    // ✅ P41/UI2: лимиты из реальной конфигурации. Weekly-лимита в конфиге
    // нет (null → дашборд оставляет ориентир), max_dd_pct — PROP-GUARD из .env.
    let daily_lim: f64 = shared.bots.iter().map(|b| b.max_daily_loss).sum();
    let max_dd_pct = std::env::var("PROP_MAX_DD_PCT")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|x| (0.01..=0.50).contains(x))
        .map(|x| (x * 100.0 * 100.0).round() / 100.0)
        .unwrap_or(20.0);

    json!({
        "bots": bots,
        "trades": trades,
        "closes": closes,
        "equity": (equity * 100.0).round() / 100.0,
        "limits": {
            "daily": (daily_lim * 100.0).round() / 100.0,
            "weekly": Option::<f64>::None,
            "max_dd_pct": max_dd_pct,
        },
        "generated_at": chrono::Utc::now().to_rfc3339(),
    })
    .to_string()
}