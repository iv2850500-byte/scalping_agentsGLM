//! bybit_futures.rs — реализация `ExchangeClient` для Bybit V5 API.
//!
//! Архитектура:
//!   - Трейт ExchangeClient тот же, что и для BinanceFuturesClient
//!   - ResilientClient (Decorator) обертывает нас — retry/fallback бесплатно
//!   - Поётма API: Bybit V5 (https://bybit-exchange.github.io/docs/v5/)
//!
//! ВАЖНЫЕ ОТЛИЧИЯ от Binance:
//!   1. Auth: HMAC на `timestamp + api_key + recv_window + payload` (4 заголовка)
//!   2. GET подпись: payload = query_string (без "?")
//!   3. POST подпись: payload = JSON body
//!   4. category=linear — обязательный параметр для всех V5 endpoints
//!   5. positionIdx=0 — one-way mode (нужно настроить аккаунт ВРУЧНУЮ через API)
//!   6. side: "Buy"/"Sell" (capital B!) vs Binance "BUY"/"SELL"
//!   7. orderType: "Market"/"Limit" (capital) vs Binance "MARKET"/"LIMIT"
//!   8. interval "60" = 1h (числовой, не "1h" как у Binance)
//!   9. WebSocket требует 20с heartbeat ping (Binance сам шлёт)
//!
//! Базовые URLs:
//!   testnet: https://api-testnet.bybit.com
//!   prod:    https://api.bybit.com

use crate::exchange::bybit_sign;
use crate::exchange::traits::{ExchangeClient, ExchangePosition, OrderResult};
use crate::exchange::time_sync::TimeSync;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

pub struct BybitFuturesClient {
    api_key: String,
    api_secret: String,
    client: Client,
    base_url: String,
    time_sync: Arc<TimeSync>,
}

impl BybitFuturesClient {
    pub fn new(
        api_key: &str,
        api_secret: &str,
        base_url: &str,
        time_sync: Arc<TimeSync>,
    ) -> Self {
        Self {
            api_key: api_key.to_string(),
            api_secret: api_secret.to_string(),
            // Bybit REST timeout: 10s как у Binance (Decorator всё равно добавит свой 3с retry)
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .pool_idle_timeout(std::time::Duration::from_secs(20)).tcp_keepalive(std::time::Duration::from_secs(15))/* P19-POOL */.build()
                .unwrap_or_default(),
            base_url: base_url.trim_end_matches('/').to_string(),
            time_sync,
        }
    }

    fn now_ms(&self) -> i64 {
        self.time_sync.now_ms()
    }

    /// Подписанный GET-запрос. Возвращает parsed JSON.
    async fn signed_get(&self, path: &str, query: &str) -> Result<Value> {
        let full_url = format!("{}{}", self.base_url, path);
        let url_with_query = if query.is_empty() {
            full_url.clone()
        } else {
            format!("{}?{}", full_url, query)
        };

        let ts = self.now_ms();
        let sign = bybit_sign::sign_get(&self.api_key, &self.api_secret, ts, query);

        let resp = self
            .client
            .get(&url_with_query)
            .header("X-BAPI-API-KEY", &self.api_key)
            .header("X-BAPI-SIGN", &sign)
            .header("X-BAPI-TIMESTAMP", ts.to_string())
            .header("X-BAPI-RECV-WINDOW", bybit_sign::RECV_WINDOW.to_string())
            .send()
            .await?;

        // v3.6 FIX: text-first body read + HTML/geoblock detection.
        // Parse errors now carry HTTP status + body head (was: bare
        // "error decoding response body" with zero diagnostics).
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| anyhow!("Bybit HTTP {} | body read failed: {}", status, e))?;
        let head: String = body
            .chars()
            .take(160)
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let lc = body.trim_start().to_ascii_lowercase();
        if lc.starts_with("<!doctype html") || lc.starts_with("<html") {
            return Err(anyhow!("Bybit HTTP {} | GEOBLOCK/HTML instead of JSON: {}", status, head));
        }
        let json: Value = serde_json::from_str(&body)
            .map_err(|e| anyhow!("Bybit HTTP {} | body[:160]: {} | serde: {}", status, head, e))?;
        // Bybit V5: retCode=0 success, anything else = error
        let ret_code = json["retCode"].as_i64().unwrap_or(-1);
        if ret_code != 0 {
            return Err(anyhow!(
                "Bybit V5 error: code={} msg={}",
                ret_code,
                json["retMsg"].as_str().unwrap_or("?")
            ));
        }
        Ok(json)
    }

    /// Подписанный POST-запрос с JSON body. Возвращает parsed JSON.
    async fn signed_post(&self, path: &str, body: &Value) -> Result<Value> {
        let full_url = format!("{}{}", self.base_url, path);
        let body_str = serde_json::to_string(body)?;

        let ts = self.now_ms();
        let sign = bybit_sign::sign_post(&self.api_key, &self.api_secret, ts, &body_str);

        let resp = self
            .client
            .post(&full_url)
            .header("X-BAPI-API-KEY", &self.api_key)
            .header("X-BAPI-SIGN", &sign)
            .header("X-BAPI-TIMESTAMP", ts.to_string())
            .header("X-BAPI-RECV-WINDOW", bybit_sign::RECV_WINDOW.to_string())
            .header("Content-Type", "application/json")
            .body(body_str)
            .send()
            .await?;

        // v3.6 FIX: text-first body read + HTML/geoblock detection.
        // Parse errors now carry HTTP status + body head (was: bare
        // "error decoding response body" with zero diagnostics).
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| anyhow!("Bybit HTTP {} | body read failed: {}", status, e))?;
        let head: String = body
            .chars()
            .take(160)
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let lc = body.trim_start().to_ascii_lowercase();
        if lc.starts_with("<!doctype html") || lc.starts_with("<html") {
            return Err(anyhow!("Bybit HTTP {} | GEOBLOCK/HTML instead of JSON: {}", status, head));
        }
        let json: Value = serde_json::from_str(&body)
            .map_err(|e| anyhow!("Bybit HTTP {} | body[:160]: {} | serde: {}", status, head, e))?;
        let ret_code = json["retCode"].as_i64().unwrap_or(-1);
        if ret_code != 0 {
            return Err(anyhow!(
                "Bybit V5 error: code={} msg={}",
                ret_code,
                json["retMsg"].as_str().unwrap_or("?")
            ));
        }
        Ok(json)
    }

    /// Нормализует ответ Bybit OrderResult к нашему типу.
    /// Bybit использует строки для всех numeric полей ("0.001", "1.234").
    fn parse_order_result(json: &Value, default_symbol: &str, default_side: &str) -> OrderResult {
        // В POST /v5/order/create ответ содержит orderId как строку
        let order_id_str = json["orderId"].as_str().unwrap_or("0");
        let order_id: u64 = order_id_str.parse().unwrap_or(0);

        let symbol = json["symbol"].as_str().unwrap_or(default_symbol).to_string();
        // Bybit side: "Buy"/"Sell" → приводим к "BUY"/"SELL" как ожидает движок
        let side_raw = json["side"].as_str().unwrap_or(default_side);
        let side = match side_raw {
            "Buy" | "buy" => "BUY",
            "Sell" | "sell" => "SELL",
            other => other,
        }.to_string();

        // avgPrice может быть в ответе на query (не в POST), но мы попытаемся
        let price: f64 = json["avgPrice"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                json["price"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0)
            });

        // qty — origQty в ответе
        let quantity: f64 = json["qty"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                json["origQty"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0)
            });

        let executed_qty: f64 = json["cumExecQty"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);

        // status: "New", "Filled", "Cancelled", "PartiallyFilled"
        let status = match json["status"].as_str().unwrap_or("New") {
            "New" => "NEW",
            "PartiallyFilled" | "PartiallyFilledCanceled" => "PARTIALLY_FILLED",
            "Filled" => "FILLED",
            "Cancelled" => "CANCELED",
            "Rejected" => "REJECTED",
            other => other,
        }.to_string();

        OrderResult {
            order_id,
            symbol,
            side,
            price,
            quantity,
            executed_qty,
            status,
            commission: 0.0, // Bybit не возвращает комиссию в order create — нужно отдельно через /v5/account/transaction-log
        }
    }
}

#[async_trait]
impl ExchangeClient for BybitFuturesClient {
    async fn sync_time(&self) -> Result<()> {
        // GET /v5/market/time — public, no auth
        let url = format!("{}/v5/market/time", self.base_url);
        let json: Value = self.client.get(&url).send().await?.json().await?;
        let server_ms = json["result"]["timeSecond"]
            .as_f64()
            .map(|v| (v * 1000.0) as i64)
            // fallback: timeNano (nanoseconds)
            .or_else(|| {
                json["result"]["timeNano"]
                    .as_str()
                    .and_then(|s| s.parse::<u128>().ok())
                    .map(|n| (n / 1_000_000) as i64)
            })
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());

        let local_ms = chrono::Utc::now().timestamp_millis();
        let offset = server_ms - local_ms;
        self.time_sync.set_offset(offset);
        tracing::info!("⏰ Bybit clock offset: {} ms", offset);
        Ok(())
    }

    async fn place_market_order(
        &self,
        symbol: &str,
        side: &str,
        quantity: f64,
        client_order_id: Option<String>,
    ) -> Result<OrderResult> {
        // Bybit side: "Buy"/"Sell" (capital B/S) — наш движок передаёт "BUY"/"SELL"
        let side_upper = side.to_uppercase();
        let bybit_side = match side_upper.as_str() {
             "BUY" => "Buy",
            "SELL" => "Sell",
            other => other,
        };

        let mut body = serde_json::json!({
            "category": "linear",
            "symbol": symbol.to_uppercase(),
            "side": bybit_side,
            "orderType": "Market",
            "qty": format!("{}", quantity),
            "timeInForce": "GTC",
            "positionIdx": 0,  // one-way mode
        });
        if let Some(id) = client_order_id {
            // Bybit: max 36 chars, alphanumeric + _-
            let safe_id: String = id.chars().take(36).collect();
            body["orderLinkId"] = serde_json::Value::String(safe_id);
        }

        let json = self.signed_post("/v5/order/create", &body).await?;
        let result = &json["result"];

        Ok(Self::parse_order_result(result, symbol, bybit_side))
    }

    async fn place_limit_order(
        &self,
        symbol: &str,
        side: &str,
        quantity: f64,
        price: f64,
        client_order_id: Option<String>,
    ) -> Result<OrderResult> {
        let side_upper = side.to_uppercase();
        let bybit_side = match side_upper.as_str() {
            "BUY" => "Buy",
            "SELL" => "Sell",
            other => other,
        };

        let mut body = serde_json::json!({
            "category": "linear",
            "symbol": symbol.to_uppercase(),
            "side": bybit_side,
            "orderType": "Limit",
            "qty": format!("{}", quantity),
            "price": format!("{}", price),
            "timeInForce": "GTC",  // Good-Till-Cancelled = maker
            "positionIdx": 0,
        });
        if let Some(id) = client_order_id {
            let safe_id: String = id.chars().take(36).collect();
            body["orderLinkId"] = serde_json::Value::String(safe_id);
        }

        let json = self.signed_post("/v5/order/create", &body).await?;
        let result = &json["result"];

        Ok(Self::parse_order_result(result, symbol, bybit_side))
    }

    async fn place_protective_order(
        &self,
        symbol: &str,
        side: &str,
        stop: bool,
        trigger_price: f64,
        client_order_id: Option<String>,
    ) -> Result<OrderResult> {
        let side_upper = side.to_uppercase();
        let bybit_side = match side_upper.as_str() {
            "BUY" => "Buy",
            "SELL" => "Sell",
            other => other,
        };

        // stop=true → STOP_MARKET (SL), stop=false → TAKE_PROFIT_MARKET (TP)
        let (order_type, tpsl) = if stop {
            ("Stop", "tpslMode: Full")
        } else {
            ("TakeProfit", "tpslMode: Full")
        };

        let _ = tpsl; // подавить warning

        let mut body = serde_json::json!({
            "category": "linear",
            "symbol": symbol.to_uppercase(),
            "side": bybit_side,
            "orderType": order_type,
            "triggerPrice": format!("{}", trigger_price),
            "triggerBy": "LastPrice",
            "closeOnTrigger": true,  // закрывает позицию, не открывает новую
            "positionIdx": 0,
            "timeInForce": "GTC",
        });
        if let Some(id) = client_order_id {
            let safe_id: String = id.chars().take(36).collect();
            body["orderLinkId"] = serde_json::Value::String(safe_id);
        }

        let json = self.signed_post("/v5/order/create", &body).await?;
        let result = &json["result"];

        Ok(Self::parse_order_result(result, symbol, bybit_side))
    }

    async fn cancel_order(&self, symbol: &str, order_id: u64) -> Result<bool> {
        let body = serde_json::json!({
            "category": "linear",
            "symbol": symbol.to_uppercase(),
            "orderId": order_id.to_string(),
        });
        match self.signed_post("/v5/order/cancel", &body).await {
            Ok(_) => Ok(true),
            Err(e) => {
                // Bybit -30001 = order not exist — НЕ ошибка для нас (idempotent)
                let err_str = e.to_string();
                if err_str.contains("-30001") || err_str.contains("not exist") {
                    return Ok(false);
                }
                Err(e)
            }
        }
    }

    async fn get_order_status(&self, symbol: &str, order_id: u64) -> Result<OrderResult> {
        let query = bybit_sign::build_query(&[
            ("category", "linear"),
            ("symbol", &symbol.to_uppercase()),
            ("orderId", &order_id.to_string()),
        ]);
        let json = self.signed_get("/v5/order/realtime", &query).await?;
        let list = json["result"]["list"]
            .as_array()
            .and_then(|a| a.first())
            .ok_or_else(|| anyhow!("order {} not found", order_id))?;

        Ok(Self::parse_order_result(list, symbol, ""))
    }

    async fn get_order_by_client_id(
        &self,
        symbol: &str,
        orig_client_order_id: &str,
    ) -> Result<OrderResult> {
        let query = bybit_sign::build_query(&[
            ("category", "linear"),
            ("symbol", &symbol.to_uppercase()),
            ("orderLinkId", orig_client_order_id),
        ]);
        let json = self.signed_get("/v5/order/realtime", &query).await?;
        let list = json["result"]["list"]
            .as_array()
            .and_then(|a| a.first())
            .ok_or_else(|| anyhow!("orderLinkId {} not found", orig_client_order_id))?;

        Ok(Self::parse_order_result(list, symbol, ""))
    }

    async fn get_open_orders(&self, symbol: &str) -> Result<Vec<OrderResult>> {
        let query = bybit_sign::build_query(&[
            ("category", "linear"),
            ("symbol", &symbol.to_uppercase()),
        ]);
        let json = self.signed_get("/v5/order/realtime", &query).await?;
        let mut out = Vec::new();
        if let Some(arr) = json["result"]["list"].as_array() {
            for item in arr {
                out.push(Self::parse_order_result(item, symbol, ""));
            }
        }
        Ok(out)
    }

    async fn get_balance(&self, asset: &str) -> Result<f64> {
        let query = bybit_sign::build_query(&[
            ("accountType", "UNIFIED"),
        ]);
        let json = self.signed_get("/v5/account/wallet-balance", &query).await?;
        // Структура: result.list[0].coin[].availableToWithdraw
        if let Some(coins) = json["result"]["list"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|acc| acc["coin"].as_array())
        {
            for coin in coins {
                if coin["coin"].as_str() == Some(asset) {
                    return Ok(coin["availableToWithdraw"]
                        .as_str()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0.0));
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
        // Public endpoint, no auth
        let url = format!(
            "{}/v5/market/orderbook?category=linear&symbol={}&limit={}",
            self.base_url,
            symbol.to_uppercase(),
            limit
        );
        let json: Value = self.client.get(&url).send().await?.json().await?;

        let parse_levels = |key: &str| -> Vec<(f64, f64)> {
            json["result"][key]
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

        Ok((parse_levels("b"), parse_levels("a")))
    }

    async fn get_positions(&self) -> Result<Vec<ExchangePosition>> {
        let query = bybit_sign::build_query(&[
            ("category", "linear"),
            ("settleCoin", "USDT"),
        ]);
        let json = self.signed_get("/v5/position/list", &query).await?;

        let mut out = Vec::new();
        if let Some(arr) = json["result"]["list"].as_array() {
            for pos in arr {
                let symbol = pos["symbol"].as_str().unwrap_or("").to_string();
                let size: f64 = pos["size"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                // Bybit: positive size = long, negative = short (если one-way mode)
                let qty = match pos["side"].as_str() {
                    Some("Sell") => -size.abs(),
                    _ => size.abs(),
                };
                let entry: f64 = pos["avgPrice"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);

                if qty.abs() > 0.0 {
                    out.push(ExchangePosition {
                        symbol,
                        qty,
                        entry_price: entry,
                    });
                }
            }
        }
        Ok(out)
    }

    async fn get_order_commission(&self, _symbol: &str, _order_id: u64) -> Result<f64> {
        // Bybit V5: /v5/account/transaction-log с фильтром по orderId
        // Реализация сложная — fallback на 0.0 (как у binance_rest на testnet)
        // Decorator + state_manager компенсирует через fact-commission из user-stream
        Ok(0.0)
    }
}
