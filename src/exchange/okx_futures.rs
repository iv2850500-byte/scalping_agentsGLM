//! okx_futures.rs - ExchangeClient for OKX V5 (USDT-margined linear SWAP).
//! Differences vs Bybit/Binance:
//!   1. OK-ACCESS-* headers; sign = base64(HMAC-SHA256(ts_iso + METHOD + path(+query) + body))
//!   2. ts = ISO8601 UTC string (not ms)
//!   3. passphrase required (3rd credential)
//!   4. sz in LOTS: lots = qty_base / ctVal (fetched from /public/instruments, cached)
//!   5. "XAUUSDT" -> instId "XAU-USDT-SWAP"; emitted symbols converted back
//!   6. envelope {"code":"0","data":[...]}, code is STRING
//!   7. demo mode: header x-simulated-trading: 1 on PRIVATE endpoints (same hosts)

use crate::exchange::okx_sign;
use crate::exchange::time_sync::TimeSync;
use crate::exchange::traits::{ExchangeClient, ExchangePosition, OrderResult};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// "XAUUSDT" -> "XAU-USDT-SWAP"; already-hyphened input passes through.
fn to_inst_id(symbol: &str) -> String {
    let s = symbol.trim().to_uppercase();
    if s.contains('-') {
        if s.ends_with("-SWAP") { s } else { format!("{}-SWAP", s) }
    } else if let Some(base) = s.strip_suffix("USDT") {
        format!("{}-USDT-SWAP", base)
    } else {
        s
    }
}

/// "XAU-USDT-SWAP" -> "XAUUSDT" (engine-side canonical symbol).
fn from_inst_id(inst_id: &str) -> String {
    let s = inst_id.trim().to_uppercase();
    if let Some(base) = s.strip_suffix("-USDT-SWAP") {
        format!("{}USDT", base)
    } else if let Some(base) = s.strip_suffix("-SWAP") {
        base.to_string()
    } else {
        s
    }
}

pub struct OkxFuturesClient {
    api_key: String,
    api_secret: String,
    passphrase: String,
    demo: bool,
    client: Client,
    base_url: String,
    time_sync: Arc<TimeSync>,
    /// instId -> (ctVal, lotSz, minSz, tickSz), lazily fetched
    contracts: Mutex<HashMap<String, (f64, f64, f64, f64)>>,
}

impl OkxFuturesClient {
    pub fn new(
        api_key: &str,
        api_secret: &str,
        passphrase: &str,
        base_url: &str,
        demo: bool,
        time_sync: Arc<TimeSync>,
    ) -> Self {
        Self {
            api_key: api_key.to_string(),
            api_secret: api_secret.to_string(),
            passphrase: passphrase.to_string(),
            demo,
            client: Client::builder().timeout(Duration::from_secs(10)).pool_idle_timeout(std::time::Duration::from_secs(20)).tcp_keepalive(std::time::Duration::from_secs(15))/* P19-POOL */.build().unwrap_or_default(),
            base_url: base_url.trim_end_matches('/').to_string(),
            time_sync,
            contracts: Mutex::new(HashMap::new()),
        }
    }

    async fn parse_envelope(resp: reqwest::Response) -> Result<Value> {
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| anyhow!("OKX HTTP {} | body read failed: {}", status, e))?;
        let head: String = body.chars().take(160).map(|c| if c.is_control() { ' ' } else { c }).collect();
        let lc = body.trim_start().to_ascii_lowercase();
        if lc.starts_with("<!doctype html") || lc.starts_with("<html") {
            return Err(anyhow!("OKX HTTP {} | GEOBLOCK/HTML instead of JSON: {}", status, head));
        }
        let json: Value = serde_json::from_str(&body)
            .map_err(|e| anyhow!("OKX HTTP {} | body[:160]: {} | serde: {}", status, head, e))?;
        let code = json["code"].as_str().unwrap_or("-1").to_string();
        if code != "0" {
            return Err(anyhow!(
                "OKX V5 error: code={} msg={}",
                code,
                json["msg"].as_str().unwrap_or("?")
            ));
        }
        Ok(json)
    }

    async fn public_get(&self, path: &str, query: &str) -> Result<Value> {
        let url = if query.is_empty() {
            format!("{}{}", self.base_url, path)
        } else {
            format!("{}{}?{}", self.base_url, path, query)
        };
        let resp = self.client.get(&url).send().await?;
        Self::parse_envelope(resp).await
    }

    async fn signed_get(&self, path: &str, query: &str) -> Result<Value> {
        let request_path = if query.is_empty() {
            path.to_string()
        } else {
            format!("{}?{}", path, query)
        };
        let url = format!("{}{}", self.base_url, request_path);
        let ts = okx_sign::iso_timestamp();
        let sign = okx_sign::sign(&self.api_secret, &ts, "GET", &request_path, "");
        let mut req = self
            .client
            .get(&url)
            .header("OK-ACCESS-KEY", &self.api_key)
            .header("OK-ACCESS-SIGN", sign)
            .header("OK-ACCESS-TIMESTAMP", ts)
            .header("OK-ACCESS-PASSPHRASE", &self.passphrase);
        if self.demo {
            req = req.header(okx_sign::DEMO_HEADER, "1");
        }
        let resp = req.send().await?;
        Self::parse_envelope(resp).await
    }

    async fn signed_post(&self, path: &str, body: &Value) -> Result<Value> {
        let body_str = serde_json::to_string(body)?;
        let ts = okx_sign::iso_timestamp();
        let sign = okx_sign::sign(&self.api_secret, &ts, "POST", path, &body_str);
        let mut req = self
            .client
            .post(&format!("{}{}", self.base_url, path))
            .header("OK-ACCESS-KEY", &self.api_key)
            .header("OK-ACCESS-SIGN", sign)
            .header("OK-ACCESS-TIMESTAMP", ts)
            .header("OK-ACCESS-PASSPHRASE", &self.passphrase)
            .header("Content-Type", "application/json")
            .body(body_str);
        if self.demo {
            req = req.header(okx_sign::DEMO_HEADER, "1");
        }
        let resp = req.send().await?;
        Self::parse_envelope(resp).await
    }

    async fn ensure_contract(&self, inst_id: &str) -> Result<(f64, f64, f64, f64)> {
        if let Some(c) = self.contracts.lock().unwrap().get(inst_id) {
            return Ok(*c);
        }
        let json = self
            .public_get("/api/v5/public/instruments", &format!("instType=SWAP&instId={}", inst_id))
            .await?;
        let d = json["data"]
            .as_array()
            .and_then(|a| a.first())
            .ok_or_else(|| anyhow!("OKX: instrument {} not found", inst_id))?;
        let ctval: f64 = d["ctVal"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let lotsz: f64 = d["lotSz"].as_str().and_then(|s| s.parse().ok()).unwrap_or(1.0);
        let minsz: f64 = d["minSz"].as_str().and_then(|s| s.parse().ok()).unwrap_or(1.0);
        let ticksz: f64 = d["tickSz"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.1);
        if ctval <= 0.0 {
            return Err(anyhow!("OKX: bad ctVal for {}", inst_id));
        }
        let c = (ctval, lotsz, minsz, ticksz);
        self.contracts.lock().unwrap().insert(inst_id.to_string(), c);
        tracing::info!("[okx] контракт {}: ctVal={} lotSz={} minSz={} tickSz={}", inst_id, ctval, lotsz, minsz, ticksz);
        Ok(c)
    }

fn qty_to_lots(qty: f64, c: (f64, f64, f64, f64)) -> f64 {
    let (ctval, lotsz, minsz, _) = c;
    let lotsz = if lotsz > 0.0 { lotsz } else { 1.0 };
    let raw = qty / ctval;
    let mut lots = (raw / lotsz).round() * lotsz;
    if lots < minsz { lots = minsz; }
    lots
}

fn lots_str(lots: f64) -> String {
    let s = format!("{:.10}", lots);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() { "0".to_string() } else { s.to_string() }
}

    fn px_str(p: f64, tick: f64) -> String {
        let t = if tick > 0.0 { tick } else { 0.1 };
        let v = (p / t).round() * t;
        let s = format!("{:.8}", v);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }

    fn check_scode(result: &Value) -> Result<()> {
        let scode = result[0]["sCode"].as_str().unwrap_or("0");
        if scode != "0" {
            return Err(anyhow!(
                "OKX order rejected: sCode={} sMsg={}",
                scode,
                result[0]["sMsg"].as_str().unwrap_or("?")
            ));
        }
        Ok(())
    }

    fn parse_order(o: &Value, ctval: f64, default_symbol: &str) -> OrderResult {
        let order_id = o["ordId"]
            .as_str()
            .or_else(|| o["algoId"].as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let symbol = from_inst_id(o["instId"].as_str().unwrap_or(default_symbol));
        let side = match o["side"].as_str().unwrap_or("") {
            "buy" => "BUY".to_string(),
            "sell" => "SELL".to_string(),
            other => other.to_string(),
        };
        let price: f64 = ["avgPx", "fillPx", "px", "slTriggerPx", "tpTriggerPx"]
            .iter()
            .find_map(|k| o[*k].as_str().and_then(|s| s.parse::<f64>().ok()))
            .unwrap_or(0.0);
        let sz: f64 = o["sz"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let acc: f64 = o["accFillSz"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let status = match o["state"].as_str().unwrap_or("live") {
            "live" => "NEW",
            "partially_filled" => "PARTIALLY_FILLED",
            "filled" => "FILLED",
            "canceled" | "cancelled" => "CANCELED",
            other => other,
        }
        .to_string();
        let commission = o["fee"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .map(|f| f.abs())
            .unwrap_or(0.0);
        OrderResult {
            order_id,
            symbol,
            side,
            price,
            quantity: sz * ctval,
            executed_qty: acc * ctval,
            status,
            commission,
        }
    }

    fn client_id_to_clord(id: &Option<String>) -> Option<Value> {
        id.as_ref().map(|s| {
            let safe: String = s.chars().filter(|ch| ch.is_ascii_alphanumeric()).take(32).collect();
            Value::String(safe)
        })
    }
}

#[async_trait]
impl ExchangeClient for OkxFuturesClient {
    async fn sync_time(&self) -> Result<()> {
        let json = self.public_get("/api/v5/public/time", "").await?;
        let server_ms = json["data"][0]["ts"]
            .as_str()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let local_ms = chrono::Utc::now().timestamp_millis();
        let offset = server_ms - local_ms;
        self.time_sync.set_offset(offset);
        tracing::info!("OKX clock offset: {} ms", offset);
        Ok(())
    }

    async fn place_market_order(
        &self,
        symbol: &str,
        side: &str,
        quantity: f64,
        client_order_id: Option<String>,
    ) -> Result<OrderResult> {
        let inst_id = to_inst_id(symbol);
        let c = self.ensure_contract(&inst_id).await?;
        let lots = Self::qty_to_lots(quantity, c);
        tracing::info!("[okx] market {}: {} {} base -> {} lots (ctVal={})", inst_id, side, quantity, lots, c.0);
        let mut body = serde_json::json!({
            "instId": inst_id,
            "tdMode": "cross",
            "side": side.to_lowercase(),
            "ordType": "market",
            "sz": Self::lots_str(lots),
        });
        if let Some(v) = Self::client_id_to_clord(&client_order_id) {
            body["clOrdId"] = v;
        }
        let json = self.signed_post("/api/v5/trade/order", &body).await?;
        Self::check_scode(&json["data"])?;
        Ok(OrderResult {
            order_id: json["data"][0]["ordId"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0),
            symbol: from_inst_id(&inst_id),
            side: side.to_uppercase(),
            price: 0.0,
            quantity: lots * c.0,
            executed_qty: 0.0,
            status: "NEW".to_string(),
            commission: 0.0,
        })
    }

    async fn place_limit_order(
        &self,
        symbol: &str,
        side: &str,
        quantity: f64,
        price: f64,
        client_order_id: Option<String>,
    ) -> Result<OrderResult> {
        let inst_id = to_inst_id(symbol);
        let c = self.ensure_contract(&inst_id).await?;
        let lots = Self::qty_to_lots(quantity, c);
        let mut body = serde_json::json!({
            "instId": inst_id,
            "tdMode": "cross",
            "side": side.to_lowercase(),
            "ordType": "limit",
            "sz": Self::lots_str(lots),
            "px": Self::px_str(price, c.3),
            "tif": "GTC",
        });
        if let Some(v) = Self::client_id_to_clord(&client_order_id) {
            body["clOrdId"] = v;
        }
        let json = self.signed_post("/api/v5/trade/order", &body).await?;
        Self::check_scode(&json["data"])?;
        Ok(OrderResult {
            order_id: json["data"][0]["ordId"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0),
            symbol: from_inst_id(&inst_id),
            side: side.to_uppercase(),
            price,
            quantity: lots * c.0,
            executed_qty: 0.0,
            status: "NEW".to_string(),
            commission: 0.0,
        })
    }

    async fn place_protective_order(
        &self,
        symbol: &str,
        side: &str,
        stop: bool,
        trigger_price: f64,
        client_order_id: Option<String>,
    ) -> Result<OrderResult> {
        // Conditional algo order, closes 100% of position (OKX analog of closeOnTrigger).
        let inst_id = to_inst_id(symbol);
        let c = self.ensure_contract(&inst_id).await?;
        let trig = Self::px_str(trigger_price, c.3);
        let mut body = if stop {
            serde_json::json!({
                "instId": inst_id, "tdMode": "cross",
                "side": side.to_lowercase(), "ordType": "conditional",
                "closeFraction": "1", "reduceOnly": true,
                "slTriggerPx": trig, "slOrdPx": "-1",
            })
        } else {
            serde_json::json!({
                "instId": inst_id, "tdMode": "cross",
                "side": side.to_lowercase(), "ordType": "conditional",
                "closeFraction": "1", "reduceOnly": true,
                "tpTriggerPx": trig, "tpOrdPx": "-1",
            })
        };
        if let Some(v) = Self::client_id_to_clord(&client_order_id) {
            body["clOrdId"] = v;
        }
        let json = self.signed_post("/api/v5/trade/order-algo", &body).await?;
        Self::check_scode(&json["data"])?;
        Ok(OrderResult {
            order_id: json["data"][0]["algoId"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0),
            symbol: from_inst_id(&inst_id),
            side: side.to_uppercase(),
            price: trigger_price,
            quantity: 0.0,
            executed_qty: 0.0,
            status: "NEW".to_string(),
            commission: 0.0,
        })
    }

    async fn cancel_order(&self, symbol: &str, order_id: u64) -> Result<bool> {
        let inst_id = to_inst_id(symbol);
        let not_exist = |e: &anyhow::Error| {
            let s = e.to_string();
            s.contains("51400") || s.contains("51503") || s.contains("51603")
                || s.contains("not exist") || s.contains("does not exist") || s.contains("doesn't exist")
        };
        let body = serde_json::json!({ "instId": inst_id, "ordId": order_id.to_string() });
        match self.signed_post("/api/v5/trade/cancel-order", &body).await {
            Ok(_) => Ok(true),
            Err(e) if not_exist(&e) => {
                // maybe it is an algo (SL/TP) order -> try cancel-algo
                let body2 = serde_json::json!({ "instId": inst_id, "algoId": order_id.to_string() });
                match self.signed_post("/api/v5/trade/cancel-algo", &body2).await {
                    Ok(_) => Ok(true),
                    Err(e2) if not_exist(&e2) => Ok(false),
                    Err(e2) => Err(e2),
                }
            }
            Err(e) => Err(e),
        }
    }

    async fn get_order_status(&self, symbol: &str, order_id: u64) -> Result<OrderResult> {
        let inst_id = to_inst_id(symbol);
        let c = self.ensure_contract(&inst_id).await?;
        let json = self
            .signed_get("/api/v5/trade/order", &format!("instId={}&ordId={}", inst_id, order_id))
            .await?;
        let o = json["data"]
            .as_array()
            .and_then(|a| a.first())
            .ok_or_else(|| anyhow!("order {} not found", order_id))?;
        Ok(Self::parse_order(o, c.0, symbol))
    }

    async fn get_order_by_client_id(
        &self,
        symbol: &str,
        orig_client_order_id: &str,
    ) -> Result<OrderResult> {
        let inst_id = to_inst_id(symbol);
        let c = self.ensure_contract(&inst_id).await?;
        let json = self
            .signed_get("/api/v5/trade/order", &format!("instId={}&clOrdId={}", inst_id, orig_client_order_id))
            .await?;
        let o = json["data"]
            .as_array()
            .and_then(|a| a.first())
            .ok_or_else(|| anyhow!("clOrdId {} not found", orig_client_order_id))?;
        Ok(Self::parse_order(o, c.0, symbol))
    }

    async fn get_open_orders(&self, symbol: &str) -> Result<Vec<OrderResult>> {
        let inst_id = to_inst_id(symbol);
        let c = self.ensure_contract(&inst_id).await?;
        let json = self
            .signed_get("/api/v5/trade/orders-pending", &format!("instId={}", inst_id))
            .await?;
        let mut out = Vec::new();
        if let Some(arr) = json["data"].as_array() {
            for o in arr {
                out.push(Self::parse_order(o, c.0, symbol));
            }
        }
        Ok(out)
    }

    async fn get_balance(&self, asset: &str) -> Result<f64> {
        let json = self
            .signed_get("/api/v5/account/balance", &format!("ccy={}", asset.to_uppercase()))
            .await?;
        if let Some(details) = json["data"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|d| d["details"].as_array())
        {
            for d in details {
                if d["ccy"].as_str() == Some(asset.to_uppercase().as_str()) {
                    let v = d["availBal"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .or_else(|| d["availEq"].as_str().filter(|s| !s.is_empty()))
                        .or_else(|| d["eq"].as_str().filter(|s| !s.is_empty()));
                    if let Some(v) = v {
                        if let Ok(f) = v.parse::<f64>() {
                            return Ok(f);
                        }
                    }
                }
            }
        }
        Ok(0.0)
    }

    async fn get_order_book(
        &self,
        symbol: &str,
        limit: usize,
    ) -> Result<(Vec<(f64, f64)>, Vec<(f64, f64)>)> {
        let inst_id = to_inst_id(symbol);
        let sz = limit.clamp(1, 400);
        let json = self
            .public_get("/api/v5/market/books", &format!("instId={}&sz={}", inst_id, sz))
            .await?;
        let d = json["data"][0].clone();
        let parse_levels = |key: &str| -> Vec<(f64, f64)> {
            d[key]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| {
                            let p = v[0].as_str()?.parse::<f64>().ok()?;
                            let q = v[1].as_str()?.parse::<f64>().ok()?;
                            Some((p, q))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        Ok((parse_levels("bids"), parse_levels("asks")))
    }

    async fn get_positions(&self) -> Result<Vec<ExchangePosition>> {
        let json = self.signed_get("/api/v5/account/positions", "instType=SWAP").await?;
        let mut out = Vec::new();
        if let Some(arr) = json["data"].as_array() {
            for p in arr {
                let pos: f64 = p["pos"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                if pos.abs() <= 0.0 {
                    continue;
                }
                let inst_id = p["instId"].as_str().unwrap_or("").to_string();
                let ctval = match self.ensure_contract(&inst_id).await {
                    Ok(c) => c.0,
                    Err(_) => {
                        tracing::warn!("[okx] нет ctVal для {} — позиция пропущена", inst_id);
                        continue;
                    }
                };
                out.push(ExchangePosition {
                    symbol: from_inst_id(&inst_id),
                    qty: pos * ctval,
                    entry_price: p["avgPx"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
                });
            }
        }
        Ok(out)
    }

    async fn get_order_commission(&self, symbol: &str, order_id: u64) -> Result<f64> {
        let inst_id = to_inst_id(symbol);
        let json = self
            .signed_get("/api/v5/trade/order", &format!("instId={}&ordId={}", inst_id, order_id))
            .await?;
        if let Some(o) = json["data"].as_array().and_then(|a| a.first()) {
            if let Some(fee) = o["fee"].as_str().and_then(|s| s.parse::<f64>().ok()) {
                return Ok(fee.abs());
            }
        }
        Ok(0.0)
    }
}