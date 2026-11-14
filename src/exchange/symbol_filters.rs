//! ✅ Фаза 2: единый источник фильтров символа (exchangeInfo).
//! Заменяет шесть слоёв захардкоженных BTC-констант.
//! Бонус: несуществующий/делистнутый символ теперь валит старт
//! с понятной ошибкой вместо тихо мёртвого бота (В-19).

use anyhow::{anyhow, Result};
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct SymbolFilters {
    pub tick_size: Decimal,
    pub step_size: Decimal,
    pub min_qty: Decimal,
    pub max_qty: Decimal,
    pub min_notional: Decimal,
}

impl SymbolFilters {
    /// Цена, кратная тику (к ближайшему) — для лимитных ордеров.
    pub fn round_price(&self, price: Decimal) -> Decimal {
        if self.tick_size.is_zero() { return price; }
        (price / self.tick_size).round() * self.tick_size
    }

    /// Объём ВНИЗ к шагу — заказать больше рассчитанного нельзя.
    pub fn floor_qty(&self, qty: Decimal) -> Decimal {
        if self.step_size.is_zero() { return qty; }
        (qty / self.step_size).floor() * self.step_size
    }

    pub fn passes_notional(&self, price: Decimal, qty: Decimal) -> bool {
        price * qty >= self.min_notional
    }

    pub fn passes_qty_bounds(&self, qty: Decimal) -> bool {
        qty >= self.min_qty && qty <= self.max_qty
    }
}

#[derive(Debug, Default)]
pub struct FilterRegistry {
    filters: HashMap<String, SymbolFilters>,
}

impl FilterRegistry {
    /// Один запрос exchangeInfo при старте на рынок (spot/futures).
        /// ✅ Лёгкая загрузка: один запрос на символ (?symbol=..., килобайты вместо мегабайт)
    /// + 3 попытки — тестнет бывает медленным.
    pub async fn load(base_url: &str, is_futures: bool, symbols: &[String]) -> Result<Self> {
        let path = if is_futures { "fapi/v1/exchangeInfo" } else { "api/v3/exchangeInfo" };
        let base = base_url.trim_end_matches('/').to_string();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(20)).tcp_keepalive(std::time::Duration::from_secs(15))/* P19-POOL */.build()?;

        tracing::info!("📊 Загружаю фильтры: {}/{} (символов: {})", base, path, symbols.len());

        let wanted: HashSet<String> = symbols.iter().map(|s| s.to_uppercase()).collect();
        let mut filters = HashMap::new();

        for sym in &wanted {
            let url = format!("{}/{}?symbol={}", base, path, sym);
            let resp: Value = fetch_json_with_retry(&client, &url, 3).await?;
            let arr = resp["symbols"].as_array()
                .ok_or_else(|| anyhow!("exchangeInfo {}: нет массива symbols", sym))?;

            for s in arr {
                if s["symbol"].as_str().unwrap_or("").to_uppercase() != *sym {
                    continue;
                }
                if s["status"].as_str().unwrap_or("") != "TRADING" {
                    return Err(anyhow!("символ {} не в статусе TRADING", sym));
                }

                let mut tick = Decimal::ZERO;
                let mut step = Decimal::ZERO;
                let mut min_qty = Decimal::ZERO;
                let mut max_qty = Decimal::MAX;
                let mut min_notional = Decimal::ZERO;

                if let Some(list) = s["filters"].as_array() {
                    for f in list {
                        match f["filterType"].as_str().unwrap_or("") {
                            "PRICE_FILTER" => tick = dec(&f["tickSize"]),
                            "LOT_SIZE" => {
                                step = dec(&f["stepSize"]);
                                min_qty = dec(&f["minQty"]);
                                max_qty = dec(&f["maxQty"]);
                            }
                            "MIN_NOTIONAL" => {
                                min_notional = dec(&f["notional"]);
                                if min_notional.is_zero() {
                                    min_notional = dec(&f["minNotional"]);
                                }
                            }
                            _ => {}
                        }
                    }
                }

                if tick.is_zero() || step.is_zero() {
                    return Err(anyhow!("{}: не найдены tickSize/stepSize", sym));
                }
                filters.insert(sym.clone(), SymbolFilters {
                    tick_size: tick, step_size: step, min_qty, max_qty, min_notional,
                });
            }
        }

        for w in &wanted {
            if !filters.contains_key(w) {
                return Err(anyhow!(
                    "символ {} не найден (рынок: {})",
                    w, if is_futures { "futures" } else { "spot" }
                ));
            }
        }
        tracing::info!("✅ Фильтры загружены: {} символов", filters.len());
        Ok(Self { filters })
    }

        /// 🆕 Phase B: загрузка фильтров с Bybit V5 /v5/market/instruments-info
    /// Парсит формат Bybit:
    ///   {
    ///     "result": {
    ///       "list": [{
    ///         "symbol": "XAUUSDT",
    ///         "status": "Trading",
    ///         "priceFilter": {"minPrice":"0.01","maxPrice":"199999.98","tickSize":"0.01"},
    ///         "lotSizeFilter": {"maxOrderQty":"500","minOrderQty":"0.001","qtyStep":"0.001"}
    ///       }]
    ///     }
    ///   }
    pub async fn load_bybit(base_url: &str, symbols: &[String]) -> Result<Self> {
        let base = base_url.trim_end_matches('/').to_string();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(20)).tcp_keepalive(std::time::Duration::from_secs(15))/* P19-POOL */.build()?;

        tracing::info!("📊 Загружаю фильтры Bybit: {}/v5/market/instruments-info (символов: {})",
            base, symbols.len());

        let wanted: HashSet<String> = symbols.iter().map(|s| s.to_uppercase()).collect();
        let mut filters = HashMap::new();

        // Bybit позволяет запросить конкретный символ через ?symbol=XAUUSDT
        for sym in &wanted {
            let url = format!("{}/v5/market/instruments-info?category=linear&symbol={}", base, sym);
            let resp: Value = fetch_json_with_retry(&client, &url, 3).await?;

            // Проверяем retCode (Bybit V5 convention)
            let ret_code = resp["retCode"].as_i64().unwrap_or(-1);
            if ret_code != 0 {
                return Err(anyhow!("Bybit instruments-info {}: retCode={} msg={}",
                    sym, ret_code, resp["retMsg"].as_str().unwrap_or("?")));
            }

            let list = resp["result"]["list"]
                .as_array()
                .ok_or_else(|| anyhow!("Bybit {}: нет массива result.list", sym))?;

            for s in list {
                if s["symbol"].as_str().unwrap_or("").to_uppercase() != *sym {
                    continue;
                }
                // Bybit status: "Trading" (capital T) vs Binance "TRADING"
                let status = s["status"].as_str().unwrap_or("");
                if status != "Trading" {
                    return Err(anyhow!("символ {} не в статусе Trading (статус: {})", sym, status));
                }

                // priceFilter
                let pf = &s["priceFilter"];
                let tick = dec(&pf["tickSize"]);

                // lotSizeFilter
                let lf = &s["lotSizeFilter"];
                let step = dec(&lf["qtyStep"]);
                let min_qty = dec(&lf["minOrderQty"]);
                let max_qty = dec(&lf["maxOrderQty"]);

                // Bybit: minNotionalValue поле на уровне lotSizeFilter (не отдельный фильтр)
                let min_notional = dec(&lf["minNotionalValue"]);

                if tick.is_zero() || step.is_zero() {
                    return Err(anyhow!("Bybit {}: не найдены tickSize/stepSize", sym));
                }
                filters.insert(sym.clone(), SymbolFilters {
                    tick_size: tick, step_size: step, min_qty, max_qty, min_notional,
                });
            }
        }

        for w in &wanted {
            if !filters.contains_key(w) {
                return Err(anyhow!("символ {} не найден на Bybit", w));
            }
        }
        tracing::info!("✅ Bybit фильтры загружены: {} символов", filters.len());
        Ok(Self { filters })
    }

    /// v4.0: OKX V5 /api/v5/public/instruments (SWAP).
    /// Engine symbols ("XAUUSDT") are queried as "XAU-USDT-SWAP"
    /// and stored under the engine symbol so filters.get("XAUUSDT") works.
    pub async fn load_okx(base_url: &str, symbols: &[String]) -> Result<Self> {
        let base = base_url.trim_end_matches('/').to_string();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(20)).tcp_keepalive(std::time::Duration::from_secs(15))/* P19-POOL */.build()?;

        tracing::info!(
            "Loading OKX filters: {}/api/v5/public/instruments ({} symbols)",
            base,
            symbols.len()
        );

        let mut filters = HashMap::new();

        for sym in symbols {
            let sym_u = sym.to_uppercase();
            let inst_id = okx_inst_id(&sym_u);
            let url = format!(
                "{}/api/v5/public/instruments?instType=SWAP&instId={}",
                base, inst_id
            );
            let resp: serde_json::Value = fetch_json_with_retry(&client, &url, 3).await?;

            let code = resp["code"].as_str().unwrap_or("-1").to_string();
            if code != "0" {
                return Err(anyhow::anyhow!(
                    "OKX instruments {}: code={} msg={}",
                    sym_u,
                    code,
                    resp["msg"].as_str().unwrap_or("?")
                ));
            }

            let d = resp["data"]
                .as_array()
                .and_then(|a| a.first())
                .ok_or_else(|| anyhow::anyhow!("OKX {}: instrument not found", inst_id))?;

            let state = d["state"].as_str().unwrap_or("");
            if state != "live" {
                return Err(anyhow::anyhow!("symbol {} is not live (state: {})", sym_u, state));
            }

            let tick = dec(&d["tickSz"]);
            let step = dec(&d["lotSz"]);
            let min_qty = dec(&d["minSz"]);
            let mut max_qty = dec(&d["maxLmtSz"]);
            if max_qty.is_zero() {
                max_qty = Decimal::MAX;
            }
            // OKX perpetuals have no MIN_NOTIONAL filter; the gate passes always.
            let min_notional = Decimal::ZERO;

            if tick.is_zero() || step.is_zero() {
                return Err(anyhow::anyhow!("OKX {}: tickSz/lotSz missing", sym_u));
            }

            filters.insert(
                sym_u.clone(),
                SymbolFilters {
                    tick_size: tick,
                    step_size: step,
                    min_qty,
                    max_qty,
                    min_notional,
                },
            );
        }

        for w in symbols {
            let w = w.to_uppercase();
            if !filters.contains_key(&w) {
                return Err(anyhow::anyhow!("symbol {} not found on OKX", w));
            }
        }

        tracing::info!("OKX filters loaded: {} symbols", filters.len());
        Ok(Self { filters })
    }
    pub fn get(&self, symbol: &str) -> Result<&SymbolFilters> {
        self.filters.get(&symbol.to_uppercase())
            .ok_or_else(|| anyhow!("нет фильтров для {} (не загружен при старте)", symbol))
    }
}

fn dec(v: &Value) -> Decimal {
    v.as_str().and_then(|s| s.parse::<Decimal>().ok()).unwrap_or(Decimal::ZERO)
}

/// GET → JSON с ретраями и паузами.
async fn fetch_json_with_retry(client: &reqwest::Client, url: &str, attempts: u32) -> Result<Value> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=attempts {
        match client.get(url).send().await {
            Ok(r) => match r.json::<Value>().await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    tracing::warn!("📊 попытка {}/{}: {}", attempt, attempts, e);
                    last_err = Some(e.into());
                }
            },
            Err(e) => {
                tracing::warn!("📊 попытка {}/{}: {}", attempt, attempts, e);
                last_err = Some(e.into());
            }
        }
        if attempt < attempts {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("unknown error")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doge_like() -> SymbolFilters {
        SymbolFilters {
            tick_size: "0.0001".parse().unwrap(),
            step_size: "1".parse().unwrap(),
            min_qty: "1".parse().unwrap(),
            max_qty: "50000000".parse().unwrap(),
            min_notional: "5".parse().unwrap(),
        }
    }

    #[test]
    fn round_price_to_tick() {
        let f = doge_like();
        assert_eq!(f.round_price("0.163256".parse().unwrap()), "0.1633".parse::<Decimal>().unwrap());
    }

    #[test]
    fn floor_qty_integer_step() {
        let f = doge_like();
        assert_eq!(f.floor_qty("1234.999".parse().unwrap()), "1234".parse::<Decimal>().unwrap());
    }

    #[test]
    fn notional_check() {
        let f = doge_like();
        assert!(f.passes_notional("0.1633".parse().unwrap(), "1234".parse().unwrap()));
        assert!(!f.passes_notional("0.1633".parse().unwrap(), "10".parse().unwrap()));
    }

    #[test]
    fn zero_tick_is_noop() {
        let mut f = doge_like();
        f.tick_size = Decimal::ZERO;
        let p: Decimal = "0.123456".parse().unwrap();
        assert_eq!(f.round_price(p), p);
    }
}
/// v4.0: "XAUUSDT" -> "XAU-USDT-SWAP"
fn okx_inst_id(symbol: &str) -> String {
    let s = symbol.trim().to_uppercase();
    if s.contains('-') {
        if s.ends_with("-SWAP") { s } else { format!("{}-SWAP", s) }
    } else if let Some(base) = s.strip_suffix("USDT") {
        format!("{}-USDT-SWAP", base)
    } else {
        s
    }
}