use super::time_sync::TimeSync;
use super::traits::{AlgoOrderInfo, ExchangeClient, ExchangePosition, OrderResult};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde_json::Value;
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;

type HmacSha256 = Hmac<Sha256>;

pub struct BinanceFuturesClient {
    api_key: String,
    api_secret: String,
    client: Client,
    base_url: String,
    time_sync: Arc<TimeSync>,
}

impl BinanceFuturesClient {
    pub fn new(
        api_key: &str,
        api_secret: &str,
        base_url: &str,
        time_sync: Arc<TimeSync>,
    ) -> Self {
        Self {
            api_key: api_key.to_string(),
            api_secret: api_secret.to_string(),
            // ✅ P41/D14: fail-loud вместо unwrap_or_default() — дефолтный
            // reqwest-клиент строится БЕЗ таймаута, зависший запрос вешал
            // торговый путь навсегда.
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
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

    // ✅ P41/D14: единая точка построения подписанного URL. recvWindow задаётся
    // явно (5000 мс = дефолт биржи — поведение 1:1, но окно теперь задокументировано
    // и легко сужается). Подпись считается ПОСЛЕ добавления recvWindow.
    fn signed_url(&self, path: &str, query: &str) -> String {
        let q = format!("{}&recvWindow=5000", query);
        let sig = self.sign(&q);
        format!("{}{}?{}&signature={}", self.base_url, path, q, sig)
    }

    // ✅ P41/D13: reqwest включает полный URL (с подписью) в Display ошибки —
    // подписанные URL не должны попадать в логи. without_url() вычищает URL.
    fn no_url_err(e: reqwest::Error) -> anyhow::Error {
        anyhow::Error::msg(e.without_url())
    }
}

#[async_trait]
impl ExchangeClient for BinanceFuturesClient {
    async fn sync_time(&self) -> Result<()> {
        let url = format!("{}/fapi/v1/time", self.base_url);
        let json: Value = self.client.get(&url).send().await.map_err(Self::no_url_err)?.json().await.map_err(Self::no_url_err)?;
        // ✅ P36/D1: serverTime отсутствует/битый → ОШИБКА, а не offset =
        // минус локальное время (гарантированные -1021 на каждом подписанном
        // запросе до рестарта). Вызывающий в main.rs ретраит 5 раз и
        // продолжает с offset=0 (локальные часы), что безопаснее мусора.
        let Some(server_ms) = json["serverTime"].as_i64() else {
            return Err(anyhow!("sync_time: в ответе нет serverTime"));
        };
        let local_ms = chrono::Utc::now().timestamp_millis();
        let offset = server_ms - local_ms;
        self.time_sync.set_offset(offset);
        tracing::info!("⏰ Futures clock offset: {} ms", offset);
        Ok(())
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
        let url = self.signed_url("/fapi/v1/order", &query); // ✅ P41/D13+D14

        let resp = self.client.post(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await.map_err(Self::no_url_err)?;
        let json: Value = resp.json().await.map_err(Self::no_url_err)?;
        if let Some(err) = json.get("msg") {
            return Err(anyhow!("Binance Futures error: {}", err));
        }
        Ok(OrderResult {
            order_id: json["orderId"].as_u64().unwrap_or(0),
            symbol: json["symbol"].as_str().unwrap_or(symbol).to_string(),
            side: json["side"].as_str().unwrap_or(side).to_string(),
            price: json["avgPrice"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
            quantity: json["origQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(quantity),
            executed_qty: json["executedQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
            status: json["status"].as_str().unwrap_or("UNKNOWN").to_string(),
            commission: 0.0,
        })
    }


    // ✅ P41/E14: reduceOnly-выход (см. traits.rs). minNotional на reduceOnly
    // не действует — пыль закрывается ордером; дрейф стейта даёт отказ (-2022),
    // а не случайное открытие противоположной позиции.
    async fn place_market_order_reduce_only(
        &self,
        symbol: &str,
        side: &str,
        quantity: f64,
        client_order_id: Option<String>,
    ) -> Result<OrderResult> {
        let ts = self.now_ms();
        let qty_str = format!("{}", quantity);
        let mut query = format!(
            "symbol={}&side={}&type=MARKET&quantity={}&reduceOnly=true&timestamp={}",
            symbol, side, qty_str, ts
        );
        if let Some(id) = client_order_id {
            query.push_str(&format!("&newClientOrderId={}", id));
        }
        let url = self.signed_url("/fapi/v1/order", &query);
        let resp = self.client.post(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await.map_err(Self::no_url_err)?;
        let json: Value = resp.json().await.map_err(Self::no_url_err)?;
        if let Some(err) = json.get("msg") {
            return Err(anyhow!("Binance Futures error: {}", err));
        }
        Ok(OrderResult {
            order_id: json["orderId"].as_u64().unwrap_or(0),
            symbol: json["symbol"].as_str().unwrap_or(symbol).to_string(),
            side: json["side"].as_str().unwrap_or(side).to_string(),
            price: json["avgPrice"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
            quantity: json["origQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(quantity),
            executed_qty: json["executedQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
            status: json["status"].as_str().unwrap_or("UNKNOWN").to_string(),
            commission: json["commission"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
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
            "symbol={}&side={}&type=LIMIT&timeInForce=GTX&quantity={}&price={}&timestamp={}",
            symbol, side, qty_str, price_str, ts
        );
        if let Some(id) = client_order_id {
            query.push_str(&format!("&newClientOrderId={}", id));
        }
        let url = self.signed_url("/fapi/v1/order", &query); // ✅ P41/D13+D14

        let resp = self.client.post(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await.map_err(Self::no_url_err)?;
        let json: Value = resp.json().await.map_err(Self::no_url_err)?;
        if let Some(err) = json.get("msg") {
            return Err(anyhow!("Binance Futures error: {}", err));
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

    // ============================================================
    // 🔧 ЭТАП B v3: защитные ордера (STOP_MARKET / TAKE_PROFIT_MARKET).
    // Тестнет принимает их только через Algo Order API.
    // История ошибок API — три круга валидации, каждый называл следующий
    // обязательный параметр (в НИЖНЕМ регистре!):
    //   круг 1: нужен  'type'          → добавили type=
    //   круг 2: нужен  'algotype'      → добавили algotype=CONDITIONAL
    //   круг 3: нужен  'triggerprice'  → ЗАМЕНЯЕМ stopPrice= на triggerprice=
    // Классический /fapi/v1/order остаётся fallback'ом (он понимает stopPrice).
    // ============================================================
        // ============================================================
    // 🔧 ЭТАП B v4: защитные ордера с ЦЕПОЧКОЙ ТОЧНОСТЕЙ.
    // История валидации Algo API: круг 1 'type', круг 2 'algotype',
    // круг 3 'triggerprice', круг 4 = -1111 Precision (цена не в tickSize).
    // Решение: пробуем цену с 8 → 7 → 6 знаками; -1111 = понизить
    // разрядность. Успешная попытка логирует найденную точность —
    // это и есть фактический tickSize Algo-эндпоинта.
    // Классический /fapi/v1/order остаётся финальным fallback'ом.
    // ============================================================
    async fn place_protective_order(
        &self,
        symbol: &str,
        side: &str,
        stop: bool,
        trigger_price: f64,
        client_order_id: Option<String>,
    ) -> Result<OrderResult> {
        let ord_type = if stop { "STOP_MARKET" } else { "TAKE_PROFIT_MARKET" };

        // ---------- Цепочка точностей на Algo Order API ----------
        for &dec in &[8usize, 7, 6] {
            let tp_str = format!("{:.*}", dec, trigger_price);
            let ts = self.now_ms();
            let mut q = format!(
                "symbol={}&side={}&type={}&algotype=CONDITIONAL&triggerprice={}&closePosition=true&timestamp={}",
                symbol, side, ord_type, tp_str, ts
            );
            if let Some(id) = &client_order_id {
                q.push_str(&format!("&newClientOrderId={}", id));
            }
            let url = self.signed_url("/fapi/v1/algoOrder", &q); // ✅ P41/D13+D14
            let resp = self.client.post(&url)
                .header("X-MBX-APIKEY", &self.api_key)
                .send().await.map_err(Self::no_url_err)?;
            let json: Value = resp.json().await.map_err(Self::no_url_err)?;

            // Успех = в ответе нет "code" (ошибка приходит как {"code":..,"msg":..})
            if json["code"].as_i64().is_none() {
                let oid = json["algoId"].as_u64()
                    .or_else(|| json["orderId"].as_u64())
                    .unwrap_or(0);
                tracing::info!(
                    "[{}] 🛡 защитный {} поставлен через Algo API (id={}, trigger={}, точность={} зн.)",
                    symbol, ord_type, oid, tp_str, dec
                );
                return Ok(OrderResult {
                    order_id: oid,
                    symbol: symbol.to_string(),
                    side: side.to_string(),
                    price: trigger_price,
                    quantity: 0.0,
                    executed_qty: 0.0,
                    status: json["algoStatus"].as_str()
                        .or_else(|| json["status"].as_str())
                        .unwrap_or("NEW").to_string(),
                    commission: 0.0,
                });
            }

            let code = json["code"].as_i64().unwrap_or(0);
            let msg = json["msg"].as_str().unwrap_or("?").to_string();
            tracing::warn!(
                "[{}] Algo API (точность {} зн., цена '{}') отказал (code={}): {}",
                symbol, dec, tp_str, code, msg
            );

            // -1111 = проблема точности → пробуем следующую разрядность.
            // Любая другая ошибка → точность не поможет, выходим в fallback.
            if code != -1111 {
                break;
            }
        }

        // ---------- Попытка 2: классический /fapi/v1/order (fallback) ----------
        let ts2 = self.now_ms();
        let tp_str2 = format!("{:.8}", trigger_price);
        // AUDIT-P8: workingType=MARK_PRICE — дефолт классики CONTRACT_PRICE,
        // триггер по last price: спайк last price на тестнете срывает стоп
        // без реального движения mark price. Mark price устойчив к всплескам.
        // В Algo-путь выше параметр НЕ добавлен сознательно: эндпоинт
        // нестандартный, главный путь SL не ломаем до подтверждения того,
        // что лишний параметр там безопасен.
        let mut q2 = format!(
            "symbol={}&side={}&type={}&stopPrice={}&closePosition=true&workingType=MARK_PRICE&timestamp={}",
            symbol, side, ord_type, tp_str2, ts2
        );
        if let Some(id) = client_order_id {
            q2.push_str(&format!("&newClientOrderId={}", id));
        }
        let url2 = self.signed_url("/fapi/v1/order", &q2); // ✅ P41/D13+D14
        let resp2 = self.client.post(&url2)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await.map_err(Self::no_url_err)?;
        let json2: Value = resp2.json().await.map_err(Self::no_url_err)?;
        if let Some(err) = json2.get("msg") {
            return Err(anyhow!("Binance Futures защитный ордер: {}", err));
        }
        Ok(OrderResult {
            order_id: json2["orderId"].as_u64().unwrap_or(0),
            symbol: symbol.to_string(),
            side: side.to_string(),
            price: trigger_price,
            quantity: 0.0,
            executed_qty: 0.0,
            status: json2["status"].as_str().unwrap_or("NEW").to_string(),
            commission: 0.0,
        })
    }

    async fn cancel_order(&self, symbol: &str, order_id: u64) -> Result<bool> {
        let ts = self.now_ms();
        let query = format!("symbol={}&orderId={}&timestamp={}", symbol, order_id, ts);
        let url = self.signed_url("/fapi/v1/order", &query); // ✅ P41/D13+D14

        let resp = self.client.delete(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await.map_err(Self::no_url_err)?;
        let status = resp.status();
        let json: Value = resp.json().await.unwrap_or(Value::Null);

        if status.is_success() {
            return Ok(true);
        }
        // AUDIT-P1: -2011 = «не знаю такой ордер» — для algoId это НОРМА:
        // классический /fapi/v1/order не видит Algo-ордера. Фолбэк:
        // DELETE /fapi/v1/algoOrder?algoId=... (тот же стиль, что постановка).
        if json["code"].as_i64() == Some(-2011) {
            let ts2 = self.now_ms();
            let q2 = format!("symbol={}&algoId={}&timestamp={}", symbol, order_id, ts2);
            let url2 = self.signed_url("/fapi/v1/algoOrder", &q2); // ✅ P41/D13+D14
            let resp2 = self.client.delete(&url2)
                .header("X-MBX-APIKEY", &self.api_key)
                .send().await.map_err(Self::no_url_err)?;
            let st2 = resp2.status();
            let json2: Value = resp2.json().await.unwrap_or(Value::Null);
            if st2.is_success() && json2["code"].as_i64().is_none() {
                tracing::info!("[{}] cancel #{}: отмена выполнена через Algo API (algoId)", symbol, order_id);
                return Ok(true);
            }
            tracing::warn!("[{}] cancel #{}: нет ни в классике (-2011), ни в Algo API (http={}, code={}: {}) — если это живой SL/TP, СВЕРИТЬ openOrders (риск дубля)", symbol, order_id, st2.as_u16(), json2["code"].as_i64().unwrap_or(0), json2["msg"].as_str().unwrap_or("?"));
            return Ok(false);
        }
        Err(anyhow!("cancel {} #{}: {}", symbol, order_id, json["msg"].as_str().unwrap_or("?")))
    }

    async fn get_order_status(&self, symbol: &str, order_id: u64) -> Result<OrderResult> {
        let ts = self.now_ms();
        let query = format!("symbol={}&orderId={}&timestamp={}", symbol, order_id, ts);
        let url = self.signed_url("/fapi/v1/order", &query); // ✅ P41/D13+D14

        let resp = self.client.get(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await.map_err(Self::no_url_err)?;
        let json: Value = resp.json().await.map_err(Self::no_url_err)?;

        // AUDIT-P1: классика не знает algoId (ответ = {"code":-2011,...}).
        // Фолбэк: GET /fapi/v1/algoOrder?algoId=...; если эндпоинт/поля иные —
        // прозрачно уходим в прежнее поведение (UNKNOWN) с явным warn.
        if json["code"].as_i64().is_some() {
            let ts2 = self.now_ms();
            let q2 = format!("symbol={}&algoId={}&timestamp={}", symbol, order_id, ts2);
            let url2 = self.signed_url("/fapi/v1/algoOrder", &q2); // ✅ P41/D13+D14
            let resp2 = self.client.get(&url2)
                .header("X-MBX-APIKEY", &self.api_key)
                .send().await.map_err(Self::no_url_err)?;
            let json2: Value = resp2.json().await.unwrap_or(Value::Null);
            let st2 = json2["algoStatus"].as_str().or_else(|| json2["status"].as_str());
            if json2["code"].as_i64().is_none() {
                if let Some(s2) = st2 {
                    tracing::info!("[{}] get_order_status #{}: статус получен через Algo API: {}", symbol, order_id, s2);
                    return Ok(OrderResult {
                        order_id,
                        symbol: symbol.to_string(),
                        side: json2["side"].as_str().unwrap_or("").to_string(),
                        price: json2["triggerprice"].as_str().and_then(|p| p.parse().ok())
                            .or_else(|| json2["triggerPrice"].as_str().and_then(|p| p.parse().ok()))
                            .unwrap_or(0.0),
                        quantity: 0.0,
                        executed_qty: 0.0,
                        status: s2.to_string(),
                        commission: 0.0,
                    });
                }
                tracing::warn!("[{}] get_order_status #{}: классика: code={}: {}; Algo API без распознанного статуса — вернуть UNKNOWN", symbol, order_id, json["code"].as_i64().unwrap_or(0), json["msg"].as_str().unwrap_or("?"));
            }
        }

        Ok(OrderResult {
            order_id,
            symbol: symbol.to_string(),
            side: json["side"].as_str().unwrap_or("").to_string(),
            price: {
                let avg: f64 = json["avgPrice"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let lim: f64 = json["price"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                if avg > 0.0 { avg } else { lim }
            },
            quantity: json["origQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
            executed_qty: json["executedQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
            status: json["status"].as_str().unwrap_or("UNKNOWN").to_string(),
            commission: 0.0,
        })
    }

    // ============================================================
    // 🔧 Поиск ордера по origClientOrderId — используется движком
    // после упавшего POST (resolve_by_client_id).
    // ============================================================
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
        let url = self.signed_url("/fapi/v1/order", &query); // ✅ P41/D13+D14

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
                let avg: f64 = json["avgPrice"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let lim: f64 = json["price"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                if avg > 0.0 { avg } else { lim }
            },
            quantity: json["origQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
            executed_qty: json["executedQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
            status: json["status"].as_str().unwrap_or("UNKNOWN").to_string(),
            commission: 0.0,
        })
    }

    // ============================================================
    // 🔧 Живые ордера символа (sweep при shutdown, ручные сверки)
    // ============================================================
    async fn get_open_orders(&self, symbol: &str) -> Result<Vec<OrderResult>> {
        let ts = self.now_ms();
        let query = format!("symbol={}&timestamp={}", symbol, ts);
        let url = self.signed_url("/fapi/v1/openOrders", &query); // ✅ P41/D13+D14

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
                let avg: f64 = o["avgPrice"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let lim: f64 = o["price"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                // ✅ P22: для STOP_MARKET отдаём stopPrice в поле price.
                // Единственный потребитель, которому это важно, — перехват
                // биржевого SL при усыновлении сирот (main.rs, P22): фильтр
                // side==закрытие && price>0 && quantity==0 (closePosition=true).
                // STOP_MARKET всегда имеет price="" на бирже, поэтому раньше
                // price=0.0 и цену стопа прочитать было невозможно. Остальные
                // потребители (sweep при shutdown) используют только id/status.
                let stp: f64 = if o["type"].as_str() == Some("STOP_MARKET") {
                    o["stopPrice"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0)
                } else {
                    0.0
                };
                out.push(OrderResult {
                    order_id: o["orderId"].as_u64().unwrap_or(0),
                    symbol: o["symbol"].as_str().unwrap_or(symbol).to_string(),
                    side: o["side"].as_str().unwrap_or("").to_string(),
                    price: if stp > 0.0 { stp } else if avg > 0.0 { avg } else { lim },
                    quantity: o["origQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
                    executed_qty: o["executedQty"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
                    status: o["status"].as_str().unwrap_or("UNKNOWN").to_string(),
                    commission: 0.0,
                });
            }
        }
        Ok(out)
    }

    // ============================================================
    // ✅ P53/C4-2: открытые алго-ордера символа (сверка защит C4).
    // GET /fapi/v1/openAlgoOrders — sibling /fapi/v1/algoOrder (тот же
    // семейство эндпоинтов, что place/cancel/get выше). Парсинг терпимый:
    // поля тестнета алго-семейства гуляют (algoId/orderId, algoType/type,
    // algoStatus/status, triggerPrice/stopPrice).
    // ============================================================
    async fn get_open_algo_orders(&self, symbol: &str) -> Result<Vec<AlgoOrderInfo>> {
        let ts = self.now_ms();
        let query = format!("symbol={}&timestamp={}", symbol, ts);
        let url = self.signed_url("/fapi/v1/openAlgoOrders", &query); // ✅ P53/C4-2

        let resp = self.client.get(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await.map_err(Self::no_url_err)?;
        let json: Value = resp.json().await.map_err(Self::no_url_err)?;

        // Ошибка семейства algo API приходит как {"code":..,"msg":..}
        if json["code"].as_i64().is_some() && json.as_array().is_none() {
            return Err(anyhow!(
                "openAlgoOrders {}: code={}: {}",
                symbol,
                json["code"].as_i64().unwrap_or(0),
                json["msg"].as_str().unwrap_or("?")
            ));
        }

        let mut out = Vec::new();
        if let Some(arr) = json.as_array() {
            for o in arr {
                let trigger: f64 = o["triggerPrice"].as_str().and_then(|s| s.parse().ok())
                    .or_else(|| o["triggerPrice"].as_f64())
                    .or_else(|| o["stopPrice"].as_str().and_then(|s| s.parse().ok()))
                    .or_else(|| o["stopPrice"].as_f64())
                    .or_else(|| o["price"].as_str().and_then(|s| s.parse().ok()))
                    .unwrap_or(0.0);
                let close_position = o["closePosition"].as_bool()
                    .unwrap_or(false);
                out.push(AlgoOrderInfo {
                    algo_id: o["algoId"].as_u64()
                        .or_else(|| o["orderId"].as_u64())
                        .unwrap_or(0),
                    symbol: o["symbol"].as_str().unwrap_or(symbol).to_string(),
                    side: o["side"].as_str().unwrap_or("").to_string(),
                    algo_type: o["algoType"].as_str()
                        .or_else(|| o["orderType"].as_str())
                        .or_else(|| o["type"].as_str())
                        .unwrap_or("UNKNOWN")
                        .to_string(),
                    status: o["algoStatus"].as_str()
                        .or_else(|| o["status"].as_str())
                        .unwrap_or("UNKNOWN")
                        .to_string(),
                    trigger_price: trigger,
                    close_position,
                });
            }
        }
        Ok(out)
    }

    async fn get_balance(&self, asset: &str) -> Result<f64> {
        let ts = self.now_ms();
        let query = format!("timestamp={}", ts);
        let url = self.signed_url("/fapi/v2/balance", &query); // ✅ P41/D13+D14

        let resp = self.client.get(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await.map_err(Self::no_url_err)?;
        let json: Value = resp.json().await.map_err(Self::no_url_err)?;

        // ✅ P36/C1c: не-массив ответ = ошибка (раньше тихо Ok(0.0)).
        if json.as_array().is_none() {
            return Err(anyhow!(
                "balance: не-массив ответ (code={}, msg={})",
                json["code"].as_i64().unwrap_or(0),
                json["msg"].as_str().unwrap_or("?")
            ));
        }
        if let Some(balances) = json.as_array() {
            for b in balances {
                if b["asset"].as_str() == Some(asset) {
                    return Ok(b["availableBalance"]
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
            "{}/fapi/v1/depth?symbol={}&limit={}",
            self.base_url, symbol, limit
        );
        let json: Value = self.client.get(&url).send().await.map_err(Self::no_url_err)?.json().await.map_err(Self::no_url_err)?;

        let bids = json["bids"].as_array().map(|arr| {
            arr.iter().filter_map(|v| {
                let p = v[0].as_str()?.parse::<f64>().ok()?;
                let q = v[1].as_str()?.parse::<f64>().ok()?;
                Some((p, q))
            }).collect()
        }).unwrap_or_default();

        let asks = json["asks"].as_array().map(|arr| {
            arr.iter().filter_map(|v| {
                let p = v[0].as_str()?.parse::<f64>().ok()?;
                let q = v[1].as_str()?.parse::<f64>().ok()?;
                Some((p, q))
            }).collect()
        }).unwrap_or_default();

        Ok((bids, asks))
    }

    async fn get_positions(&self) -> Result<Vec<ExchangePosition>> {
        let ts = self.now_ms();
        let query = format!("timestamp={}", ts);
        let url = self.signed_url("/fapi/v2/positionRisk", &query); // ✅ P41/D13+D14

        let resp = self.client.get(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await.map_err(Self::no_url_err)?;
        let json: Value = resp.json().await.map_err(Self::no_url_err)?;

        // ✅ P36/C1a: ответ биржи, который НЕ массив ({"code":-1003,…} при
        // IP-ban, {"code":-2015,…} при плохом ключе, HTML 200 после прокси) —
        // это ОШИБКА, а не «позиций нет». Раньше тихо возвращался Ok(vec![])
        // и pre-entry reconcile верил, что биржа flat.
        if json.as_array().is_none() {
            return Err(anyhow!(
                "positionRisk: не-массив ответ (code={}, msg={})",
                json["code"].as_i64().unwrap_or(0),
                json["msg"].as_str().unwrap_or("?")
            ));
        }
        let mut out = Vec::new();
        if let Some(arr) = json.as_array() {
            for p in arr {
                let symbol = p["symbol"].as_str().unwrap_or("").to_string();
                let qty: f64 = p["positionAmt"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let entry: f64 = p["entryPrice"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                if qty.abs() > 0.0 {
                    out.push(ExchangePosition { symbol, qty, entry_price: entry });
                }
            }
        }
        Ok(out)
    }

    async fn get_order_commission(&self, symbol: &str, order_id: u64) -> Result<f64> {
        let ts = self.now_ms();
        let query = format!(
            "symbol={}&incomeType=COMMISSION&orderId={}&limit=50&timestamp={}",
            symbol, order_id, ts
        );
        let url = self.signed_url("/fapi/v1/income", &query); // ✅ P41/D13+D14

        let resp = self.client.get(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await.map_err(Self::no_url_err)?;
        let json: Value = resp.json().await.map_err(Self::no_url_err)?;
        if !json.is_array() {
            // ✅ P36/C1c: не-массив = ошибка (раньше тихо Ok(0.0));
            // вызывающий recover_commissions обрабатывает Err мягко.
            return Err(anyhow!(
                "income: не-массив ответ (code={}, msg={})",
                json["code"].as_i64().unwrap_or(0),
                json["msg"].as_str().unwrap_or("?")
            ));
        }
        let mut total = 0.0;
        if let Some(arr) = json.as_array() {
            for row in arr {
                if let Some(inc) = row["income"].as_str().and_then(|s| s.parse::<f64>().ok()) {
                    total += inc.abs();
                }
            }
        }
        Ok(total)
    }

    // ============================================================
    // ✅ P37/K3: плечо + режим маржи при старте. Раньше задавались только
    // вручную на бирже, конфиг молча расходился с фактом.
    // marginType: -4046 «No need to change margin type» = уже ISOLATED → ОК.
    // Верификация: GET positionRisk по символу → поле leverage.
    // ============================================================
    async fn apply_margin_settings(
        &self,
        symbol: &str,
        leverage: u32,
        isolated: bool,
    ) -> Result<()> {
        if isolated {
            let ts = self.now_ms();
            let q = format!("symbol={}&marginType=ISOLATED&timestamp={}", symbol, ts);
            let url = self.signed_url("/fapi/v1/marginType", &q); // ✅ P41/D13+D14
            let resp = self.client.post(&url)
                .header("X-MBX-APIKEY", &self.api_key)
                .send().await.map_err(Self::no_url_err)?;
            let json: Value = resp.json().await.map_err(Self::no_url_err)?;
            match json["code"].as_i64() {
                None => tracing::info!("[{}] ✅ margin mode: ISOLATED установлен", symbol),
                Some(-4046) => tracing::info!("[{}] margin mode уже ISOLATED (-4046)", symbol),
                Some(c) => {
                    return Err(anyhow!(
                        "marginType ISOLATED отказал (code={}): {}",
                        c,
                        json["msg"].as_str().unwrap_or("?")
                    ));
                }
            }
        }
        let ts = self.now_ms();
        let q = format!("symbol={}&leverage={}&timestamp={}", symbol, leverage, ts);
        let url = self.signed_url("/fapi/v1/leverage", &q); // ✅ P41/D13+D14
        let resp = self.client.post(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await.map_err(Self::no_url_err)?;
        let json: Value = resp.json().await.map_err(Self::no_url_err)?;
        if let Some(c) = json["code"].as_i64() {
            return Err(anyhow!(
                "leverage {} отказал (code={}): {}",
                leverage, c, json["msg"].as_str().unwrap_or("?")
            ));
        }
        // Сверка факта: positionRisk по символу отдаёт поле leverage.
        let ts2 = self.now_ms();
        let q2 = format!("symbol={}&timestamp={}", symbol, ts2);
        let url2 = self.signed_url("/fapi/v2/positionRisk", &q2); // ✅ P41/D13+D14
        let resp2 = self.client.get(&url2)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await.map_err(Self::no_url_err)?;
        let json2: Value = resp2.json().await.map_err(Self::no_url_err)?;
        if let Some(arr) = json2.as_array() {
            for p in arr {
                if p["symbol"].as_str() == Some(symbol) {
                    let lev = p["leverage"]
                        .as_str()
                        .and_then(|s| s.parse::<u32>().ok())
                        .or_else(|| p["leverage"].as_u64().map(|v| v as u32))
                        .unwrap_or(0);
                    if lev != leverage {
                        return Err(anyhow!(
                            "сверка positionRisk: плечо на бирже={}, в конфиге={}",
                            lev, leverage
                        ));
                    }
                    tracing::info!("[{}] ✅ плечо подтверждено биржей: x{}", symbol, lev);
                    return Ok(());
                }
            }
        }
        Err(anyhow!("сверка positionRisk: символ {} не найден в ответе", symbol))
    }
}