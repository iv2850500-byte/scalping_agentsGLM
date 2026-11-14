pub mod binance_futures;
pub mod binance_rest;
pub mod bybit_sign;          // 🆕 Phase B
pub mod bybit_futures;       // 🆕 Phase B
pub mod funding;
pub mod symbol_filters;
pub mod time_sync;
pub mod traits;
pub mod user_stream;
pub mod okx_sign;           // v4.0 OKX
pub mod okx_futures;         // v4.0 OKX
pub mod kline_poller;
pub mod resilient_client;

use crate::config::{BotConfig, GlobalConfig};
use anyhow::Result;
use std::sync::Arc;

pub use funding::FundingTracker;
pub use symbol_filters::FilterRegistry;
pub use time_sync::TimeSync;
pub use traits::ExchangeClient;

pub type SharedTimeSync = Arc<TimeSync>;
// ============================================================
// ✅ P25: типизированная биржа вместо строковых проверок URL
// ------------------------------------------------------------
// Личность биржи раньше определялась россыпью contains-проверок по URL
// и строковым match по global.exchange в 4 файлах (main.rs,
// websocket/mod.rs, exchange/mod.rs, funding.rs). Теперь один тип и
// одна точка определения. Поведение 1:1: приоритет URL (bybit, затем
// okx), затем global.exchange ("binance"/"bybit"/"okx"/"okx-demo");
// неизвестная строка -> None (create_client отвечает прежней ошибкой
// «Неизвестная биржа»), from_url для WS/REST-хабов по умолчанию
// Binance — как прежние else-ветки is_bybit=false/is_okx=false.
// ============================================================
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Exchange {
    Binance,
    Bybit,
    Okx,
}

impl Exchange {
    /// Каноническое нижнерегистровое имя (логи, метрики Prometheus).
    pub fn as_str(self) -> &'static str {
        match self {
            Exchange::Binance => "binance",
            Exchange::Bybit => "bybit",
            Exchange::Okx => "okx",
        }
    }

    /// Определение биржи по URL + строке global.exchange из конфига.
    /// None = неизвестная биржа (прежде — Err «Неизвестная биржа: …»).
    pub fn detect(url: &str, global_exchange: &str) -> Option<Exchange> {
        let u = url.to_lowercase();
        if u.contains("bybit") {
            return Some(Exchange::Bybit);
        }
        if u.contains("okx") {
            return Some(Exchange::Okx);
        }
        match global_exchange.trim().to_lowercase().as_str() {
            "binance" => Some(Exchange::Binance),
            "bybit" => Some(Exchange::Bybit),
            "okx" | "okx-demo" => Some(Exchange::Okx),
            _ => None,
        }
    }

    /// Определение только по URL (WS-хабы, поллеры, FundingTracker).
    /// Неизвестно -> Binance: ровно как прежние ветки
    /// is_bybit=false / is_okx=false / else-бины Binance.
    pub fn from_url(url: &str) -> Exchange {
        Self::detect(url, "").unwrap_or(Exchange::Binance)
    }
}

pub fn create_time_sync() -> SharedTimeSync {
    Arc::new(TimeSync::new())
}

pub fn create_client(
    global: &GlobalConfig,
    config: &BotConfig,
    time_sync: SharedTimeSync,
) -> Result<Arc<dyn ExchangeClient>> {
    // ✅ Приоритет: rest_url_override из BotConfig → глобальный
    let rest_url = config
        .rest_url_override
        .as_ref()
        .unwrap_or(&global.rest_url);

    let is_futures = config.market_type == "futures"
        || rest_url.contains("futures")
        || rest_url.contains("binancefuture")
        || rest_url.contains("bybit");

    tracing::info!(
        "[{}] Создаю клиент | market={} | rest_url={}",
        config.name,
        if is_futures { "FUTURES" } else { "SPOT" },
        rest_url
    );

    // ✅ P25: типизированная биржа вместо строк exchange_detected/match.
    // Ошибка «Неизвестная биржа» — с той же строкой, что прежде (global.exchange в нижнем регистре).
    let ex = Exchange::detect(rest_url, &global.exchange)
        .ok_or_else(|| anyhow::anyhow!("Неизвестная биржа: {}", global.exchange.to_lowercase()))?;
    // v4.0: demo/prod OKX решается строкой global.exchange ("okx-demo" vs "okx")
    let okx_demo = global.exchange.to_lowercase() == "okx-demo";

    match ex {
        // === v4.0: OKX V5 (demo when global.exchange = "okx-demo") ===
        Exchange::Okx => {
            // ✅ P40/K1: ключи из конфига — только через expose()
            let passphrase = config
                .passphrase
                .as_ref()
                .map(|s| s.expose())
                .ok_or_else(|| {
                    anyhow::anyhow!("[{}] OKX: passphrase not set (bots.{}.passphrase)", config.name, config.name)
                })?;
            tracing::info!("[{}] OKX client | demo={} | rest_url={}", config.name, okx_demo, rest_url);
            Ok(Arc::new(okx_futures::OkxFuturesClient::new(
                config.api_key.expose(),
                config.api_secret.expose(),
                passphrase,
                rest_url,
                okx_demo,
                time_sync,
            )) as Arc<dyn ExchangeClient>)
        }

        // === Binance ===
        Exchange::Binance => {
            if is_futures {
                // ✅ P40/K1: ключи — expose()
                Ok(Arc::new(binance_futures::BinanceFuturesClient::new(
                    config.api_key.expose(),
                    config.api_secret.expose(),
                    rest_url,
                    time_sync,
                )) as Arc<dyn ExchangeClient>)
            } else {
                // ✅ P40/K1: ключи — expose()
                Ok(Arc::new(binance_rest::BinanceRestClient::new(
                    config.api_key.expose(),
                    config.api_secret.expose(),
                    rest_url,
                    time_sync,
                )) as Arc<dyn ExchangeClient>)
            }
        }

        // === 🆕 Phase B: Bybit V5 ===
        Exchange::Bybit => {
            // Bybit не разделяет spot/futures в API структуре как Binance —
            // для USDT perp (XAUUSDT, BTCUSDT) используем BybitFuturesClient.
            // Spot редкий случай — пока не поддерживается (bybit_spot.rs можно добавить позже).
            if is_futures {
                // ✅ P40/K1: ключи — expose()
                Ok(Arc::new(bybit_futures::BybitFuturesClient::new(
                    config.api_key.expose(),
                    config.api_secret.expose(),
                    rest_url,
                    time_sync,
                )) as Arc<dyn ExchangeClient>)
            } else {
                // Если в config market_type != "futures" но exchange=bybit — пока fallback на futures
                tracing::warn!(
                    "[{}] Bybit SPOT пока не реализован — использую Futures клиент (для perp символов это OK)",
                    config.name
                );
                // ✅ P40/K1: ключи — expose()
                Ok(Arc::new(bybit_futures::BybitFuturesClient::new(
                    config.api_key.expose(),
                    config.api_secret.expose(),
                    rest_url,
                    time_sync,
                )) as Arc<dyn ExchangeClient>)
            }
        }
    }
}