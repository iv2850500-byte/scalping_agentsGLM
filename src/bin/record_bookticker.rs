//! Recorder для Binance Futures bookTicker через WebSocket.
//! Пишет поток best_bid/best_ask в CSV по формату archives:
//!   update_id,best_bid_price,best_bid_qty,best_ask_price,best_ask_qty,transaction_time,event_time
//!
//! Запуск:
//!   cargo run --release --bin record_bookticker -- DOGEUSDT
//!
//! Файл растёт в data/bookticker_live/DOGEUSDT-YYYY-MM-DD.csv

use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use serde_json::Value;
use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio_tungstenite::connect_async;

const WS_BASE: &str = "wss://fstream.binance.com/ws";
const DATA_DIR: &str = "data/bookticker_live";

#[tokio::main]
async fn main() -> Result<()> {
    let symbol = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("Usage: record_bookticker <SYMBOL> (e.g. DOGEUSDT)"))?;
    let symbol_upper = symbol.to_uppercase();
    let symbol_lower = symbol.to_lowercase();

    create_dir_all(DATA_DIR)?;

    tracing_subscriber::fmt()
        .with_target(false)
        .with_env_filter("info")
        .init();

    let url = format!("{}/{}@bookTicker", WS_BASE, symbol_lower);
    tracing::info!("Recorder starting: {}", url);

    let mut current_file: Option<(String, std::fs::File)> = None;
    let mut rows_written: u64 = 0;
    let mut last_log = Instant::now();
    let mut reconnect_count: u32 = 0;

    loop {
        tracing::info!("Connecting to {}", url);
        let connect = connect_async(&url).await;

        let ws = match connect {
            Ok((ws, _)) => {
                tracing::info!("✅ Connected");
                reconnect_count = 0;
                ws
            }
            Err(e) => {
                reconnect_count += 1;
                let backoff = (2u64.pow(reconnect_count.min(6))).min(60);
                tracing::error!("Connection failed: {}. Retry in {}s", e, backoff);
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                continue;
            }
        };

        let (_, mut read) = ws.split();

        while let Some(msg) = read.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    tracing::error!("WS read error: {}", e);
                    break;
                }
            };

            if !msg.is_text() {
                continue;
            }

            let text = match msg.into_text() {
                Ok(t) => t,
                Err(_) => continue,
            };

            let json: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // Поля: e, u (update_id), s (symbol), b, B, a, A, T, E
            let update_id = json.get("u").and_then(|v| v.as_u64()).unwrap_or(0);
            let best_bid = json.get("b").and_then(|v| v.as_str()).unwrap_or("0");
            let bid_qty = json.get("B").and_then(|v| v.as_str()).unwrap_or("0");
            let best_ask = json.get("a").and_then(|v| v.as_str()).unwrap_or("0");
            let ask_qty = json.get("A").and_then(|v| v.as_str()).unwrap_or("0");
            let trans_time = json.get("T").and_then(|v| v.as_i64()).unwrap_or(0);
            let event_time = json.get("E").and_then(|v| v.as_i64()).unwrap_or(trans_time);

            // Определяем дату по event_time (UTC)
            let date = chrono::DateTime::from_timestamp_millis(event_time)
                .map(|dt| dt.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "unknown".to_string());

            // Roll файла при смене даты
            let need_new_file = match &current_file {
                Some((d, _)) => d != &date,
                None => true,
            };

            if need_new_file {
                if let Some((old_date, _)) = current_file.take() {
                    tracing::info!("Rolling file from {} to {}", old_date, date);
                }
                let path = PathBuf::from(DATA_DIR).join(format!("{}-{}.csv", symbol_upper, date));
                let new_file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)?;
                tracing::info!("Writing to {}", path.display());
                current_file = Some((date, new_file));
            }

            let line = format!(
                "{},{},{},{},{},{},{}\n",
                update_id, best_bid, bid_qty, best_ask, ask_qty, trans_time, event_time
            );

             if let Some((_, f)) = current_file.as_mut() {
                f.write_all(line.as_bytes())?;
                f.flush()?;              // ✅ Windows: сброс буфера после каждой строки
                rows_written += 1;
            }

            // Периодический лог + flush
            if last_log.elapsed() >= Duration::from_secs(10) {
                if let Some((_, f)) = current_file.as_mut() {
                    f.flush()?;
                }
                tracing::info!(
                    "Recorded {} rows | current file: {}",
                    rows_written,
                    current_file.as_ref().map(|(d, _)| d.as_str()).unwrap_or("—")
                );
                last_log = Instant::now();
            }
        }

        tracing::warn!("WS disconnected. Reconnecting...");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}