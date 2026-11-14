use anyhow::Result;
use reqwest::Client;
use serde_json::Value;

use super::Exchange; // ✅ P25: типизированная биржа вместо contains-проверки URL

pub struct FundingTracker {
    client: Client,
    base_url: String,
    /// ✅ P25: биржа определяется один раз в new() по base_url
    exchange: Exchange,
}

#[derive(Debug, Clone)]
pub struct FundingRate {
    pub symbol: String,
    pub rate: f64,              // текущая ставка (например, 0.0001 = 0.01%)
    pub next_funding_time: i64, // ms UTC
}

impl FundingTracker {
    pub fn new(base_url: &str) -> Self {
        Self {
            // ✅ P41/D13: единственный клиент без таймаута (аудит) — висячий
            // запрос funding блокировал вызывающую задачу. 10с как у остальных.
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("funding reqwest client"),
            base_url: base_url.trim_end_matches('/').to_string(),
            exchange: Exchange::from_url(base_url),
        }
    }

    /// Текущая ставка funding для символа
    pub async fn get_rate(&self, symbol: &str) -> Result<FundingRate> {
        // v4.0: OKX V5 /api/v5/public/funding-rate (base_url points to OKX)
        if self.exchange == Exchange::Okx { // ✅ P25: было contains-проверкой base_url
            let s = symbol.trim().to_uppercase();
            let inst = if s.contains('-') {
                if s.ends_with("-SWAP") { s } else { format!("{}-SWAP", s) }
            } else if let Some(base) = s.strip_suffix("USDT") {
                format!("{}-USDT-SWAP", base)
            } else {
                s
            };
            let url = format!(
                "{}/api/v5/public/funding-rate?instId={}",
                self.base_url, inst
            );
            let resp: Value = self.client.get(&url).send().await?.json().await?;
            if resp["code"].as_str().unwrap_or("-1") != "0" {
                anyhow::bail!(
                    "OKX funding-rate {}: code={} msg={}",
                    inst,
                    resp["code"].as_str().unwrap_or("?"),
                    resp["msg"].as_str().unwrap_or("?")
                );
            }
            let d = resp["data"]
                .as_array()
                .and_then(|a| a.first())
                .ok_or_else(|| anyhow::anyhow!("OKX funding-rate {}: empty data", inst))?;
            return Ok(FundingRate {
                symbol: symbol.to_string(),
                rate: d["fundingRate"].as_str().and_then(|x| x.parse().ok()).unwrap_or(0.0),
                next_funding_time: d["fundingTime"].as_str().and_then(|x| x.parse().ok()).unwrap_or(0),
            });
        }

        let url = format!(
            "{}/fapi/v1/premiumIndex?symbol={}",
            self.base_url, symbol
        );
        let resp: Value = self.client.get(&url).send().await?.json().await?;

        Ok(FundingRate {
            symbol: symbol.to_string(),
            rate: resp["lastFundingRate"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0),
            next_funding_time: resp["nextFundingTime"]
                .as_i64()
                .unwrap_or(0),
        })
    }

    /// Ожидаемая стоимость funding за N часов удержания позиции
    /// Funding начисляется каждые 8 часов
    pub fn expected_cost(&self, rate: &FundingRate, position_notional: f64, hours: f64) -> f64 {
        let periods = (hours / 8.0).floor();
        -rate.rate * position_notional * periods
    }
}