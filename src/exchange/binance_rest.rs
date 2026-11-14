use super::time_sync::TimeSync;
use super::traits::{ExchangeClient, OrderResult};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde_json::Value;
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;

type HmacSha256 = Hmac<Sha256>;

pub struct BinanceRestClient {
    api_key: String,
    api_secret: String,
    client: Client,
    base_url: String,
    time_sync: Arc<TimeSync>,
}

impl BinanceRestClient {
    pub fn new(
        api_key: &str,
        api_secret: &str,
        base_url: &str,
        time_sync: Arc<TimeSync>,
    ) -> Self {
        Self {
            api_key: api_key.to_string(),
            api_secret: api_secret.to_string(),
            // ✅ P41/D14: fail-loud вместо unwrap_or_default() (дефолтный
            // клиент — без таймаута).
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .pool_idle_timeout(std::time::Duration::from_secs(20)).tcp_keepalive(std::time::Duration::from_secs(15))/* P19-POOL */.build()
                .expect("reqwest client with 10s timeout"),
            base_url: base_url.trim_end_matches('/').to_string(),
            time_sync,
        }
    }

    fn now_ms(&self) -> i64 {
        self.time_sync.now_ms()
    }

    fn sign(&self, query: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(self.api_secret.as_bytes()).unwrap();
        mac.update(query.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    // ✅ P41/D14: единая точка подписанного URL с явным recvWindow (5000 мс
    // = дефолт биржи, поведение 1:1). Подпись после добавления recvWindow.
    fn signed_url(&self, path: &str, query: &str) -> String {
        let q = format!("{}&recvWindow=5000", query);
        let sig = self.sign(&q);
        format!("{}{}?{}&signature={}", self.base_url, path, q, sig)
    }

    // ✅ P41/D13: не отдаём подписанные URL в логи (reqwest кладёт URL в Display).
    fn no_url_err(e: reqwest::Error) -> anyhow::Error {
        anyhow::Error::msg(e.without_url())
    }
}

#[async_trait]
impl ExchangeClient for BinanceRestClient {
    async fn sync_time(&self) -> Result<()> {
        self.time_sync.sync(&self.base_url).await
    }

    async fn place_market_order(
        &self,
        symbol: &str,
        side: &str,
        quantity: f64,
        client_order_id: Option<String>,
    ) -> Result<OrderResult> {
        let ts = self.now_ms();
        let qty_str = format!("{}", quantity);
        let mut query = format!(
            "symbol={}&side={}&type=MARKET&quantity={}&timestamp={}",
            symbol, side, qty_str, ts
        );
        if let Some(id) = client_order_id {
            query.push_str(&format!("&newClientOrderId={}", id));
        }
        let url = self.signed_url("/api/v3/order", &query); // ✅ P41/D13+D14

        let resp = self.client.post(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await.map_err(Self::no_url_err)?;

        let json: Value = resp.json().await.map_err(Self::no_url_err)?;
        if let Some(err) = json.get("msg") {
            return Err(anyhow!("Binance API error: {}", err));
        }

        Ok(OrderResult {
            order_id: json["orderId"].as_u64().unwrap_or(0),
            symbol: json["symbol"].as_str().unwrap_or(symbol).to_string(),
            side: json["side"].as_str().unwrap_or(side).to_string(),
            price: json["fills"].as_array().and_then(|a| a.first())
                .and_then(|f| f["price"].as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0),
            quantity: json["origQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(quantity),
            executed_qty: json["executedQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
            status: json["status"].as_str().unwrap_or("UNKNOWN").to_string(),
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
        let ts = self.now_ms();
        let qty_str = format!("{}", quantity);
        let price_str = format!("{}", price);
        let mut query = format!(
            "symbol={}&side={}&type=LIMIT&timeInForce=GTC&quantity={}&price={}&timestamp={}",
            symbol, side, qty_str, price_str, ts
        );
        if let Some(id) = client_order_id {
            query.push_str(&format!("&newClientOrderId={}", id));
        }
        let url = self.signed_url("/api/v3/order", &query); // ✅ P41/D13+D14

        let resp = self.client.post(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await.map_err(Self::no_url_err)?;

        let json: Value = resp.json().await.map_err(Self::no_url_err)?;
        if let Some(err) = json.get("msg") {
            return Err(anyhow!("Binance API error: {}", err));
        }

        Ok(OrderResult {
            order_id: json["orderId"].as_u64().unwrap_or(0),
            symbol: symbol.to_string(),
            side: side.to_string(),
            price,
            quantity,
            executed_qty: 0.0,
            status: json["status"].as_str().unwrap_or("NEW").to_string(),
            commission: 0.0,
        })
    }

    async fn cancel_order(&self, symbol: &str, order_id: u64) -> Result<bool> {
        let ts = self.now_ms();
        let query = format!("symbol={}&orderId={}&timestamp={}", symbol, order_id, ts);
        let url = self.signed_url("/api/v3/order", &query); // ✅ P41/D13+D14

        let resp = self.client.delete(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await.map_err(Self::no_url_err)?;
        let status = resp.status();
        let json: Value = resp.json().await.unwrap_or(Value::Null);

        if status.is_success() {
            return Ok(true);
        }
        if json["code"].as_i64() == Some(-2011) {
            return Ok(false);
        }
        Err(anyhow!("cancel {} #{}: {}", symbol, order_id, json["msg"].as_str().unwrap_or("?")))
    }

    async fn get_order_status(&self, symbol: &str, order_id: u64) -> Result<OrderResult> {
        let ts = self.now_ms();
        let query = format!("symbol={}&orderId={}&timestamp={}", symbol, order_id, ts);
        let url = self.signed_url("/api/v3/order", &query); // ✅ P41/D13+D14

        let resp = self.client.get(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await.map_err(Self::no_url_err)?;

        let json: Value = resp.json().await.map_err(Self::no_url_err)?;
        Ok(OrderResult {
            order_id,
            symbol: symbol.to_string(),
            side: json["side"].as_str().unwrap_or("").to_string(),
            price: {
                let exec: f64 = json["executedQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let quote: f64 = json["cummulativeQuoteQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let lim: f64 = json["price"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                if exec > 0.0 && quote > 0.0 { quote / exec } else { lim }
            },
            quantity: json["origQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
            executed_qty: json["executedQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
            status: json["status"].as_str().unwrap_or("UNKNOWN").to_string(),
            commission: 0.0,
        })
    }

    // 🔧 ПАТЧ-СИРОТЫ: см. комментарий в binance_futures.rs
    async fn get_order_by_client_id(
        &self,
        symbol: &str,
        orig_client_order_id: &str,
    ) -> Result<OrderResult> {
        let ts = self.now_ms();
        let query = format!(
            "symbol={}&origClientOrderId={}&timestamp={}",
            symbol, orig_client_order_id, ts
        );
        let url = self.signed_url("/api/v3/order", &query); // ✅ P41/D13+D14

        let resp = self.client.get(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await.map_err(Self::no_url_err)?;
        let json: Value = resp.json().await.map_err(Self::no_url_err)?;

        if json["code"].as_i64().is_some() && json["status"].is_null() {
            return Err(anyhow!(
                "-{} query by client {}: {}",
                json["code"].as_i64().unwrap_or(0),
                orig_client_order_id,
                json["msg"].as_str().unwrap_or("?")
            ));
        }

        Ok(OrderResult {
            order_id: json["orderId"].as_u64().unwrap_or(0),
            symbol: json["symbol"].as_str().unwrap_or(symbol).to_string(),
            side: json["side"].as_str().unwrap_or("").to_string(),
            price: {
                let exec: f64 = json["executedQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let quote: f64 = json["cummulativeQuoteQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let lim: f64 = json["price"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                if exec > 0.0 && quote > 0.0 { quote / exec } else { lim }
            },
            quantity: json["origQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
            executed_qty: json["executedQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
            status: json["status"].as_str().unwrap_or("UNKNOWN").to_string(),
            commission: 0.0,
        })
    }

    // 🔧 ПАТЧ-СИРОТЫ: живые ордера символа (для sweep при shutdown)
    async fn get_open_orders(&self, symbol: &str) -> Result<Vec<OrderResult>> {
        let ts = self.now_ms();
        let query = format!("symbol={}&timestamp={}", symbol, ts);
        let url = self.signed_url("/api/v3/openOrders", &query); // ✅ P41/D13+D14

        let resp = self.client.get(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await.map_err(Self::no_url_err)?;
        let json: Value = resp.json().await.map_err(Self::no_url_err)?;

        if json["code"].as_i64().is_some() && json.as_array().is_none() {
            return Err(anyhow!(
                "openOrders {}: {}",
                symbol,
                json["msg"].as_str().unwrap_or("?")
            ));
        }

        let mut out = Vec::new();
        if let Some(arr) = json.as_array() {
            for o in arr {
                let exec: f64 = o["executedQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let quote: f64 = o["cummulativeQuoteQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let lim: f64 = o["price"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                out.push(OrderResult {
                    order_id: o["orderId"].as_u64().unwrap_or(0),
                    symbol: o["symbol"].as_str().unwrap_or(symbol).to_string(),
                    side: o["side"].as_str().unwrap_or("").to_string(),
                    price: if exec > 0.0 && quote > 0.0 { quote / exec } else { lim },
                    quantity: o["origQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
                    executed_qty: exec,
                    status: o["status"].as_str().unwrap_or("UNKNOWN").to_string(),
                    commission: 0.0,
                });
            }
        }
        Ok(out)
    }

    async fn get_balance(&self, asset: &str) -> Result<f64> {
        let ts = self.now_ms();
        let query = format!("timestamp={}", ts);
        let url = self.signed_url("/api/v3/account", &query); // ✅ P41/D13+D14

        let resp = self.client.get(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await.map_err(Self::no_url_err)?;

        let json: Value = resp.json().await.map_err(Self::no_url_err)?;
        if let Some(balances) = json["balances"].as_array() {
            for b in balances {
                if b["asset"].as_str() == Some(asset) {
                    return Ok(b["free"]
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
        let url = format!(
            "{}/api/v3/depth?symbol={}&limit={}",
            self.base_url, symbol, limit
        );
        let json: Value = self.client.get(&url).send().await.map_err(Self::no_url_err)?.json().await.map_err(Self::no_url_err)?;

        let bids = json["bids"]
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
            .unwrap_or_default();

        let asks = json["asks"]
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
            .unwrap_or_default();

        Ok((bids, asks))
    }
    // ============================================================
    // 🔧 ЭТАП B: защитные ордера (STOP_MARKET closePosition) —
    // это фьючерсная механика. Спот-клиент их не поддерживает:
    // осознанный fail-safe Err (спот-боты у нас не используются,
    // а молчаливое отсутствие защиты хуже явной ошибки).
    // ============================================================
    async fn place_protective_order(
        &self,
        symbol: &str,
        _side: &str,
        stop: bool,
        _trigger_price: f64,
        _client_order_id: Option<String>,
    ) -> Result<OrderResult> {
        Err(anyhow!(
            "защитные ордера (STOP_MARKET/TP_MARKET) не поддержаны для спота ({symbol}, stop={stop}) — \
             используйте futures-клиент"
        ))
    }

}