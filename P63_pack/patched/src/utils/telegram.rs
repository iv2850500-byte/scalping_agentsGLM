//! [P11:tg] Telegram alerts.
//! [P62/1] HUB v2: единый фоновой отправитель вместо HTTP-запроса на каждый вызов.
//! Раньше notify() строил НОВЫЙ reqwest::Client на каждое сообщение, не имел
//! rate-limit (Telegram: ~1 msg/s на чат, 429 при burst) и мог параллельно
//! выстрелить десятком запросов из P53-смотрителя. Теперь:
//!   - один общий клиент и один воркер (Lazy-инициализация при первом notify);
//!   - очередь на 128 сообщений, переполнение = drop со счётчиком (не блокирует торговлю);
//!   - rate-limit 1.15 с между отправками, ретрай, 429 = пауза по retry_after;
//!   - коалесинг повторов по ключу (60 с): C4-восстановления и crash-loop не штормят;
//!   - notify_once_spawn: ровно один алерт на ключ (пороги day-loss, ранние P15);
//!   - P41/D13-урок: Display ошибки reqwest содержит ПОЛНЫЙ URL с токеном —
//!     наружу идёт только фиксированный тег, URL не покидает send().
//!
//! Закрытия сделок теперь приходят МГНОВЕННО из trade_journal::record_close
//! (единая воронка всех 11 сайтов закрытия), а не поллером через 30 с —
//! из poller'а секция journal-CLOSE убрана (иначе были бы дубли).
//!
//! [P63/1] Транспорт под блокировку Telegram в РФ (публичный API не изменился):
//!   - TG_PROXY (http:// | socks5:// | socks5h://) — трафик через локальный
//!     прокси/туннель (v2rayN: 10808 socks5 / 10809 http, Clash: 7890 mixed).
//!     Сам URL в логи не пишется (может содержать пароль) — только схема;
//!   - TG_ENABLED=false — полный kill-switch алертов без удаления токена из .env;
//!   - ретрай больше не фиксированный: 3с -> 15с, максимум 3 попытки на сообщение;
//!   - circuit breaker: 3 неудачные попытки ПОДРЯД -> пауза 10 мин (далее 20/40/60,
//!     кап 60 мин). В паузе в сеть не бьёмся: алерты считаются и после
//!     восстановления уходят ОДНОЙ сводкой ("пропущено алертов: N") — конец
//!     WARN-шторму каждые 3 с при заблокированном канале;
//!   - 401/403/404 = неверный токен/chat_id — ретраить бессмысленно: алерты
//!     гасятся до рестарта (AUTH_DEAD), одна строка в лог.
use crate::strategies::Signal;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::json;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

const MIN_SEND_INTERVAL: Duration = Duration::from_millis(1150);
const COALESCE_WINDOW: Duration = Duration::from_secs(60);
/// Мягкий лимит текста (Telegram hard-cap 4096 на сообщение).
const TEXT_CAP: usize = 3500;
const QUEUE_CAP: usize = 128;

// ============================================================
// [P63/2] Параметры ретрая и предохранителя
// ============================================================

/// Пауза перед 2-й попыткой (после 1-й неудачи).
const RETRY_DELAY_1: Duration = Duration::from_secs(3);
/// Пауза перед 3-й попыткой.
const RETRY_DELAY_2: Duration = Duration::from_secs(15);
/// Максимум попыток на одно сообщение (вне HALF-OPEN пробы).
const MAX_ATTEMPTS: u32 = 3;
/// Неудачных попыток подряд до открытия предохранителя.
const BREAKER_THRESHOLD: u32 = 3;
/// Первая пауза предохранителя; удваивается при каждой следующей неудаче пробы.
const BREAKER_BASE_COOLDOWN: Duration = Duration::from_secs(600);
/// Кап паузы предохранителя (60 мин).
const BREAKER_MAX_COOLDOWN: Duration = Duration::from_secs(3600);

/// Чистая математика ретрая: пауза после неудачной попытки N (N >= 1).
fn retry_delay(attempt: u32) -> Duration {
    match attempt {
        1 => RETRY_DELAY_1,
        _ => RETRY_DELAY_2,
    }
}

static CREDS: Lazy<Option<(String, String)>> = Lazy::new(creds_from_env);

fn creds_from_env() -> Option<(String, String)> {
    let t = std::env::var("TG_BOT_TOKEN").unwrap_or_default();
    let c = std::env::var("TG_CHAT_ID").unwrap_or_default();
    if t.is_empty() || c.is_empty() {
        return None;
    }
    Some((t, c))
}

fn creds() -> Option<(String, String)> {
    CREDS.clone()
}

// ============================================================
// [P63/3] Разбор env (чистые функции — покрываются тестами)
// ============================================================

/// TG_ENABLED: отсутствие = вкл; "0|false|off|no" (в любом регистре) = выкл.
fn parse_enabled(v: Option<&str>) -> bool {
    match v.map(|s| s.trim().to_ascii_lowercase()) {
        None => true,
        Some(s) => !matches!(s.as_str(), "0" | "false" | "off" | "no"),
    }
}

/// TG_PROXY: None/пусто/пробелы/"none|off|direct" = напрямую; иначе URL как есть.
fn parse_proxy(v: Option<&str>) -> Option<String> {
    let s = v?.trim().to_string();
    if s.is_empty() {
        return None;
    }
    let low = s.to_ascii_lowercase();
    if matches!(low.as_str(), "none" | "off" | "direct") {
        return None;
    }
    Some(s)
}

/// Для лога: только схема прокси. Полный URL наружу не идёт (пароль в нём).
fn proxy_scheme(url: &str) -> String {
    url.split("://")
        .next()
        .unwrap_or("?")
        .to_ascii_lowercase()
}

// ============================================================
// [P63/4] Статические флаги транспорта
// ============================================================

/// Kill-switch из .env: TG_ENABLED=false гасит все алерты.
static ENABLED: Lazy<bool> =
    Lazy::new(|| parse_enabled(std::env::var("TG_ENABLED").ok().as_deref()));
/// Прокси из .env: TG_PROXY=socks5://127.0.0.1:10808 (или http://...).
static PROXY: Lazy<Option<String>> =
    Lazy::new(|| parse_proxy(std::env::var("TG_PROXY").ok().as_deref()));
/// 401/403/404: токен/chat_id неверны — ретраить бессмысленно, гасим до рестарта.
static AUTH_DEAD: AtomicBool = AtomicBool::new(false);
/// Алерты, сброшенные во время открытого предохранителя (уйдут сводкой).
static OUTAGE_DROPPED: AtomicU64 = AtomicU64::new(0);

static DROPPED: AtomicU64 = AtomicU64::new(0);

static HUB: Lazy<tokio::sync::mpsc::Sender<Msg>> = Lazy::new(|| {
    let (tx, rx) = tokio::sync::mpsc::channel::<Msg>(QUEUE_CAP);
    tokio::spawn(worker(rx));
    tx
});

// ============================================================
// Публичный API (переносимость с P11/P62: сигнатуры не менялись)
// ============================================================

/// Fire-and-forget текстовое сообщение (никогда не паникует, не блокирует).
pub async fn notify(event: &str, detail: &str) {
    notify_inner(None, event, detail).await;
}

/// То же, но с ключом коалесинга: повтор в течение 60 с подавляется.
pub async fn notify_key(key: &str, event: &str, detail: &str) {
    notify_inner(Some(key.to_string()), event, detail).await;
}

/// Для СИНХРОННЫХ контекстов (risk record_close, супервизор): уходит фоновой
/// задачей, вне tokio-рантайма молча теряется (тот же приём, что P54/HALT-2).
pub fn notify_spawn(event: &str, detail: &str) {
    // Строки переезжают в таск ('static) — ссылки туда жить не могут.
    let event = event.to_string();
    let detail = detail.to_string();
    spawn_opt(async move { notify(&event, &detail).await });
}

pub fn notify_key_spawn(key: &str, event: &str, detail: &str) {
    let key = key.to_string();
    let event = event.to_string();
    let detail = detail.to_string();
    spawn_opt(async move { notify_key(&key, &event, &detail).await });
}

/// РОВНО ОДИН алерт на ключ за жизнь процесса (пороги day-loss, P15 50/80%).
/// Ключ резервируется СИНХРОННО до спавна — гонок двойной отправки нет.
static ONCE_SENT: Lazy<Mutex<HashSet<String>>> = Lazy::new(|| Mutex::new(HashSet::new()));

pub fn notify_once_spawn(key: &str, event: &str, detail: &str) {
    if !once_reserve(&mut ONCE_SENT.lock(), key) {
        return;
    }
    notify_spawn(event, detail);
}

/// Чистая функция резерва ключа (для тестов): true = первый раз.
fn once_reserve(set: &mut HashSet<String>, key: &str) -> bool {
    set.insert(key.to_string())
}

fn spawn_opt(fut: impl std::future::Future<Output = ()> + Send + 'static) {
    match tokio::runtime::Handle::try_current() {
        Ok(h) => {
            h.spawn(fut);
        }
        Err(_) => {
            // вне рантайма отправить некому — молча (алерты не должны ронять торговлю)
        }
    }
}

async fn notify_inner(key: Option<String>, event: &str, detail: &str) {
    // [P63/5] гейты до входа в очередь: выключено/мёртвый токен — не занимаем слоты
    if CREDS.is_none() || !*ENABLED || AUTH_DEAD.load(Ordering::Relaxed) {
        return;
    }
    let text = format!("[scalping_agents] {}\n{}", event, truncate(detail));
    match HUB.try_send(Msg { key, text }) {
        Ok(()) => {}
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            let n = DROPPED.fetch_add(1, Ordering::Relaxed);
            if n.rem_euclid(10) == 0 {
                tracing::warn!("Telegram: очередь полна — всего сброшено {} алертов", n + 1);
            }
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            tracing::warn!("Telegram: очередь закрыта — алерт потерян");
        }
    }
}

// ============================================================
// Воркер: коалесинг + rate-limit + ретрай + предохранитель
// ============================================================

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Send,
    Suppress,
}

/// Чистая логика коалесинга: Send если ключа нет/окно истекло, иначе Suppress.
/// Возвращаемое число — сколько уже подавлено ДО этого вызова (для сводки).
fn coalesce_decision(
    last: Option<(std::time::Instant, u64)>,
    now: std::time::Instant,
    window: Duration,
) -> (Decision, u64) {
    match last {
        None => (Decision::Send, 0),
        Some((t, n)) => {
            if now.duration_since(t) <= window {
                (Decision::Suppress, n)
            } else {
                (Decision::Send, n)
            }
        }
    }
}

/// Чистая математика rate-limit: задержка до ближайшего слота отправки.
/// Слот уже назначен с учётом MIN_SEND_INTERVAL — здесь только остаток времени.
fn next_send_delay(slot: Option<std::time::Instant>, now: std::time::Instant) -> Option<Duration> {
    let s = slot?;
    if now >= s {
        None
    } else {
        Some(s.duration_since(now))
    }
}

fn truncate(detail: &str) -> String {
    if detail.chars().count() <= TEXT_CAP {
        detail.to_string()
    } else {
        let cut: String = detail.chars().take(TEXT_CAP).collect();
        format!("{}…", cut)
    }
}

// ============================================================
// [P63/6] Предохранитель канала: CLOSED -> OPEN(10м) -> HALF-OPEN проба
// Неудача пробы -> OPEN с удвоенной паузой (20/40/60, кап 60 мин).
// Успех пробы -> CLOSED (пауза и счётчик неудач сбрасываются).
// ============================================================

struct Breaker {
    open_until: Option<std::time::Instant>,
    fail_streak: u32,
    cooldown: Duration,
}

impl Breaker {
    fn new() -> Self {
        Self {
            open_until: None,
            fail_streak: 0,
            cooldown: BREAKER_BASE_COOLDOWN,
        }
    }

    fn allow_send(&self, now: std::time::Instant) -> bool {
        match self.open_until {
            None => true,
            Some(t) => now >= t,
        }
    }

    /// OPEN-окно истекло: следующее сообщение = HALF-OPEN проба (одна попытка).
    fn ready_for_probe(&self, now: std::time::Instant) -> bool {
        match self.open_until {
            Some(t) => now >= t,
            None => false,
        }
    }

    fn record_success(&mut self) {
        self.open_until = None;
        self.fail_streak = 0;
        self.cooldown = BREAKER_BASE_COOLDOWN;
    }

    /// Если предохранитель открылся/переоткрылся — возвращает новую паузу (для лога).
    fn record_failure(&mut self, now: std::time::Instant) -> Option<Duration> {
        self.fail_streak = self.fail_streak.saturating_add(1);
        let should_open =
            self.fail_streak >= BREAKER_THRESHOLD || self.open_until.is_some();
        if should_open {
            let cd = self.cooldown;
            self.open_until = Some(now + cd);
            self.cooldown = self
                .cooldown
                .checked_mul(2)
                .unwrap_or(BREAKER_MAX_COOLDOWN)
                .min(BREAKER_MAX_COOLDOWN);
            self.fail_streak = BREAKER_THRESHOLD;
            Some(cd)
        } else {
            None
        }
    }
}

async fn worker(mut rx: tokio::sync::mpsc::Receiver<Msg>) {
    let (token, chat) = match creds() {
        Some(x) => x,
        None => {
            tracing::info!("Telegram: нет TG_BOT_TOKEN/TG_CHAT_ID — воркер не запущен");
            return;
        }
    };
    if !*ENABLED {
        tracing::info!("Telegram: TG_ENABLED=false — отправитель не запущен");
        return;
    }
    // [P63/7] клиент строится ОДИН раз; прокси прикладывается здесь же
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .pool_idle_timeout(Duration::from_secs(20))
        .tcp_keepalive(Duration::from_secs(15));
    if let Some(p) = PROXY.as_ref() {
        match reqwest::Proxy::all(p.as_str()) {
            Ok(px) => {
                builder = builder.proxy(px);
                tracing::info!("Telegram: транспорт через прокси (scheme={})", proxy_scheme(p));
            }
            Err(_) => {
                tracing::warn!(
                    "Telegram: TG_PROXY не поддержан (scheme={}; нужен http|socks5|socks5h) — работаем напрямую",
                    proxy_scheme(p)
                );
            }
        }
    }
    let client = builder.build().unwrap_or_default();
    let mut next_slot: Option<std::time::Instant> = None;
    let mut last_sent: HashMap<String, (std::time::Instant, u64)> = HashMap::new();
    let mut breaker = Breaker::new();
    while let Some(msg) = rx.recv().await {
        // auth-dead: очередь сливается молча, в сеть не бьёмся
        if AUTH_DEAD.load(Ordering::Relaxed) {
            continue;
        }
        let mut text = msg.text;
        if let Some(k) = &msg.key {
            let now = std::time::Instant::now();
            let (decision, suppressed) =
                coalesce_decision(last_sent.get(k).copied(), now, COALESCE_WINDOW);
            match decision {
                Decision::Suppress => {
                    if let Some(e) = last_sent.get_mut(k) {
                        e.1 += 1;
                    }
                    continue;
                }
                Decision::Send => {
                    last_sent.insert(k.clone(), (now, 0));
                    if suppressed > 0 {
                        text = format!(
                            "{}\n(+{} похожих подавлено за последние {} с)",
                            text,
                            suppressed,
                            COALESCE_WINDOW.as_secs()
                        );
                    }
                }
            }
        }
        // [P63/8] gate предохранителя: открыты — тихо считаем пропуски
        let gate_now = std::time::Instant::now();
        if !breaker.allow_send(gate_now) {
            OUTAGE_DROPPED.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        // HALF-OPEN: окно истекло — одна пробная попытка без ретраев
        let probe = breaker.ready_for_probe(gate_now);
        let attempts = if probe { 1 } else { MAX_ATTEMPTS };
        let mut ok = false;
        for attempt in 1..=attempts {
            if let Some(delay) = next_send_delay(next_slot, std::time::Instant::now()) {
                tokio::time::sleep(delay).await;
            }
            match send(&client, &token, &chat, &text).await {
                Ok(()) => {
                    ok = true;
                    break;
                }
                Err(e) => {
                    if e.is_permanent() {
                        // [P63/9] 401/403/404: ретраить бессмысленно
                        AUTH_DEAD.store(true, Ordering::Relaxed);
                        tracing::error!(
                            "Telegram: ответ {} (401/403/404) — токен или chat_id неверны, алерты отключены до рестарта",
                            e.tag()
                        );
                        break;
                    }
                    if let Some(cd) = breaker.record_failure(std::time::Instant::now()) {
                        tracing::warn!(
                            "Telegram: сеть недоступна ({}) — предохранитель открыт на {} мин; алерты копятся и уйдут сводкой после восстановления",
                            e.tag(),
                            cd.as_secs() / 60
                        );
                    } else {
                        tracing::warn!("Telegram: отправка не удалась ({})", e.tag());
                    }
                    if attempt < attempts {
                        tokio::time::sleep(retry_delay(attempt)).await;
                    }
                }
            }
        }
        next_slot = Some(std::time::Instant::now() + MIN_SEND_INTERVAL);
        // [P63/10] восстановление: сброс предохранителя + разовая сводка пропусков
        if ok {
            breaker.record_success();
            if probe {
                let dropped = OUTAGE_DROPPED.swap(0, Ordering::Relaxed);
                tracing::info!(
                    "Telegram: связь восстановлена; за время недоступности пропущено алертов: {}",
                    dropped
                );
                if dropped > 0 {
                    let digest = format!(
                        "[scalping_agents] Telegram: связь восстановлена.\nПока канал был недоступен, пропущено алертов: {} (детали входов/выходов — в bot.log).",
                        dropped
                    );
                    let _ = send(&client, &token, &chat, &digest).await;
                }
            }
        }
    }
}

// ============================================================
// [P63/11] HTTP-слой: ошибки классифицированы, теги фиксированные
// (Display ошибки reqwest содержит URL с токеном — наружу не идёт)
// ============================================================

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum SendErr {
    /// Таймаут/соединение/прокси — ретраится, питает предохранитель.
    Network,
    /// 429: retry_after уже отработан внутри send.
    RateLimited,
    /// 401/403/404: неверный токен или chat_id — ретраить бессмысленно.
    Auth,
    /// Прочие HTTP-коды.
    Http,
}

impl SendErr {
    fn tag(self) -> &'static str {
        match self {
            SendErr::Network => "network",
            SendErr::RateLimited => "rate_limited",
            SendErr::Auth => "auth",
            SendErr::Http => "http_error",
        }
    }

    fn is_permanent(self) -> bool {
        matches!(self, SendErr::Auth)
    }
}

async fn send(client: &reqwest::Client, token: &str, chat: &str, text: &str) -> Result<(), SendErr> {
    // ✅ P41/D13-урок: Display ошибки reqwest содержит ПОЛНЫЙ URL, а URL
    // содержит токен бота. Наружу — только фиксированный тег. URL не логируется
    // нигде и не покидает пределов этой функции.
    let url = format!("https://api.telegram.org/bot{}/sendMessage", token);
    let body = json!({
        "chat_id": chat,
        "text": text,
        "disable_web_page_preview": true,
    });
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|_| SendErr::Network)?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    match status.as_u16() {
        401 | 403 | 404 => return Err(SendErr::Auth),
        429 => {
            // Уважаем retry_after Telegram (сек), ограничиваем сверху 15 с.
            let wait = resp
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| v["parameters"]["retry_after"].as_u64())
                .unwrap_or(3)
                .clamp(2, 15);
            tokio::time::sleep(Duration::from_secs(wait)).await;
            return Err(SendErr::RateLimited);
        }
        _ => {}
    }
    Err(SendErr::Http)
}

// ============================================================
// Форматтеры событий (чистые функции — покрываются тестами)
// ============================================================

pub fn fmt_close(bot: &str, sig: &Signal, pnl: f64) -> String {
    format!(
        "{} | {} {} {} | qty {:.5} @ {:.6} | {} | PnL {:+.2} USDT",
        bot,
        sig.strategy_name,
        sig.side,
        sig.symbol,
        sig.quantity,
        sig.price,
        if pnl > 0.0 { "WIN" } else if pnl < 0.0 { "LOSS" } else { "FLAT" },
        pnl
    )
}

pub fn fmt_entry(bot: &str, sig: &Signal, price: f64, qty: f64) -> String {
    format!(
        "{} | {} {} | qty {:.5} @ {:.6} | SL {} | TP {} | {}",
        bot,
        sig.side,
        sig.symbol,
        qty,
        price,
        sig.stop_loss.map(|v| format!("{:.6}", v)).unwrap_or_else(|| "-".to_string()),
        sig.take_profit.map(|v| format!("{:.6}", v)).unwrap_or_else(|| "-".to_string()),
        sig.strategy_name
    )
}

/// Порог дневного убытка в процентах лимита: 50 / 80 / 100 (наибольший
/// достигнутый). None — лимит не достигнут или входы невалидны.
pub fn day_loss_level(daily_loss: f64, max_loss: f64) -> Option<u32> {
    if !max_loss.is_finite() || !daily_loss.is_finite() || max_loss <= 0.0 || daily_loss <= 0.0 {
        return None;
    }
    let ratio = daily_loss / max_loss;
    if ratio >= 1.0 {
        Some(100)
    } else if ratio >= 0.8 {
        Some(80)
    } else if ratio >= 0.5 {
        Some(50)
    } else {
        None
    }
}

// ============================================================
// Poller v2: suspensions (P11) + KILL-переходы. Секция journal-CLOSE
// УДАЛЕНА (P62): закрытия приходят мгновенно из record_close, поллер бы
// дублировал их через 30 с.
// ============================================================
pub fn spawn_poller(bots: Vec<(String, String)>, _journal_db: String) {
    tokio::spawn(async move {
        if CREDS.is_none() {
            tracing::info!("Telegram: TG_BOT_TOKEN/TG_CHAT_ID не заданы — алерты выключены");
            return;
        }
        if !*ENABLED {
            tracing::info!("Telegram: TG_ENABLED=false — алерты и поллер выключены");
            return;
        }
        // [P63/12] статус транспорта в лог: схема прокси, креденшелов нет
        match PROXY.as_ref() {
            Some(p) => tracing::info!(
                "Telegram: алерты ВКЛ (P63: proxy scheme={}, breaker on, полл 30 с)",
                proxy_scheme(p)
            ),
            None => tracing::info!("Telegram: алерты ВКЛ (P63: direct, breaker on, полл 30 с)"),
        }
        let mut pools: HashMap<String, SqlitePool> = HashMap::new();
        let mut last_susp: HashMap<String, bool> = HashMap::new();
        let mut last_kill: Option<bool> = None;
        loop {
            // 1) [P62/1] переходы KILL-файла (семантика fail-closed — C8)
            let kill = crate::utils::killfile::kill_switch_active();
            // match-guard вместо let-chain: кодовая база не использует
            // let-chains (нужен rustc >= 1.88), не рискуем совместимостью.
            match last_kill {
                Some(prev) if kill != prev => {
                    if kill {
                        notify(
                            "KILL file ACTIVATED",
                            "новые входы запрещены; удалить файл KILL в рабочей папке бота для возобновления",
                        )
                        .await;
                    } else {
                        notify("KILL file REMOVED", "входы снова разрешены").await;
                    }
                }
                _ => {}
            }
            last_kill = Some(kill);
            // 2) переходы suspension по ботам (как в P11)
            for (name, db) in &bots {
                let Some(pool) = pool_for(&mut pools, db, false).await else {
                    continue;
                };
                if let Ok(Some(row)) = sqlx::query(
                    "SELECT suspended, suspend_reason FROM risk_stats WHERE bot_name = ?",
                )
                .bind(name)
                .fetch_optional(&pool)
                .await
                {
                    let susp: i64 = row.try_get("suspended").unwrap_or(0);
                    let now = susp != 0;
                    let prev = last_susp.get(name).copied().unwrap_or(false);
                    if now && !prev {
                        let reason: String = row.try_get("suspend_reason").unwrap_or_default();
                        let why = if reason.is_empty() {
                            "reason n/a"
                        } else {
                            reason.as_str()
                        };
                        notify("Bot SUSPENDED", &format!("{}: {}", name, why)).await;
                    }
                    last_susp.insert(name.clone(), now);
                }
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}

async fn pool_for(
    pools: &mut HashMap<String, SqlitePool>,
    db: &str,
    create: bool,
) -> Option<SqlitePool> {
    if let Some(p) = pools.get(db) {
        return Some(p.clone());
    }
    let mode = if create { "rwc" } else { "rw" };
    let url = format!("sqlite:{}?mode={}", db, mode);
    match SqlitePool::connect(&url).await {
        Ok(p) => {
            pools.insert(db.to_string(), p.clone());
            Some(p)
        }
        Err(_) => None,
    }
}

// ============================================================
// [P63/13] Тесты чистой логики P63 (transport hardening)
// ============================================================
#[cfg(test)]
mod p63_tests {
    use super::*;

    #[test]
    fn t63_retry_delay_math() {
        assert_eq!(retry_delay(1), Duration::from_secs(3));
        assert_eq!(retry_delay(2), Duration::from_secs(15));
        // попыток больше 3 не бывает, но функция безопасна на любом входе
        assert_eq!(retry_delay(5), Duration::from_secs(15));
    }

    #[test]
    fn t63_breaker_opens_on_threshold() {
        let t0 = std::time::Instant::now();
        let mut b = Breaker::new();
        assert!(b.allow_send(t0));
        assert_eq!(b.record_failure(t0), None);
        assert_eq!(b.record_failure(t0), None);
        // третья неудача подряд -> открытие на базовую паузу 10 мин
        assert_eq!(b.record_failure(t0), Some(Duration::from_secs(600)));
        assert!(!b.allow_send(t0));
        assert!(!b.allow_send(t0 + Duration::from_secs(599)));
    }

    #[test]
    fn t63_breaker_probe_success_recovers_to_base() {
        let t0 = std::time::Instant::now();
        let mut b = Breaker::new();
        for _ in 0..3 {
            b.record_failure(t0);
        }
        // окно истекло: HALF-OPEN проба разрешена
        let t1 = t0 + Duration::from_secs(601);
        assert!(b.ready_for_probe(t1));
        assert!(b.allow_send(t1));
        // успех пробы: CLOSED, счётчики сброшены, пауза снова базовая
        b.record_success();
        assert!(b.allow_send(t1 + Duration::from_secs(1)));
        assert_eq!(b.record_failure(t1), None);
        assert_eq!(b.record_failure(t1), None);
        assert_eq!(b.record_failure(t1), Some(Duration::from_secs(600)));
    }

    #[test]
    fn t63_breaker_probe_fail_doubles_cooldown_capped() {
        let t0 = std::time::Instant::now();
        let mut b = Breaker::new();
        for _ in 0..3 {
            b.record_failure(t0);
        }
        // 1-я неудача пробы: 10 -> 20 мин
        let t1 = t0 + Duration::from_secs(601);
        assert_eq!(b.record_failure(t1), Some(Duration::from_secs(1200)));
        // 2-я: 20 -> 40 мин
        let t2 = t1 + Duration::from_secs(1201);
        assert_eq!(b.record_failure(t2), Some(Duration::from_secs(2400)));
        // 3-я: 40 -> 60 мин (кап)
        let t3 = t2 + Duration::from_secs(2401);
        assert_eq!(b.record_failure(t3), Some(Duration::from_secs(3600)));
        // дальше кап держится
        let t4 = t3 + Duration::from_secs(3601);
        assert_eq!(b.record_failure(t4), Some(Duration::from_secs(3600)));
        assert!(!b.allow_send(t4));
    }

    #[test]
    fn t63_enabled_parse() {
        assert!(parse_enabled(None));
        assert!(parse_enabled(Some("true")));
        assert!(parse_enabled(Some("1")));
        assert!(parse_enabled(Some("yes")));
        assert!(parse_enabled(Some("")));
        assert!(!parse_enabled(Some("false")));
        assert!(!parse_enabled(Some("0")));
        assert!(!parse_enabled(Some("off")));
        assert!(!parse_enabled(Some(" FALSE ")));
        assert!(!parse_enabled(Some("No")));
    }

    #[test]
    fn t63_proxy_parse() {
        assert_eq!(parse_proxy(None), None);
        assert_eq!(parse_proxy(Some("")), None);
        assert_eq!(parse_proxy(Some("   ")), None);
        assert_eq!(parse_proxy(Some("none")), None);
        assert_eq!(parse_proxy(Some("direct")), None);
        assert_eq!(parse_proxy(Some("OFF")), None);
        assert_eq!(
            parse_proxy(Some("  http://127.0.0.1:7890 ")),
            Some("http://127.0.0.1:7890".to_string())
        );
        assert_eq!(
            parse_proxy(Some("socks5://u:p@127.0.0.1:10808")),
            Some("socks5://u:p@127.0.0.1:10808".to_string())
        );
        assert_eq!(
            parse_proxy(Some("socks5h://127.0.0.1:1080")),
            Some("socks5h://127.0.0.1:1080".to_string())
        );
    }

    #[test]
    fn t63_proxy_scheme_redacts() {
        // в лог идёт только схема, пароль из URL наружу не утекает
        let s = proxy_scheme("socks5://user:secret-password@127.0.0.1:10808");
        assert_eq!(s, "socks5");
        assert!(!s.contains("secret"));
        assert_eq!(proxy_scheme("http://127.0.0.1:10809"), "http");
    }

    #[test]
    fn t63_send_err_tags() {
        assert_eq!(SendErr::Network.tag(), "network");
        assert_eq!(SendErr::RateLimited.tag(), "rate_limited");
        assert_eq!(SendErr::Auth.tag(), "auth");
        assert_eq!(SendErr::Http.tag(), "http_error");
        // permanent только у auth (401/403/404)
        assert!(SendErr::Auth.is_permanent());
        assert!(!SendErr::Network.is_permanent());
        assert!(!SendErr::RateLimited.is_permanent());
        assert!(!SendErr::Http.is_permanent());
    }
}

// ============================================================
// [P62/1] Тесты чистой логики хаба
// ============================================================
#[cfg(test)]
mod p62_tests {
    use super::*;
    use crate::strategies::Signal;

    fn sig() -> Signal {
        Signal {
            symbol: "TAOUSDT".to_string(),
            side: "BUY".to_string(),
            price: 123.456789,
            quantity: 0.5,
            strategy_name: "Exit_TakeProfit".to_string(),
            leverage: 1.0,
            stop_loss: Some(120.0),
            take_profit: Some(130.0),
            atr_hint: 0.0,
            entry_spread: 0.0,
        }
    }

    #[test]
    fn t62_coalesce_first_send_then_suppress() {
        let now = std::time::Instant::now();
        let (d, n) = coalesce_decision(None, now, COALESCE_WINDOW);
        assert_eq!(d, Decision::Send);
        assert_eq!(n, 0);
        // повтор через 1 с — подавить
        let (d, n) = coalesce_decision(Some((now - Duration::from_secs(1), 0)), now, COALESCE_WINDOW);
        assert_eq!(d, Decision::Suppress);
        assert_eq!(n, 0);
    }

    #[test]
    fn t62_coalesce_after_window_sends_with_count() {
        let now = std::time::Instant::now();
        // последний отправ 61 с назад, подавлено было 5 → Send + счётчик 5 для сводки
        let (d, n) =
            coalesce_decision(Some((now - Duration::from_secs(61), 5)), now, COALESCE_WINDOW);
        assert_eq!(d, Decision::Send);
        assert_eq!(n, 5);
    }

    #[test]
    fn t62_day_loss_levels() {
        assert_eq!(day_loss_level(0.0, 30.0), None);
        assert_eq!(day_loss_level(14.9, 30.0), None);
        assert_eq!(day_loss_level(15.0, 30.0), Some(50));
        assert_eq!(day_loss_level(23.9, 30.0), Some(50));
        assert_eq!(day_loss_level(24.0, 30.0), Some(80));
        assert_eq!(day_loss_level(29.9, 30.0), Some(80));
        assert_eq!(day_loss_level(30.0, 30.0), Some(100));
        assert_eq!(day_loss_level(45.0, 30.0), Some(100));
    }

    #[test]
    fn t62_day_loss_guards() {
        assert_eq!(day_loss_level(10.0, 0.0), None); // деление на ноль
        assert_eq!(day_loss_level(-5.0, 30.0), None); // убыток отрицательный (профит)
        assert_eq!(day_loss_level(f64::NAN, 30.0), None);
        assert_eq!(day_loss_level(10.0, f64::INFINITY), None);
    }

    #[test]
    fn t62_fmt_close_contains_rails() {
        let s = fmt_close("tao_momentum", &sig(), -1.25);
        assert!(s.contains("tao_momentum"));
        assert!(s.contains("TAOUSDT"));
        assert!(s.contains("BUY"));
        assert!(s.contains("Exit_TakeProfit"));
        assert!(s.contains("-1.25"));
        assert!(s.contains("LOSS"));
    }

    #[test]
    fn t62_fmt_entry_contains_sl_tp() {
        let s = fmt_entry("tao_momentum", &sig(), 123.456789, 0.5);
        assert!(!s.contains("ENTRY")); // префикс даёт event, не форматтер
        assert!(s.contains("SL 120.000000"));
        assert!(s.contains("TP 130.000000"));
        assert!(s.contains("0.50000"));
        let mut s2 = sig();
        s2.stop_loss = None;
        s2.take_profit = None;
        let s = fmt_entry("bot2", &s2, 1.0, 1.0);
        assert!(s.contains("SL - | TP -"));
    }

    #[test]
    fn t62_truncate_is_char_safe() {
        let long = "ж".repeat(TEXT_CAP + 50);
        let t = truncate(&long);
        assert_eq!(t.chars().count(), TEXT_CAP + 1); // + многоточие
        assert!(truncate("коротко") == "коротко");
    }

    #[test]
    fn t62_once_reserve_dedups() {
        let mut set = HashSet::new();
        assert!(once_reserve(&mut set, "k1"));
        assert!(!once_reserve(&mut set, "k1"));
        assert!(once_reserve(&mut set, "k2"));
    }

    #[test]
    fn t62_next_send_delay_math() {
        let now = std::time::Instant::now();
        assert_eq!(next_send_delay(None, now), None);
        // слот в прошлом — не ждём
        assert_eq!(next_send_delay(Some(now - Duration::from_secs(2)), now), None);
        // слот через 0.5 с — ждём ~0.5 с
        let d = next_send_delay(Some(now + Duration::from_millis(500)), now);
        assert!(d.is_some());
        assert!(d.unwrap() <= Duration::from_millis(500));
    }
}
