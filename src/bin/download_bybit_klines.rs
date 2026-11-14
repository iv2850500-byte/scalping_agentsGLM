//! download_bybit_klines — утилита скачивания klines с Bybit V5 PROD.

use reqwest::Client;
use serde_json::Value;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Duration;
use chrono::{Utc, TimeZone};

const BYBIT_KLINES: &str = "https://api.bybit.com/v5/market/kline";
const USER_AGENT: &str = "Mozilla/5.0 (scalping_agents/klines-downloader) reqwest/0.12";

fn interval_to_bybit(interval: &str) -> Option<&'static str> {
    match interval {
        "1m" => Some("1"), "5m" => Some("5"), "15m" => Some("15"), "30m" => Some("30"),
        "1h" => Some("60"), "2h" => Some("120"), "4h" => Some("240"),
        "1d" => Some("D"), "1w" => Some("W"), "1M" => Some("M"), _ => None,
    }
}

fn interval_seconds(interval: &str) -> Option<i64> {
    match interval {
        "1m" => Some(60), "5m" => Some(300), "15m" => Some(900), "30m" => Some(1800),
        "1h" => Some(3600), "2h" => Some(7200), "4h" => Some(14400),
        "1d" => Some(86400), "1w" => Some(604800), _ => None,
    }
}

fn parse_args() -> Result<(String, String, i64, PathBuf), String> {
    let args: Vec<String> = std::env::args().collect();
    let mut symbol = String::new();
    let mut interval = String::from("1h");
    let mut days: i64 = 365;
    let mut out = PathBuf::from("data/klines");
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--symbol" => { symbol = args.get(i+1).ok_or("missing symbol")?.clone(); i+=2; }
            "--interval" => { interval = args.get(i+1).ok_or("missing interval")?.clone(); i+=2; }
            "--days" => { days = args.get(i+1).ok_or("missing days")?.parse().map_err(|e| format!("{}",e))?; i+=2; }
            "--out" => { out = PathBuf::from(args.get(i+1).ok_or("missing out")?); i+=2; }
            "-h" | "--help" => {
                eprintln!("Usage: download_bybit_klines --symbol XAUUSDT --interval 1h --days 365 --out data/klines");
                std::process::exit(0);
            }
            _ => { eprintln!("Unknown arg: {}", args[i]); i+=1; }
        }
    }
    if symbol.is_empty() { return Err("--symbol required".into()); }
    Ok((symbol, interval, days, out))
}

async fn fetch_klines(
    client: &Client, symbol: &str, interval_bybit: &str,
    start_ms: i64, end_ms: i64, limit: u32,
) -> Result<Vec<Value>, String> {
    let url = format!(
        "{}/v5/market/kline?category=linear&symbol={}&interval={}&start={}&end={}&limit={}",
        BYBIT_KLINES, symbol.to_uppercase(), interval_bybit, start_ms, end_ms, limit
    );

    // 🆕 ФИКС: используем .text() для лучшей диагностики
    let resp = client.get(&url).send().await.map_err(|e| format!("HTTP: {}", e))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("TEXT: {}", e))?;

    if !status.is_success() {
        return Err(format!("HTTP {}: {}", status, &text[..text.len().min(500)]));
    }

    let json: Value = serde_json::from_str(&text)
        .map_err(|e| format!("JSON parse failed: {} | body[:200]: {}", e, &text[..text.len().min(200)]))?;

    let ret_code = json["retCode"].as_i64().unwrap_or(-1);
    if ret_code != 0 {
        return Err(format!("Bybit retCode={} msg={}", ret_code, json["retMsg"].as_str().unwrap_or("?")));
    }

    Ok(json["result"]["list"].as_array().cloned().unwrap_or_default())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (symbol, interval, days, out_dir) = parse_args()?;
    let interval_bybit = interval_to_bybit(&interval).ok_or(format!("Bad interval: {}", interval))?;
    let interval_secs = interval_seconds(&interval).ok_or(format!("Bad interval secs: {}", interval))?;

    let end_date = Utc::now();
    let start_date = end_date - chrono::Duration::days(days);
    let end_ms = end_date.timestamp_millis();
    let start_ms = start_date.timestamp_millis();

    println!("=== Bybit klines downloader (Rust) ===");
    println!("Symbol:    {}", symbol);
    println!("Interval:  {} → Bybit '{}' ({}с)", interval, interval_bybit, interval_secs);
    println!("Range:     {} → {}", start_date.format("%Y-%m-%d"), end_date.format("%Y-%m-%d"));
    println!("Days:      {}", days);
    println!("Out:       {}", out_dir.display());
    println!();

    std::fs::create_dir_all(&out_dir)?;
    let out_file = out_dir.join(format!(
        "{}-{}-{}_{}.csv",
        symbol.to_uppercase(), interval,
        start_date.format("%Y%m%d").to_string(),
        end_date.format("%Y%m%d").to_string()
    ));

    // 🆕 ФИКС: явный User-Agent + Accept header — Bybit блокирует дефолтный UA
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("Accept", reqwest::header::HeaderValue::from_static("application/json"));
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(USER_AGENT)
        .default_headers(headers)
        .pool_idle_timeout(std::time::Duration::from_secs(20)).tcp_keepalive(std::time::Duration::from_secs(15))/* P19-POOL */.build()?;

    let mut all_rows: Vec<(i64, String, String, String, String, String, i64, String, i64, String, String, i64)> = Vec::new();
    let mut cursor_end_ms = end_ms;
    let mut request_count = 0;

    while cursor_end_ms > start_ms {
        request_count += 1;
        let list = match fetch_klines(&client, &symbol, interval_bybit, start_ms, cursor_end_ms, 1000).await {
            Ok(v) => v,
            Err(e) => {
                println!("  ⚠ запрос {}: ошибка: {}", request_count, e);
                break;
            }
        };
        if list.is_empty() {
            println!("  запрос {}: 0 свечей — конец", request_count);
            break;
        }

        let mut sorted: Vec<Value> = list.clone();
        sorted.reverse();

        let mut new_count = 0;
        let mut oldest_ms = i64::MAX;
        let mut newest_ms = i64::MIN;
        for row in &sorted {
            let start_str = row[0].as_str().unwrap_or("0");
            let start_ms_val: i64 = start_str.parse().unwrap_or(0);
            if start_ms_val < start_ms || start_ms_val > end_ms { continue; }
            let open_time_ms = start_ms_val;
            let close_time_ms = open_time_ms + interval_secs * 1000 - 1;
            if all_rows.iter().any(|r| r.0 == open_time_ms) { continue; }

            let open = row[1].as_str().unwrap_or("").to_string();
            let high = row[2].as_str().unwrap_or("").to_string();
            let low = row[3].as_str().unwrap_or("").to_string();
            let close = row[4].as_str().unwrap_or("").to_string();
            let volume = row[5].as_str().unwrap_or("").to_string();
            let turnover = row[6].as_str().unwrap_or("").to_string();

            all_rows.push((open_time_ms, open, high, low, close, volume, close_time_ms, turnover, 0, "0".to_string(), "0".to_string(), 0));
            new_count += 1;
            if open_time_ms < oldest_ms { oldest_ms = open_time_ms; }
            if open_time_ms > newest_ms { newest_ms = open_time_ms; }
        }

        let oldest_dt = Utc.timestamp_millis_opt(oldest_ms).unwrap();
        let newest_dt = Utc.timestamp_millis_opt(newest_ms).unwrap();
        println!("  запрос {}: {} свечей | {} → {} | итого {}",
                 request_count, new_count,
                 oldest_dt.format("%Y-%m-%d %H:%M"),
                 newest_dt.format("%Y-%m-%d %H:%M"),
                 all_rows.len());

        if new_count == 0 || sorted.len() < 1000 { break; }
        cursor_end_ms = oldest_ms - 1;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    if all_rows.is_empty() {
        eprintln!("❌ Нет свечей");
        std::process::exit(1);
    }

    all_rows.sort_by_key(|r| r.0);

    let mut f = BufWriter::new(File::create(&out_file)?);
    writeln!(f, "open_time,open,high,low,close,volume,close_time,quote_volume,count,taker_buy_volume,taker_buy_quote_volume,ignore")?;
    for r in &all_rows {
        writeln!(f, "{},{},{},{},{},{},{},{},{},{},{},{}",
                 r.0, r.1, r.2, r.3, r.4, r.5, r.6, r.7, r.8, r.9, r.10, r.11)?;
    }
    drop(f);

    println!();
    println!("✅ Скачано {} свечей → {}", all_rows.len(), out_file.display());
    let first_dt = Utc.timestamp_millis_opt(all_rows[0].0).unwrap();
    let last_dt = Utc.timestamp_millis_opt(all_rows[all_rows.len()-1].0).unwrap();
    println!("   Период: {} → {}",
             first_dt.format("%Y-%m-%d %H:%M"),
             last_dt.format("%Y-%m-%d %H:%M"));

    let closes: Vec<f64> = all_rows.iter().filter_map(|r| r.4.parse().ok()).collect();
    if !closes.is_empty() {
        let min = closes.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = closes.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let avg = closes.iter().sum::<f64>() / closes.len() as f64;
        println!("   Цена: min={:.2} max={:.2} avg={:.2}", min, max, avg);
    }

    Ok(())
}