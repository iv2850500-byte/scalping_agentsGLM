//! ✅ Диагностика: какая форма подписки реально работает на fstream.binance.com.
//! Запуск: cargo run --release --bin ws_probe -- [symbol]   (по умолчанию solusdt)

use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const SECS: u64 = 15;

async fn probe(name: &str, url: &str, subscribe: Option<&str>) {
    tracing::info!("[{}] → {}", name, url);
    let ws = match tokio::time::timeout(Duration::from_secs(10), connect_async(url)).await {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(e)) => { tracing::error!("[{}] connect: {}", name, e); return; }
        Err(_) => { tracing::error!("[{}] connect: таймаут 10с", name); return; }
    };
    let (mut write, mut read) = ws.split();

    if let Some(payload) = subscribe {
        if let Err(e) = write.send(Message::Text(payload.to_string())).await {
            tracing::error!("[{}] subscribe send: {}", name, e);
            return;
        }
    }

    let (mut agg, mut book, mut kline, mut other) = (0u64, 0u64, 0u64, 0u64);
    let mut first: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(SECS);

    while let Ok(msg) = tokio::time::timeout_at(deadline, read.next()).await {
        let msg = match msg { Some(Ok(m)) => m, _ => break };
        let text = match msg {
            Message::Text(t) => t,
            Message::Binary(_) => { tracing::warn!("[{}] Binary-кадр (пропущен)", name); continue; }
            _ => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => { other += 1; continue; }
        };
        if v.get("result").is_some() { continue; } // ack на SUBSCRIBE
        let ev = v["data"]["e"].as_str().or_else(|| v["e"].as_str()).unwrap_or("?");
        if first.len() < 2 {
            let label = v.get("stream").and_then(|s| s.as_str()).unwrap_or("<без обёртки>");
            first.push(format!("stream={} e={}", label, ev));
        }
        match ev {
            "aggTrade" => agg += 1,
            "bookTicker" => book += 1,
            "kline" => kline += 1,
            _ => other += 1,
        }
    }
    tracing::info!("[{}] ИТОГ за {}с: aggTrade={} bookTicker={} kline={} другое={}",
        name, SECS, agg, book, kline, other);
    for f in first { tracing::info!("[{}]   кадр: {}", name, f); }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let sym = std::env::args().nth(1).unwrap_or_else(|| "solusdt".to_string());
    let s = sym.to_lowercase();
    tracing::info!("Пробник fstream для {}", s);

        // ===== Батарея 2: дифференциальная диагностика =====

    // A) Контроль: спотовый aggTrade — в работающем боте он приходит
    probe("A-спот-aggTrade-КОНТРОЛЬ",
        &format!("wss://stream.binance.com:9443/stream?streams={s}@aggTrade"),
        None).await;

    // B) Фьючерсный aggTrade по BTCUSDT — зависит ли от символа
    probe("B-fstream-btcusdt-aggTrade",
        "wss://fstream.binance.com/stream?streams=btcusdt@aggTrade",
        None).await;

    // C) Фьючерсный markPrice — другой тип стрима
    probe("C-fstream-markPrice",
        &format!("wss://fstream.binance.com/stream?streams={s}@markPrice"),
        None).await;

    // D) Фьючерсный miniTicker — ещё один тип
    probe("D-fstream-miniTicker",
        &format!("wss://fstream.binance.com/stream?streams={s}@miniTicker"),
        None).await;

    Ok(())
}