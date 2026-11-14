use anyhow::{Context, Result};
use rust_decimal::Decimal;
// P41/K6: ToPrimitive для Decimal::to_f64() в validate() (E0599)
use rust_decimal::prelude::ToPrimitive;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

// ✅ P40/K1: обёртка секрета — Debug/Display печатают `Secret(***)`,
// значение доступно только явно через expose(). Случайный вывод конфига
// в лог ({:?} в anyhow/tracing) больше не раскрывает api_key/api_secret.
// Deserialize — прозрачная загрузка строки из TOML; Serialize маскированный
// (конфиг нигде не сериализуется обратно — маска ничего не ломает).
#[derive(Clone, serde::Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(s: String) -> Self {
        Self(s)
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl std::fmt::Display for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl serde::Serialize for SecretString {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("Secret(***)")
    }
}

// ============================================================
// Global defaults
// ============================================================
fn default_symbol() -> String { "BTCUSDT".to_string() }
fn default_base_currency() -> String { "USDT".to_string() }
fn default_db_path() -> String { "data/state.db".to_string() }
fn default_sync_interval() -> u64 { 5 }
fn default_max_concurrent_positions() -> usize { 3 }
fn default_rest_url() -> String { "https://testnet.binance.vision".to_string() }
fn default_ws_url() -> String { "wss://stream.binance.com:9443".to_string() }
fn default_market_type() -> String { "spot".to_string() }
fn default_kline_interval() -> String { "1m".to_string() }
fn default_orderbook_density() -> Decimal { Decimal::from(5) }

// ============================================================
// Bot defaults
// ============================================================
fn default_initial_balance() -> Decimal { Decimal::from(1000) }
fn default_max_daily_loss_amount() -> Decimal { Decimal::from(30) }
fn default_circuit_breaker_window() -> u64 { 1800 }
fn default_circuit_breaker_cooldown() -> u64 { 600 }
// ✅ P41/D15: порог срабатывания CB и «значимый» лосс — из конфига.
fn default_circuit_breaker_threshold() -> u32 { 3 }
fn default_circuit_breaker_loss_usdt() -> Decimal { Decimal::new(10, 1) } // 0.10 USDT
fn default_order_timeout() -> u64 { 15 }
fn default_leverage() -> u32 { 1 }
fn default_risk_per_trade() -> Decimal { Decimal::new(5, 1) }
fn default_trailing_activation() -> Decimal { Decimal::new(15, 2) }
fn default_break_even_offset() -> Decimal { Decimal::new(5, 2) }
// [P9:mode-field] trailing regime: "atr" | "percent" | "tightest"
fn default_trailing_mode() -> String { "atr".to_string() }
fn default_max_position_duration() -> u64 { 60 }
fn default_simulate() -> bool { true }
fn default_params() -> HashMap<String, Decimal> { HashMap::new() }
fn default_maker_fee() -> Decimal { Decimal::new(2, 4) }     // 0.0002
fn default_min_ev_pct() -> Decimal { Decimal::ZERO }
fn default_min_net_rr() -> Decimal { Decimal::new(12, 1) }   // 1.2
fn default_max_concurrent_risk_pct() -> Decimal { Decimal::new(20, 1) } // 2.0% (P41/K6: единицы — ПРОЦЕНТЫ ОТ initial_balance бота, см. поле ниже)

// ============================================================
// GlobalConfig
// ============================================================
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
    pub exchange: String,

    // P42/K6: account-level capital for the portfolio 5% daily-budget check.
    // None -> fallback = SUM of bots initial_balance. Old code used the FIRST
    // bot balance as the base for ALL bots limits (50 vs 5% of 200 = 10).
    #[serde(default)]
    pub account_capital_usdt: Option<Decimal>,

    #[serde(default = "default_symbol")]
    pub symbol: String,

    #[serde(default = "default_base_currency")]
    pub base_currency: String,

    #[serde(default = "default_rest_url")]
    pub rest_url: String,

    #[serde(default = "default_ws_url")]
    pub ws_url: String,

    #[serde(default = "default_db_path")]
    pub db_path: String,

    #[serde(default = "default_sync_interval")]
    pub sync_interval_sec: u64,

    #[serde(default = "default_max_concurrent_positions")]
    pub max_concurrent_positions: usize,

    #[serde(default = "default_orderbook_density")]
    pub orderbook_density_threshold: Decimal,

    /// ✅ P37/K4: явный разрешающий флаг PROD-торговли. false (дефолт) =
    /// старт ЗАПРЕЩЁН, если хоть один REST URL бота не выглядит как
    /// testnet/demo. Раньше единственным барьером live была подстрока
    /// "testnet" в URL, рассыпанная по коду: одно неаккуратное имя хоста
    /// в конфиге — и бот молча торговал реальными деньгами.
    #[serde(default)]
    pub allow_live: bool,
}

// ============================================================
// BotConfig — с полями изоляции
// ============================================================
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct BotConfig {
    pub name: String,

    /// Символ бота. Если не задан — используется глобальный.
    #[serde(default)]
    pub symbol: Option<String>,

    /// ИЗОЛЯЦИЯ: если true — свой WS hub, свой REST, своя БД.
    #[serde(default)]
    pub isolated: bool,

    /// Тип рынка: "spot" или "futures"
    #[serde(default = "default_market_type")]
    pub market_type: String,

    /// ✅ 6: интервал свечей стратегии ("1m", "5m", "15m" ...).
    /// Для futures-ботов задаёт интервал REST-поллера свечей.
    #[serde(default = "default_kline_interval")]
    pub kline_interval: String,

    /// Свой REST URL (для futures)
    #[serde(default)]
    pub rest_url_override: Option<String>,

    /// Свой WS URL (для futures)
    #[serde(default)]
    pub ws_url_override: Option<String>,

    /// Своя БД (для изоляции состояния)
    #[serde(default)]
    pub db_path_override: Option<String>,

    #[serde(default)]
    pub subaccount_id: Option<String>,

    // ✅ P40/K1: ключи — SecretString (Debug → `Secret(***)`)
    pub api_key: SecretString,
    pub api_secret: SecretString,

    /// v4.0: OKX passphrase (required for OKX; absent for Binance/Bybit)
    #[serde(default)]
    pub passphrase: Option<SecretString>,

    #[serde(default = "default_initial_balance")]
    pub initial_balance: Decimal,

    #[serde(default = "default_sync_interval")]
    pub sync_interval_sec: u64,

    #[serde(default = "default_max_daily_loss_amount")]
    pub max_daily_loss_amount: Decimal,

    #[serde(default = "default_circuit_breaker_threshold")]
    pub circuit_breaker_threshold: u32,
    // ✅ P41/D15: лосс сделки, начиная с которого сделка двигает счётчик CB.
    #[serde(default = "default_circuit_breaker_loss_usdt")]
    pub circuit_breaker_loss_usdt: Decimal,

    #[serde(default = "default_circuit_breaker_window")]
    pub circuit_breaker_window_sec: u64,

    #[serde(default = "default_circuit_breaker_cooldown")]
    pub circuit_breaker_cooldown_sec: u64,

    #[serde(default = "default_order_timeout")]
    pub order_timeout_sec: u64,

    #[serde(default = "default_leverage")]
    pub leverage: u32,

    #[serde(default = "default_risk_per_trade", alias = "risk_per_trade")]
    pub risk_per_trade_pct: Decimal,

    #[serde(default = "default_trailing_activation")]
    pub trailing_stop_activation_percent: Decimal,

    #[serde(default = "default_break_even_offset")]
    pub break_even_offset_percent: Decimal,

    // [P9:mode-field] trailing regime switch
    #[serde(default = "default_trailing_mode")]
    pub trailing_mode: String,

    #[serde(default)]
    pub fee_rate: Option<Decimal>,

    #[serde(default = "default_maker_fee")]
    pub maker_fee: Decimal,

    #[serde(default = "default_min_ev_pct")]
    pub min_ev_pct: Decimal,

    #[serde(default = "default_min_net_rr")]
    pub min_net_rr: Decimal,

    #[serde(default = "default_max_concurrent_risk_pct")]
    pub max_concurrent_risk_pct: Decimal,

    #[serde(default = "default_max_position_duration")]
    pub max_position_duration_sec: u64,

    #[serde(default = "default_simulate")]
    pub simulate: bool,

    pub strategy: StrategyConfig,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct StrategyConfig {
    pub name: String,

    #[serde(default = "default_params")]
    pub params: HashMap<String, Decimal>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub global: GlobalConfig,

    #[serde(default)]
    pub bots: Vec<BotConfig>,
}

// ============================================================
// AppConfig impl
// ============================================================
impl AppConfig {
    pub fn load_from(path: &str) -> Result<Self> {
        if !Path::new(path).exists() {
            anyhow::bail!(
                "config.toml не найден по пути '{path}'. \
                 Создайте вручную (см. config.example.toml). \
                 Бот НЕ должен создавать конфиг автоматически — это небезопасно."
            );
        }
        let content = fs::read_to_string(path)?;
        let mut config: Self = toml::from_str(&content)?;

        // ✅ Подстановка переменных окружения в API-ключи — fail-loud:
        // если VAR не задана или пуста — bail с понятным сообщением.
        for bot in config.bots.iter_mut() {
            // ✅ P40/K1: env-подстановка в маскированные ключи (fail-loud как прежде)
            bot.api_key = SecretString::new(resolve_env_required(
                bot.api_key.expose(),
                &format!("bots.{}.api_key", bot.name),
            )?);
            bot.api_secret = SecretString::new(resolve_env_required(
                bot.api_secret.expose(),
                &format!("bots.{}.api_secret", bot.name),
            )?);
            if let Some(p) = bot.passphrase.clone() {
                bot.passphrase = Some(SecretString::new(resolve_env_required(
                    p.expose(),
                    &format!("bots.{}.passphrase", bot.name),
                )?));
            }
        }

        // [P9:mode-validation] fail-loud on unknown trailing_mode
        for bot in config.bots.iter() {
            match bot.trailing_mode.as_str() {
                "atr" | "percent" | "tightest" => {}
                other => anyhow::bail!(
                    "bots.{}: trailing_mode = \"{}\" - must be atr | percent | tightest",
                    bot.name, other
                ),
            }
        }

        // ✅ P37/K2: структурная валидация значений (fail-loud ДО старта
        // клиентов). Раньше принималось молча: leverage=0 (деление на ноль
        // в расчётах объёма), нулевой/отрицательный риск (бот не торговал
        // и не говорил почему), дубли db_path двух ботов (терли состояние
        // друг друга).
        config.validate()?;

        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        for bot in &self.bots {
            if bot.leverage < 1 || bot.leverage > 125 {
                anyhow::bail!(
                    "bots.{}: leverage={} вне диапазона 1..=125",
                    bot.name, bot.leverage
                );
            }
            let risk = bot.risk_per_trade_pct;
            if risk <= Decimal::ZERO || risk > Decimal::new(100, 1) {
                anyhow::bail!(
                    "bots.{}: risk_per_trade_pct={} должен быть в (0; 10]",
                    bot.name, risk
                );
            }
            if bot.max_daily_loss_amount <= Decimal::ZERO {
                anyhow::bail!(
                    "bots.{}: max_daily_loss_amount={} должен быть > 0",
                    bot.name, bot.max_daily_loss_amount
                );
            }
            if bot.kline_interval.trim().is_empty() {
                anyhow::bail!("bots.{}: kline_interval пуст", bot.name);
            }
        }
        // Дубликаты БД: два бота с одинаковым db_path_override пишут в одну
        // БД и молча стирают состояние друг друга (audit S7-родственник).
        let mut seen: std::collections::HashSet<String> = Default::default();
        for bot in &self.bots {
            if let Some(p) = &bot.db_path_override {
                if !seen.insert(p.clone()) {
                    anyhow::bail!(
                        "bots.{}: db_path_override '{}' задан у нескольких ботов",
                        bot.name, p
                    );
                }
            }
        }

        // ✅ P41/K6: согласованность портфеля — WARN (не блокирует старт,
        // это требования владельца, а не корректность конфига).
        let bots_n = self.bots.len();
        if bots_n > 1 && self.global.max_concurrent_positions < bots_n {
            tracing::warn!(
                "K6: max_concurrent_positions ({}) меньше числа ботов ({}): при пике                  одновременных позиций заявки части ботов будут отклонены глобальным лимитом",
                self.global.max_concurrent_positions, bots_n
            );
        }
        let daily_sum: f64 = self
            .bots
            .iter()
            .map(|b| b.max_daily_loss_amount.to_f64().unwrap_or(0.0))
            .sum();
                // P42/K6: base = ACCOUNT capital ([global].account_capital_usdt),
        // fallback: sum of bots initial_balance.
        let equity_base: f64 =
            match self.global.account_capital_usdt.and_then(|d| d.to_f64()) {
                Some(v) if v > 0.0 => v,
                _ => self
                    .bots
                    .iter()
                    .filter_map(|b| b.initial_balance.to_f64())
                    .sum(),
            };
        if equity_base > 0.0 && daily_sum > equity_base * 0.05 {
            tracing::warn!(
                "K6: суммарный дневной лимит {:.2} USDT превышает 5% от начального баланса                  {:.2} USDT ({:.2}) — сверяй max_daily_loss_amount ботов с аппетитом к риску",
                daily_sum, equity_base, equity_base * 0.05
            );
        }
        Ok(())
    }
}
  // ============================================================
 // Свободная функция — подстановка ${VAR} из окружения.
 // fail-loud: bail если VAR не задана или пуста.
 // ============================================================
 fn resolve_env_required(value: &str, field: &str) -> Result<String> {
    let v = value.trim();
    if v.starts_with("${") && v.ends_with('}') {
        let key = &v[2..v.len() - 1];
        let resolved = std::env::var(key).with_context(|| {
            format!(
                "Переменная окружения {key} не задана — заполните .env (поле {field}). \
                 Без неё бот не сможет подписать запросы к Binance."
            )
        })?;
        if resolved.trim().is_empty() {
            anyhow::bail!(
                "Переменная окружения {key} задана пустой — проверьте .env (поле {field})"
            );
        }
        // ✅ P38/K5: возвращаем ОЧИЩЕННОЕ значение — пробел/перевод строки
        // в .env не должен попадать в ключ (частая причина invalid API key).
        return Ok(resolved.trim().to_string());
    }
    if v.is_empty() {
        anyhow::bail!(
            "Поле {field} пусто и не использует ${{VAR}} — задайте значение в config.toml"
        );
    }
    // ✅ P38/K5: литерал тоже возвращаем очищенным (v уже trim()).
    Ok(v.to_string())
}