//! P31: Walk-Forward Analysis MomentumStrategy (1h) — Rust-порт пакета P31.
//!
//! Порт momentum.rs (P24-редакция, v3 FULL: 6 патчей + P23 спред + P24 funding)
//! в WFA-каркас wfa_bounce.rs (P30 v2). Все фильтры живой стратегии воспроизведены
//! 1:1 в порядке живого process(): SignalFilter, loss-streak cooldown, volume-spike,
//! volume anomaly, ATR-порог, OFI, momentum(+согласование знака), ROC-дампы, MFI
//! в нетренде, EMA200+Supertrend (бинарный тренд-фильтр), режим Trending, торговые
//! часы, лимит сделок в день, пауза между сделками, min_tp-пол (fee-aware),
//! net_rr гейт + edge-запас 1.15, EV-гейт после 20 сделок.
//!
//! Соответствие живому боту (config_tao_momentum_v3.toml — Фаза A):
//!   lookback=12, atr_period=14, ofi_window=3, fee=0.0005, min_net_rr=1.2,
//!   tp_atr_mult=2.5, min_tp_pct=0.020, rr_ratio=1.5, min_atr_threshold=0.25,
//!   ofi_threshold=0.10, volume_anomaly_threshold=0.0 (клампится в 1.0
//!   builder'ом with_thresholds — .max(1.0), mod.rs:185),
//!   max_trades_per_day=5, min_time_between=1h, часы 0-24,
//!   regime (window 30, volatile 1.5%, trending 0.25%).
//!
//! Модель исполнения (как в P29/P30, чтобы гейты были сопоставимы):
//!   - 1 бар = 1 ЗАКРЫТЫЙ 1h-клайн; timestamp = close_time (как kline.timestamp
//!     в websocket/mod.rs:80);
//!   - сигнал по close бара -> вход по ЭТОЙ же close (signal.price = entry,
//!     округлён round_to_tick 0.01 — как живой округлитель);
//!   - проверки выхода начинаются со СЛЕДУЮЩЕГО бара: сначала гэп (open за SL/TP
//!     -> выход по open), затем внутри бара SL раньше TP (пессимистично);
//!   - издержки: net_rr-гейт считает 2*fee + spread + funding (P23/P24 формула,
//!     спред = entry*--spread как прокси bookTicker-спреда — консервативно, у
//!     живого бота без bookTicker было бы 0); учёт выхода: net_r = (PnL -
//!     cost_unit*entry)/sl_dist, cost_unit = 2*fee + spread + funding;
//!   - НЕ моделируется (оверлеи state_manager, то же упрощение, что в P29/P30
//!     для bounce): BE=1.5R, chandelier-трейл 2*ATR, TrailExit, Timeout/
//!     TimeoutFlat (state_manager.rs:318/517). Эффект смешанный, направление
//!     смещения не заявляем;
//!   - loss-streak cooldown: в WFA по времени бара (живой бот — wall clock,
//!     momentum.rs chrono::Utc::now(); на непрерывной сессии эквивалентно);
//!   - позиция, не закрывшаяся до конца окна OOS, в сделки не попадает
//!     (как в каркасе P29/P30).
//!
//! Только std (без новых зависимостей). Запуск:
//!   cargo run --release --bin wfa_momentum
//!   cargo run --release --bin wfa_momentum -- --data TAOUSDT-1h-450d.csv --out tao
//!
//! Гейт P31 (те же 5 критериев, что у P29/P30): n>=20; нижняя граница 95% CI
//! bootstrap по total R > 0; WR > breakeven+3п.п.; стресс-прогон R>0; плато
//! (>=3 конфига сетки с R>0 на OOS).

use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::exit;

// ---------- константы стратегии (momentum.rs, 1:1) ----------
const EMA200_PERIOD: usize = 200;
const EMA200_DEAD_ZONE_PCT: f64 = 0.001;
const VOL_SPIKE_REJECTION_MULT: f64 = 3.0;
const VOL_SPIKE_WINDOW: usize = 20;
const LOSS_STREAK_THRESHOLD: u32 = 2;
const LOSS_COOLDOWN_MS: i64 = 4 * 3600 * 1000;
const SUPERTREND_PERIOD: usize = 10;
const SUPERTREND_MULT: f64 = 3.0;
const MFI_PERIOD: usize = 14;
const MFI_OVERBOUGHT: f64 = 80.0;
const MFI_OVERSOLD: f64 = 20.0;
const ROC_PERIOD: usize = 12;
const ROC_DUMP_THRESHOLD: f64 = -0.05;
const MOMENTUM_MIN_ABS: f64 = 0.0005;
const EDGE_QUALITY_MULT: f64 = 1.15;
const TP_CAP_PCT: f64 = 0.02; // .min(0.02) в живом коде
const TICK_SIZE: f64 = 0.01; // momentum.rs: BTC_TICK_SIZE (захардкожен для всех символов)

fn die(msg: &str) -> ! {
    eprintln!("{}", msg);
    exit(1);
}

// формат числа как repr() в Python: 1.0 -> "1.0", 1.5 -> "1.5"
fn py_f64(v: f64) -> String {
    if v == v.trunc() {
        format!("{:.1}", v)
    } else {
        format!("{}", v)
    }
}

// сумма как в Python 3.12+: компенсированная сумма Ноймайера
fn py_sum(iter: impl Iterator<Item = f64>) -> f64 {
    let mut s = 0.0f64;
    let mut c = 0.0f64;
    for x in iter {
        let t = s + x;
        if s.abs() >= x.abs() {
            c += (s - t) + x;
        } else {
            c += (x - t) + s;
        }
        s = t;
    }
    s + c
}

// ---------- CLI ----------
struct Args {
    data: String,
    out: String,
    fee: f64,
    spread: f64,
    funding: f64,
    train: usize,
    test: usize,
    step: usize,
    warm: usize,
    seed: u64,
    n_min_train: usize,
    // параметры prod-конфига (дефолты = живой config_tao_momentum_v3.toml)
    lookback: usize,
    atr_period: usize,
    ofi_window: usize,
    min_atr: f64,    // в процентах (как в конфиге)
    ofi_thr: f64,
    vol_anom: f64,
    tp_atr_mult: f64,
    min_tp: f64, // доля (0.020 = 2%)
    rr: f64,
    max_per_day: u32,
    hours_start: u32,
    hours_end: u32,
    regime_win: usize,
    regime_vol: f64,   // проценты
    regime_trend: f64, // проценты
    min_net_rr: f64,
}

fn usage() {
    println!("P31 WFA MomentumStrategy (Rust-порт, только std)");
    println!("Запуск: cargo run --release --bin wfa_momentum [-- параметры]");
    println!("  --data ПУТЬ    CSV клайнов 1h (default: TAOUSDT-1h-450d.csv, fallback klines/)");
    println!("  --out КАТАЛОГ  результаты (default: wfa_out)");
    println!("  --fee F        taker на сторону, доля (default 0.0005 = живой config)");
    println!("  --spread F     спред полный круг, доля (default 0.0002)");
    println!("  --funding F    funding один интервал 8h, доля (default 0.0001 = P24)");
    println!("  --train N      train-окно, баров (default 2160 = 90 дней на 1h)");
    println!("  --test N       test-окно OOS, баров (default 720 = 30 дней)");
    println!("  --step N       шаг карусели, баров (default 720 = 30 дней)");
    println!("  --warm N       прогрев индикаторов, баров (default 250)");
    println!("  --seed N       seed bootstrap-CI (default 42)");
    println!("  --n-min-train N  минимум сделок в train для выбора конфига (default 8)");
    println!("  --- prod-параметры стратегии (живой v3-конфиг): ---");
    println!("  --lookback N --atr-period N --ofi-window N");
    println!("  --min-atr F (%, 0.25) --ofi F (0.10) --vol-anom F (1.0 после клампа)");
    println!("  --tp-atr F (2.5) --min-tp F (0.020) --rr F (1.5)");
    println!("  --max-per-day N (5) --hours N,M (0,24)");
    println!("  --regime-win N (30) --regime-vol F (1.5%) --regime-trend F (0.25%)");
    println!("  --min-net-rr F (1.2)");
}

fn parse_f64(args: &mut impl Iterator<Item = String>, flag: &str) -> f64 {
    let v = args
        .next()
        .unwrap_or_else(|| die(&format!("[!] {}: нужно значение", flag)));
    v.parse::<f64>()
        .unwrap_or_else(|_| die(&format!("[!] {}: плохое число '{}'", flag, v)))
}

fn parse_usize(args: &mut impl Iterator<Item = String>, flag: &str) -> usize {
    let v = args
        .next()
        .unwrap_or_else(|| die(&format!("[!] {}: нужно значение", flag)));
    v.parse::<usize>()
        .unwrap_or_else(|_| die(&format!("[!] {}: плохое число '{}'", flag, v)))
}

fn parse_args() -> Args {
    let mut a = Args {
        data: String::from("TAOUSDT-1h-450d.csv"),
        out: String::from("wfa_out"),
        fee: 0.0005,
        spread: 0.0002,
        funding: 0.0001,
        train: 2160,
        test: 720,
        step: 720,
        warm: 250,
        seed: 42,
        n_min_train: 8,
        lookback: 12,
        atr_period: 14,
        ofi_window: 3,
        min_atr: 0.25,
        ofi_thr: 0.10,
        vol_anom: 1.0,
        tp_atr_mult: 2.5,
        min_tp: 0.020,
        rr: 1.5,
        max_per_day: 5,
        hours_start: 0,
        hours_end: 24,
        regime_win: 30,
        regime_vol: 1.5,
        regime_trend: 0.25,
        min_net_rr: 1.2,
    };
    let mut it = env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "-h" | "--help" => {
                usage();
                exit(0);
            }
            "--data" => {
                a.data = it
                    .next()
                    .unwrap_or_else(|| die("[!] --data: нужно значение"));
            }
            "--out" => {
                a.out = it
                    .next()
                    .unwrap_or_else(|| die("[!] --out: нужно значение"));
            }
            "--fee" => a.fee = parse_f64(&mut it, "--fee"),
            "--spread" => a.spread = parse_f64(&mut it, "--spread"),
            "--funding" => a.funding = parse_f64(&mut it, "--funding"),
            "--train" => a.train = parse_usize(&mut it, "--train"),
            "--test" => a.test = parse_usize(&mut it, "--test"),
            "--step" => a.step = parse_usize(&mut it, "--step"),
            "--warm" => a.warm = parse_usize(&mut it, "--warm"),
            "--seed" => {
                let v = it
                    .next()
                    .unwrap_or_else(|| die("[!] --seed: нужно значение"));
                a.seed = v
                    .parse::<u64>()
                    .unwrap_or_else(|_| die(&format!("[!] --seed: плохое число '{}'", v)));
            }
            "--n-min-train" => a.n_min_train = parse_usize(&mut it, "--n-min-train"),
            "--lookback" => a.lookback = parse_usize(&mut it, "--lookback"),
            "--atr-period" => a.atr_period = parse_usize(&mut it, "--atr-period"),
            "--ofi-window" => a.ofi_window = parse_usize(&mut it, "--ofi-window"),
            "--min-atr" => a.min_atr = parse_f64(&mut it, "--min-atr"),
            "--ofi" => a.ofi_thr = parse_f64(&mut it, "--ofi"),
            "--vol-anom" => a.vol_anom = parse_f64(&mut it, "--vol-anom"),
            "--tp-atr" => a.tp_atr_mult = parse_f64(&mut it, "--tp-atr"),
            "--min-tp" => a.min_tp = parse_f64(&mut it, "--min-tp"),
            "--rr" => a.rr = parse_f64(&mut it, "--rr"),
            "--max-per-day" => a.max_per_day = parse_usize(&mut it, "--max-per-day") as u32,
            "--hours" => {
                let v = it
                    .next()
                    .unwrap_or_else(|| die("[!] --hours: нужно значение N,M"));
                let parts: Vec<&str> = v.split(',').collect();
                if parts.len() != 2 {
                    die("[!] --hours: формат N,M");
                }
                a.hours_start = parts[0]
                    .trim()
                    .parse::<u32>()
                    .unwrap_or_else(|_| die("[!] --hours: плохое число"));
                a.hours_end = parts[1]
                    .trim()
                    .parse::<u32>()
                    .unwrap_or_else(|_| die("[!] --hours: плохое число"));
            }
            "--regime-win" => a.regime_win = parse_usize(&mut it, "--regime-win"),
            "--regime-vol" => a.regime_vol = parse_f64(&mut it, "--regime-vol"),
            "--regime-trend" => a.regime_trend = parse_f64(&mut it, "--regime-trend"),
            "--min-net-rr" => a.min_net_rr = parse_f64(&mut it, "--min-net-rr"),
            other => die(&format!(
                "[!] неизвестный аргумент '{}', см. --help",
                other
            )),
        }
    }
    // fallback: если дефолтного файла нет, смотрим klines/ рядом с запуском
    if !Path::new(&a.data).exists() && a.data == "TAOUSDT-1h-450d.csv" {
        let alt = Path::new("klines").join("TAOUSDT-1h-450d.csv");
        if alt.exists() {
            a.data = alt.to_string_lossy().into_owned();
        }
    }
    a
}

// ---------- данные ----------
struct Bar {
    ts: i64,
    o: f64,
    h: f64,
    l: f64,
    c: f64,
    v: f64,
    tb: f64,
}

fn load_bars(path: &str) -> Vec<Bar> {
    let f = File::open(path)
        .unwrap_or_else(|e| die(&format!("[!] не удалось открыть {}: {}", path, e)));
    let mut bars = Vec::new();
    for (i, line) in BufReader::new(f).lines().enumerate() {
        let line = line.unwrap_or_else(|e| die(&format!("[!] ошибка чтения: {}", e)));
        if i == 0 || line.trim().is_empty() {
            continue; // заголовок / пустые строки
        }
        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() < 7 {
            die(&format!(
                "[!] строка {}: ожидалось >= 7 колонок (close_time,o,h,l,c,volume,taker_buy)",
                i + 1
            ));
        }
        let parse_f = |s: &str, name: &str| -> f64 {
            s.trim()
                .parse::<f64>()
                .unwrap_or_else(|_| die(&format!("[!] строка {}: плохое {} = '{}'", i + 1, name, s)))
        };
        bars.push(Bar {
            ts: cols[0]
                .trim()
                .parse::<i64>()
                .unwrap_or_else(|_| die(&format!("[!] строка {}: плохой close_time", i + 1))),
            o: parse_f(cols[1], "open"),
            h: parse_f(cols[2], "high"),
            l: parse_f(cols[3], "low"),
            c: parse_f(cols[4], "close"),
            v: parse_f(cols[5], "volume"),
            tb: parse_f(cols[6], "taker_buy_volume"),
        });
    }
    bars.sort_by_key(|b| b.ts);
    bars
}

// ---------- режим рынка (market_regime.rs, 1:1) ----------
#[derive(Clone, Copy, PartialEq)]
enum Regime {
    Trending,
    Ranging,
    Volatile,
}

struct RegimeDetector {
    window: usize,
    volatile_pct: f64,
    trending_pct: f64,
    dir_thr: f64,     // 0.6
    strong_thr: f64,  // 0.7
    hist: Vec<f64>,
}

impl RegimeDetector {
    fn new(window: usize, volatile_pct: f64, trending_pct: f64) -> Self {
        RegimeDetector {
            window: window.max(20),
            volatile_pct,
            trending_pct,
            dir_thr: 0.6,
            strong_thr: 0.7,
            hist: Vec::with_capacity(window.max(20) * 2),
        }
    }

    fn push_and_detect(&mut self, close: f64) -> Regime {
        if close > 0.0 {
            self.hist.push(close);
        }
        if self.hist.len() > self.window {
            let excess = self.hist.len() - self.window;
            self.hist.drain(0..excess);
        }
        if self.hist.len() < 20 {
            return Regime::Ranging;
        }
        let last = *self.hist.last().unwrap();
        if last <= 0.0 {
            return Regime::Ranging;
        }
        let max = self.hist.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = self.hist.iter().cloned().fold(f64::INFINITY, f64::min);
        let range_pct = (max - min) / last * 100.0;
        let first = self.hist[0];
        let dir_move = (last - first).abs() / last * 100.0;
        let bias = if range_pct > 0.0 { dir_move / range_pct } else { 0.0 };
        // Новая логика с directional bias (market_regime.rs, порядок 1:1)
        if range_pct > self.volatile_pct && bias < self.strong_thr {
            return Regime::Volatile;
        }
        if range_pct > self.trending_pct && bias >= self.dir_thr {
            return Regime::Trending;
        }
        if range_pct > self.volatile_pct && bias >= self.strong_thr {
            return Regime::Trending;
        }
        Regime::Ranging
    }
}

// ---------- конфиги ----------
#[derive(Clone, Copy)]
struct Cfg {
    lookback: usize,
    atr_period: usize,
    ofi_window: usize,
    min_atr: f64, // проценты
    ofi_thr: f64,
    vol_anom: f64,
    tp_atr_mult: f64,
    min_tp: f64,
    rr: f64,
    max_per_day: u32,
    hours_start: u32,
    hours_end: u32,
    regime_win: usize,
    regime_vol: f64,
    regime_trend: f64,
    min_net_rr: f64,
}

fn prod_cfg(a: &Args) -> Cfg {
    Cfg {
        lookback: a.lookback,
        atr_period: a.atr_period,
        ofi_window: a.ofi_window,
        min_atr: a.min_atr,
        ofi_thr: a.ofi_thr,
        vol_anom: a.vol_anom,
        tp_atr_mult: a.tp_atr_mult,
        min_tp: a.min_tp,
        rr: a.rr,
        max_per_day: a.max_per_day,
        hours_start: a.hours_start,
        hours_end: a.hours_end,
        regime_win: a.regime_win,
        regime_vol: a.regime_vol,
        regime_trend: a.regime_trend,
        min_net_rr: a.min_net_rr,
    }
}

// Сетка плато: один фактор за раз от prod (остальное = prod).
fn build_grid(p: &Cfg) -> Vec<Cfg> {
    let mut g = vec![*p]; // 0: prod
    let variants: [(usize, f64, f64, f64); 7] = [
        (20, -1.0, -1.0, -1.0),  // lookback 20
        (0, 0.15, -1.0, -1.0),   // ofi 0.15
        (0, 0.06, -1.0, -1.0),   // ofi 0.06
        (0, -1.0, 2.0, -1.0),    // rr 2.0
        (0, -1.0, 2.5, -1.0),    // rr 2.5
        (0, -1.0, -1.0, 0.015),  // min_tp 0.015
        (0, -1.0, -1.0, -1.1),   // vol_anom 1.5 (маркер <0 -> ветка ниже)
    ];
    for v in variants {
        let mut c = *p;
        if v.0 > 0 {
            c.lookback = v.0;
        }
        if v.1 >= 0.0 {
            c.ofi_thr = v.1;
        }
        if v.2 >= 0.0 {
            c.rr = v.2;
        }
        if v.3 >= 0.0 {
            c.min_tp = v.3;
        }
        if v.3 < -1.0 {
            c.vol_anom = 1.5;
        }
        g.push(c);
    }
    g
}

fn cfg_tag(c: &Cfg) -> String {
    format!(
        "lb{} ofi{:.2} rr{} mtp{}% va{}",
        c.lookback,
        c.ofi_thr,
        py_f64(c.rr),
        py_f64(c.min_tp * 100.0),
        py_f64(c.vol_anom)
    )
}

// ---------- счётчики фильтр-воронки (диагностика prod OOS) ----------
#[derive(Default, Clone, Copy)]
struct Counters {
    total: u64,
    blocked_filter: u64,
    blocked_warm: u64,
    blocked_loss_streak: u64,
    blocked_vol_spike: u64,
    blocked_volume: u64,
    blocked_atr: u64,
    blocked_ofi: u64,
    blocked_momentum: u64,
    blocked_roc: u64,
    blocked_mfi: u64,
    blocked_trend: u64,
    blocked_regime: u64,
    blocked_time: u64,
    blocked_limit_day: u64,
    blocked_cooldown: u64,
    blocked_net_rr: u64,
    blocked_edge: u64,
    blocked_ev: u64,
    passed: u64,
}

// ---------- симулятор стратегии ----------
#[derive(Clone, Copy)]
struct Pos {
    side: i8, // +1 long, -1 short
    entry: f64,
    sl: f64,
    tp: f64,
    sl_dist: f64,
    ts_in: i64,
}

#[derive(Clone)]
struct Trade {
    side: i8,
    ts_in: i64,
    ts_out: i64,
    entry: f64,
    sl: f64,
    tp: f64,
    exit: f64,
    why: &'static str,
    sl_dist: f64,
    net_r: f64,
}

struct Strat {
    // истории (живые VecDeque -> Vec с обрезкой спереди)
    closes: Vec<f64>,
    highs: Vec<f64>,
    lows: Vec<f64>,
    volumes: Vec<f64>,
    taker_buys: Vec<f64>,
    atrs: Vec<f64>,
    max_hist: usize,
    // параметры
    cfg: Cfg,
    // EMA200
    ema200: Option<f64>,
    ema_alpha: f64,
    // Supertrend
    st_prev_up: Option<f64>,
    st_prev_lo: Option<f64>,
    st_dir: i8,
    // режим
    regime: RegimeDetector,
    //SignalFilter
    f_state: u8, // 0 flat, 1 cooldown, 2 in position
    f_last_signal_at: Option<i64>,
    f_last_signal_price: f64,
    // лимиты/стрики
    trades_today: u32,
    current_day: i64,
    last_trade_ts: Option<i64>,
    consecutive_losses: u32,
    last_loss_ts: Option<i64>,
    // EV-гейт (история сделок, как record_trade_result)
    ev_won: u32,
    ev_lost: u32,
    ev_win_amt: f64,
    ev_loss_amt: f64,
    // издержки
    fee: f64,
    spread: f64,
    funding: f64,
    counters: Counters,
}

fn round_tick(price: f64) -> f64 {
    (price / TICK_SIZE).round() * TICK_SIZE
}

impl Strat {
    fn new(cfg: Cfg, fee: f64, spread: f64, funding: f64) -> Self {
        let max_hist = cfg
            .lookback
            .max(cfg.atr_period * 2)
            .max(cfg.ofi_window)
            .max(EMA200_PERIOD)
            .max(120)
            + 10;
        Strat {
            closes: Vec::new(),
            highs: Vec::new(),
            lows: Vec::new(),
            volumes: Vec::new(),
            taker_buys: Vec::new(),
            atrs: Vec::new(),
            max_hist,
            cfg,
            ema200: None,
            ema_alpha: 2.0 / (EMA200_PERIOD as f64 + 1.0),
            st_prev_up: None,
            st_prev_lo: None,
            st_dir: 1,
            regime: RegimeDetector::new(cfg.regime_win, cfg.regime_vol, cfg.regime_trend),
            f_state: 0,
            f_last_signal_at: None,
            f_last_signal_price: 0.0,
            trades_today: 0,
            current_day: -1,
            last_trade_ts: None,
            consecutive_losses: 0,
            last_loss_ts: None,
            ev_won: 0,
            ev_lost: 0,
            ev_win_amt: 0.0,
            ev_loss_amt: 0.0,
            fee,
            spread,
            funding,
            counters: Counters::default(),
        }
    }

    fn current_atr(&self) -> f64 {
        self.atrs.last().copied().unwrap_or(0.0)
    }

    fn update_atr(&mut self) {
        let len = self.closes.len();
        if len < 2 {
            return;
        }
        let high = self.highs[len - 1];
        let low = self.lows[len - 1];
        let prev_close = self.closes[len - 2];
        let tr = (high - low)
            .max((high - prev_close).abs())
            .max((low - prev_close).abs());
        let atr = match self.atrs.last() {
            Some(&prev) => {
                let n = self.cfg.atr_period as f64;
                (prev * (n - 1.0) + tr) / n
            }
            None => tr,
        };
        self.atrs.push(atr);
        let max_hist = self.cfg.atr_period * 3 + 10;
        if self.atrs.len() > max_hist {
            self.atrs.remove(0);
        }
    }

    fn update_ema200(&mut self, close: f64) {
        if let Some(prev) = self.ema200 {
            self.ema200 = Some(close * self.ema_alpha + prev * (1.0 - self.ema_alpha));
        } else if self.closes.len() >= EMA200_PERIOD {
            // take(200) на деке = самые СТАРЫЕ 200 значений
            let sum: f64 = py_sum(self.closes.iter().take(EMA200_PERIOD).copied());
            self.ema200 = Some(sum / EMA200_PERIOD as f64);
        }
    }

    fn update_supertrend(&mut self) {
        let len = self.closes.len();
        if len < SUPERTREND_PERIOD + 1 {
            return;
        }
        let high = self.highs[len - 1];
        let low = self.lows[len - 1];
        let hl2 = (high + low) / 2.0;
        let close = self.closes[len - 1];
        let atr_window = SUPERTREND_PERIOD.min(self.atrs.len());
        if atr_window == 0 {
            return;
        }
        let atr_sum: f64 = py_sum(self.atrs.iter().rev().take(atr_window).copied());
        let atr = atr_sum / atr_window as f64;
        let basic_upper = hl2 + SUPERTREND_MULT * atr;
        let basic_lower = hl2 - SUPERTREND_MULT * atr;
        let prev_close = self.closes[len - 2];
        let final_upper = match self.st_prev_up {
            Some(prev_up) => {
                if basic_upper < prev_up || prev_close > prev_up {
                    basic_upper
                } else {
                    prev_up
                }
            }
            None => basic_upper,
        };
        let final_lower = match self.st_prev_lo {
            Some(prev_lo) => {
                if basic_lower > prev_lo || prev_close < prev_lo {
                    basic_lower
                } else {
                    prev_lo
                }
            }
            None => basic_lower,
        };
        let new_dir = if self.st_dir == 1 {
            if close < final_lower { -1 } else { 1 }
        } else if close > final_upper {
            1
        } else {
            -1
        };
        self.st_prev_up = Some(final_upper);
        self.st_prev_lo = Some(final_lower);
        self.st_dir = new_dir;
    }

    fn is_volume_spike(&self) -> bool {
        let len = self.volumes.len();
        if len < VOL_SPIKE_WINDOW + 1 {
            return false;
        }
        let current = self.volumes[len - 1];
        let mut sum = 0.0;
        for i in (len - 1 - VOL_SPIKE_WINDOW)..(len - 1) {
            sum += self.volumes[i];
        }
        let avg = sum / VOL_SPIKE_WINDOW as f64;
        if avg <= 0.0 {
            return false;
        }
        current >= avg * VOL_SPIKE_REJECTION_MULT
    }

    fn volume_anomaly(&self) -> f64 {
        let len = self.volumes.len();
        if len < 10 {
            return 1.0;
        }
        let window = 100.min(len - 1);
        let start = len - 1 - window;
        let mut sum = 0.0;
        for i in start..(len - 1) {
            sum += self.volumes[i];
        }
        let avg = sum / window as f64;
        if avg <= 0.0 {
            1.0
        } else {
            self.volumes[len - 1] / avg
        }
    }

    fn calculate_ofi(&self) -> f64 {
        let n = self.cfg.ofi_window.min(self.taker_buys.len());
        if n == 0 {
            return 0.0;
        }
        let start = self.taker_buys.len() - n;
        let mut buy = 0.0;
        let mut sell = 0.0;
        for i in start..self.taker_buys.len() {
            buy += self.taker_buys[i];
            sell += (self.volumes[i] - self.taker_buys[i]).max(0.0);
        }
        let total = buy + sell;
        if total <= 0.0 {
            0.0
        } else {
            (buy - sell) / total
        }
    }

    fn calculate_momentum(&self) -> f64 {
        let len = self.closes.len();
        if len < self.cfg.lookback {
            return 0.0;
        }
        let now = self.closes[len - 1];
        let prev = self.closes[len - self.cfg.lookback];
        if prev <= 0.0 {
            0.0
        } else {
            (now - prev) / prev
        }
    }

    fn calculate_mfi(&self) -> Option<f64> {
        let len = self.closes.len();
        if len < MFI_PERIOD + 1 {
            return None;
        }
        let mut pos_flow = 0.0;
        let mut neg_flow = 0.0;
        let start = len - MFI_PERIOD - 1;
        for i in start..(len - 1) {
            let typical = (self.highs[i + 1] + self.lows[i + 1] + self.closes[i + 1]) / 3.0;
            let prev_typical = (self.highs[i] + self.lows[i] + self.closes[i]) / 3.0;
            let money_flow = typical * self.volumes[i + 1];
            if typical > prev_typical {
                pos_flow += money_flow;
            } else if typical < prev_typical {
                neg_flow += money_flow;
            }
        }
        if neg_flow <= 0.0 {
            return Some(100.0);
        }
        let ratio = pos_flow / neg_flow;
        Some(100.0 - 100.0 / (1.0 + ratio))
    }

    fn calculate_roc(&self) -> Option<f64> {
        let len = self.closes.len();
        if len < ROC_PERIOD + 1 {
            return None;
        }
        let current = self.closes[len - 1];
        let past = self.closes[len - 1 - ROC_PERIOD];
        if past <= 0.0 {
            return None;
        }
        Some((current - past) / past)
    }

    fn is_trading_hour(&self, ts_ms: i64) -> bool {
        let secs_of_day = ((ts_ms / 1000) % 86_400) as u32;
        let hour = secs_of_day / 3600;
        if self.cfg.hours_start <= self.cfg.hours_end {
            hour >= self.cfg.hours_start && hour < self.cfg.hours_end
        } else {
            hour >= self.cfg.hours_start || hour < self.cfg.hours_end
        }
    }

    fn min_tp_floor(&self) -> f64 {
        let fee_rt = 2.0 * self.fee;
        let denom = 1.0 - (1.0 / self.cfg.rr) * self.cfg.min_net_rr;
        if denom <= 0.0 {
            return f64::INFINITY;
        }
        (fee_rt * (1.0 + self.cfg.min_net_rr) / denom).max(0.001)
    }

    // net_rr 1:1 с живой (P23 спред + P24 funding); спред в WFA = entry*--spread
    fn net_rr(&self, entry: f64, sl: f64, tp: f64, is_long: bool) -> Option<f64> {
        let fee_cost = 2.0 * self.fee * entry;
        let spread_cost = entry * self.spread;
        let (gross_profit, gross_loss) = if is_long {
            (tp - entry - spread_cost, entry - sl + spread_cost)
        } else {
            (entry - tp - spread_cost, sl - entry + spread_cost)
        };
        let funding_cost = entry * self.funding;
        let net_profit = gross_profit - fee_cost - funding_cost;
        let net_loss = gross_loss + fee_cost + funding_cost;
        if net_loss <= 0.0 || net_profit <= 0.0 {
            return None;
        }
        Some(net_profit / net_loss)
    }

    // SignalFilter::can_signal (mod.rs:45) — 1:1
    fn can_signal(&mut self, price: f64, now_ms: i64) -> bool {
        match self.f_state {
            2 => false,
            0 => true,
            _ => {
                let t = match self.f_last_signal_at {
                    Some(t) => t,
                    None => {
                        self.f_state = 0;
                        return true;
                    }
                };
                let price_moved = if self.f_last_signal_price > 0.0 {
                    (price - self.f_last_signal_price).abs() / self.f_last_signal_price
                } else {
                    0.0
                };
                if price_moved > 0.15 / 100.0 {
                    self.f_state = 0;
                    return true;
                }
                if now_ms.saturating_sub(t) >= 60_000 {
                    self.f_state = 0;
                    return true;
                }
                false
            }
        }
    }

    fn record_trade_result(&mut self, net_r: f64) {
        if net_r > 0.0 {
            self.ev_won += 1;
            self.ev_win_amt += net_r;
            self.consecutive_losses = 0;
        } else {
            self.ev_lost += 1;
            self.ev_loss_amt += net_r.abs();
            self.consecutive_losses = self.consecutive_losses.saturating_add(1);
            self.last_loss_ts = None; // перезаписывается вызывающим кодом (ts бара)
        }
    }
}

// Один прогон стратегии по окну [warmup_from, sig_to); сигналы только в
// [sig_from, sig_to). Возвращает сделки prod-конфига.
fn run(
    bars: &[Bar],
    cfg: Cfg,
    sig_from: usize,
    sig_to: usize,
    warmup_from: usize,
    fee: f64,
    spread: f64,
    funding: f64,
    cost_unit: f64,
    counters: &mut Counters,
) -> Vec<Trade> {
    let mut st = Strat::new(cfg, fee, spread, funding);
    let mut trades: Vec<Trade> = Vec::new();
    let mut pos: Option<Pos> = None;
    let end = sig_to.min(bars.len());
    let mut c = Counters::default();

    for i in warmup_from..end {
        let b = &bars[i];
        let (ts, o, h, l, cl) = (b.ts, b.o, b.h, b.l, b.c);
        let in_sig = sig_from <= i && i < sig_to;

        // --- 1) выход из позиции на этой свече (вход был по close СЛЕДУЮЩЕЙ
        //        после сигнала, поэтому первый бар проверки = следующий) ---
        if let Some(p) = pos {
            let mut exit_px = f64::NAN;
            let mut why = "";
            let mut hit = false;
            if p.side == 1 {
                if o <= p.sl {
                    exit_px = o; why = "gap_sl"; hit = true;
                } else if o >= p.tp {
                    exit_px = o; why = "gap_tp"; hit = true;
                } else if l <= p.sl {
                    exit_px = p.sl; why = "sl"; hit = true;
                } else if h >= p.tp {
                    exit_px = p.tp; why = "tp"; hit = true;
                }
            } else if o >= p.sl {
                exit_px = o; why = "gap_sl"; hit = true;
            } else if o <= p.tp {
                exit_px = o; why = "gap_tp"; hit = true;
            } else if h >= p.sl {
                exit_px = p.sl; why = "sl"; hit = true;
            } else if l <= p.tp {
                exit_px = p.tp; why = "tp"; hit = true;
            }
            if hit {
                let d = if p.side == 1 { 1.0 } else { -1.0 };
                let cost = p.entry * cost_unit;
                let net_r = (d * (exit_px - p.entry) - cost) / p.sl_dist;
                trades.push(Trade {
                    side: p.side,
                    ts_in: p.ts_in,
                    ts_out: ts,
                    entry: p.entry,
                    sl: p.sl,
                    tp: p.tp,
                    exit: exit_px,
                    why,
                    sl_dist: p.sl_dist,
                    net_r,
                });
                // record_trade_result + filter.on_position_closed (1:1)
                st.record_trade_result(net_r);
                if net_r <= 0.0 {
                    st.last_loss_ts = Some(ts);
                }
                st.f_state = 1; // Cooldown
                st.f_last_signal_at = Some(ts);
                // ✅ P36/F5 (зеркало strategies/mod.rs): сброс якоря цены,
                // чтобы живой код и WFA-реплика не разъехались.
                st.f_last_signal_price = 0.0;
                pos = None;
            }
        }

        // --- 2) режим рынка (detect_from_msg ДО пушей, 1:1 с process()) ---
        let regime = st.regime.push_and_detect(cl);

        // --- 3) день/лимит ---
        let day = ts / 86_400_000;
        if day != st.current_day {
            st.current_day = day;
            st.trades_today = 0;
        }

        // --- 4) пуш баров + трим ---
        st.closes.push(cl);
        st.highs.push(h);
        st.lows.push(l);
        st.volumes.push(b.v);
        st.taker_buys.push(b.tb);
        if st.closes.len() > st.max_hist {
            st.closes.remove(0);
            st.highs.remove(0);
            st.lows.remove(0);
            st.volumes.remove(0);
            st.taker_buys.remove(0);
        }

        // --- 5) индикаторы (как живой: atr, ema200, supertrend) ---
        st.update_atr();
        st.update_ema200(cl);
        st.update_supertrend();

        // --- 6) сигнальная цепочка (только в сигнальном окне) ---
        if !in_sig {
            continue;
        }
        c.total += 1;
        // is_backfill-гейта в WFA нет: [warmup_from, sig_from) вообще не
        // доходит сюда (in_sig=false) — эквивалент прогрева.
        if !st.can_signal(cl, ts) {
            c.blocked_filter += 1;
            continue;
        }
        if st.closes.len() < st.cfg.lookback.max(st.cfg.atr_period) {
            c.blocked_warm += 1;
            continue;
        }
        // ПАТЧ 3: loss-streak cooldown (в WFA — время бара)
        if st.consecutive_losses >= LOSS_STREAK_THRESHOLD {
            if let Some(last_ts) = st.last_loss_ts {
                if ts - last_ts < LOSS_COOLDOWN_MS {
                    c.blocked_loss_streak += 1;
                    continue;
                }
            }
        }
        // ПАТЧ 2: volume spike rejection
        if st.is_volume_spike() {
            c.blocked_vol_spike += 1;
            continue;
        }
        let vol_anom = st.volume_anomaly();
        if vol_anom < st.cfg.vol_anom {
            c.blocked_volume += 1;
            continue;
        }
        let atr = st.current_atr();
        let atr_pct = atr / cl;
        let min_atr_pct = st.cfg.min_atr / 100.0;
        if atr_pct < min_atr_pct {
            c.blocked_atr += 1;
            continue;
        }
        let ofi = st.calculate_ofi();
        if ofi.abs() < st.cfg.ofi_thr {
            c.blocked_ofi += 1;
            continue;
        }
        let momentum = st.calculate_momentum();
        if momentum.abs() < MOMENTUM_MIN_ABS {
            c.blocked_momentum += 1;
            continue;
        }
        if (ofi > 0.0) != (momentum > 0.0) {
            c.blocked_momentum += 1;
            continue;
        }
        let is_long = momentum > 0.0;
        // ПАТЧ 6: ROC — block SHORT на паническом дампе
        let roc = st.calculate_roc();
        let is_dump = roc.map(|r| r < ROC_DUMP_THRESHOLD).unwrap_or(false);
        if is_dump && !is_long {
            c.blocked_roc += 1;
            continue;
        }
        // ПАТЧ 5: MFI — фильтр боковика (в нетренде)
        if regime != Regime::Trending {
            if let Some(mfi) = st.calculate_mfi() {
                if mfi > MFI_OVERBOUGHT || mfi < MFI_OVERSOLD {
                    c.blocked_mfi += 1;
                    continue;
                }
            }
        }
        // ПАТЧ 1+4: EMA200 + Supertrend — бинарный тренд-фильтр
        let above_ema = match st.ema200 {
            Some(ema) => cl > ema * (1.0 + EMA200_DEAD_ZONE_PCT),
            None => false,
        };
        let below_ema = match st.ema200 {
            Some(ema) => cl < ema * (1.0 - EMA200_DEAD_ZONE_PCT),
            None => false,
        };
        let st_up = st.st_dir == 1;
        if is_long && (!above_ema || !st_up) {
            c.blocked_trend += 1;
            continue;
        }
        if !is_long && (!below_ema || st_up) {
            c.blocked_trend += 1;
            continue;
        }
        // Regime
        if regime != Regime::Trending {
            c.blocked_regime += 1;
            continue;
        }
        if !st.is_trading_hour(ts) {
            c.blocked_time += 1;
            continue;
        }
        if st.trades_today >= st.cfg.max_per_day {
            c.blocked_limit_day += 1;
            continue;
        }
        if let Some(last_ts) = st.last_trade_ts {
            if ts - last_ts < 3_600_000.min(i64::MAX) {
                // min_time_between_trades_ms (конфиг v3 = 3600000)
                c.blocked_cooldown += 1;
                continue;
            }
        }

        let min_tp_floor = st.min_tp_floor();
        let tp_pct = (atr_pct * st.cfg.tp_atr_mult)
            .max(min_tp_floor)
            .max(st.cfg.min_tp)
            .min(TP_CAP_PCT);
        let sl_pct = tp_pct / st.cfg.rr;

        let (side, sl_raw, tp_raw) = if is_long {
            (1i8, cl * (1.0 - sl_pct), cl * (1.0 + tp_pct))
        } else {
            (-1i8, cl * (1.0 + sl_pct), cl * (1.0 - tp_pct))
        };
        let entry = round_tick(cl);
        let sl = round_tick(sl_raw);
        let tp = round_tick(tp_raw);

        let eff_rr = match st.net_rr(entry, sl, tp, is_long) {
            Some(rr) => rr,
            None => {
                c.blocked_net_rr += 1;
                continue;
            }
        };
        if eff_rr + 1e-4 < st.cfg.min_net_rr {
            c.blocked_net_rr += 1;
            continue;
        }
        if eff_rr < st.cfg.min_net_rr * EDGE_QUALITY_MULT {
            c.blocked_edge += 1;
            continue;
        }

        // EV-гейт (после 20 закрытых сделок; min_ev_pct = 0.0)
        let total_trades = st.ev_won + st.ev_lost;
        if total_trades >= 20 {
            let wr = st.ev_won as f64 / total_trades as f64;
            let avg_win = if st.ev_won > 0 {
                st.ev_win_amt / st.ev_won as f64
            } else {
                0.0
            };
            let avg_loss = if st.ev_lost > 0 {
                st.ev_loss_amt / st.ev_lost as f64
            } else {
                0.0
            };
            if avg_loss > 0.0 {
                let ev = wr * avg_win - (1.0 - wr) * avg_loss;
                let ev_pct = ev / avg_loss * 100.0;
                if ev_pct < 0.0 {
                    c.blocked_ev += 1;
                    continue;
                }
            }
        }

        // --- 7) вход по close сигнального бара (signal.price = entry) ---
        let sl_dist = if is_long {
            entry - sl
        } else {
            sl - entry
        };
        if sl_dist <= 0.0 {
            c.blocked_net_rr += 1;
            continue;
        }
        pos = Some(Pos {
            side,
            entry,
            sl,
            tp,
            sl_dist,
            ts_in: ts,
        });
        st.passed_count(&mut c);
        st.trades_today += 1;
        st.last_trade_ts = Some(ts);
        st.f_state = 1; // on_signal -> Cooldown (позиция откроется сразу)
        st.f_last_signal_at = Some(ts);
        st.f_last_signal_price = entry;
    }
    *counters = c;
    trades
}

impl Strat {
    fn passed_count(&mut self, c: &mut Counters) {
        self.counters.passed += 1;
        c.passed += 1;
    }
}

// ---------- статистика ----------
struct St {
    n: usize,
    wr: f64,
    total_r: f64,
    pf: f64,
    maxdd: f64,
    avg_r: f64,
}

fn stats(trades: &[Trade]) -> St {
    if trades.is_empty() {
        return St { n: 0, wr: 0.0, total_r: 0.0, pf: 0.0, maxdd: 0.0, avg_r: 0.0 };
    }
    let n = trades.len();
    let rs: Vec<f64> = trades.iter().map(|t| t.net_r).collect();
    let wins = rs.iter().filter(|&&x| x > 0.0).count();
    let gw: f64 = py_sum(rs.iter().filter(|&&x| x > 0.0).copied());
    let gl: f64 = py_sum(rs.iter().filter(|&&x| x <= 0.0).map(|x| -x));
    let mut eq = 0.0f64;
    let mut peak = 0.0f64;
    let mut maxdd = 0.0f64;
    for &x in &rs {
        eq += x;
        if eq > peak {
            peak = eq;
        }
        if peak - eq > maxdd {
            maxdd = peak - eq;
        }
    }
    St {
        n,
        wr: 100.0 * wins as f64 / n as f64,
        total_r: py_sum(rs.iter().copied()),
        pf: if gl > 0.0 { gw / gl } else { f64::INFINITY },
        maxdd,
        avg_r: py_sum(rs.iter().copied()) / n as f64,
    }
}

fn fmt_st(s: &St) -> String {
    if s.n == 0 {
        return String::from("n=0");
    }
    format!(
        "n={} WR={:.1}% R={:+.2} PF={:.2} maxDD={:.2}R avg={:+.3}R",
        s.n, s.wr, s.total_r, s.pf, s.maxdd, s.avg_r
    )
}

// ---------- ГПСЧ MT19937 (бит-в-бит совместим с random.Random из CPython) ----------
struct Mt19937 {
    mt: [u32; 624],
    idx: usize,
}

impl Mt19937 {
    fn init_genrand(&mut self, s: u32) {
        self.mt[0] = s;
        for i in 1..624 {
            let prev = self.mt[i - 1];
            self.mt[i] = 1812433253u32
                .wrapping_mul(prev ^ (prev >> 30))
                .wrapping_add(i as u32);
        }
        self.idx = 624;
    }

    fn new(seed: u64) -> Self {
        let mut key: Vec<u32> = Vec::new();
        if seed == 0 {
            key.push(0);
        } else {
            key.push((seed & 0xffff_ffff) as u32);
            let hi = (seed >> 32) as u32;
            if hi != 0 {
                key.push(hi);
            }
        }
        let mut m = Mt19937 { mt: [0u32; 624], idx: 624 };
        m.init_genrand(19650218);
        let mut i = 1usize;
        let mut j = 0usize;
        let mut k = if 624 > key.len() { 624 } else { key.len() };
        while k > 0 {
            let prev = m.mt[i - 1];
            m.mt[i] = (m.mt[i] ^ ((prev ^ (prev >> 30)).wrapping_mul(1664525)))
                .wrapping_add(key[j])
                .wrapping_add(j as u32);
            i += 1;
            j += 1;
            if i >= 624 {
                m.mt[0] = m.mt[623];
                i = 1;
            }
            if j >= key.len() {
                j = 0;
            }
            k -= 1;
        }
        k = 623;
        while k > 0 {
            let prev = m.mt[i - 1];
            m.mt[i] = (m.mt[i] ^ ((prev ^ (prev >> 30)).wrapping_mul(1566083941)))
                .wrapping_sub(i as u32);
            i += 1;
            if i >= 624 {
                m.mt[0] = m.mt[623];
                i = 1;
            }
            k -= 1;
        }
        m.mt[0] = 0x8000_0000;
        m
    }

    fn genrand_u32(&mut self) -> u32 {
        if self.idx >= 624 {
            for i in 0..624 {
                let y = (self.mt[i] & 0x8000_0000) | (self.mt[(i + 1) % 624] & 0x7fff_ffff);
                let mut nxt = self.mt[(i + 397) % 624] ^ (y >> 1);
                if y & 1 != 0 {
                    nxt ^= 0x9908_b0df;
                }
                self.mt[i] = nxt;
            }
            self.idx = 0;
        }
        let mut y = self.mt[self.idx];
        self.idx += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^= y >> 18;
        y
    }

    fn getrandbits(&mut self, k: u32) -> u32 {
        self.genrand_u32() >> (32 - k)
    }

    fn below(&mut self, n: usize) -> usize {
        let n = n as u32;
        let k = 32 - n.leading_zeros();
        let mut r = self.getrandbits(k);
        while r >= n {
            r = self.getrandbits(k);
        }
        r as usize
    }
}

struct FoldRow {
    fold: usize,
    train: String,
    test: String,
    chosen: String,
    train_stats: St,
    oos_chosen: St,
    oos_prod: St,
}

fn main() {
    let args = parse_args();
    let cost_unit = 2.0 * args.fee + args.spread + args.funding;
    let seed = args.seed;

    if !Path::new(&args.data).exists() {
        die(&format!(
            "[!] файл данных не найден: {}\n    положи TAOUSDT-1h-450d.csv в корень проекта или укажи: cargo run --release --bin wfa_momentum -- --data ПУТЬ",
            args.data
        ));
    }
    let mut bars = load_bars(&args.data);
    bars.sort_by_key(|b| b.ts);
    let n = bars.len();
    if n < args.train + args.test + args.warm {
        die(&format!(
            "[!] мало данных: {} баров, нужно >= train+test+warm = {} (на 1h: {} дней)",
            n,
            args.train + args.test + args.warm,
            (args.train + args.test + args.warm) / 24
        ));
    }

    println!("[P31] data = {}", args.data);
    println!(
        "[P31] bars={} | fee={} spread={} funding={} | bootstrap seed={}",
        n, args.fee, args.spread, args.funding, args.seed
    );

    // ---------- WFA ----------
    let (train, test, step, warm) = (args.train, args.test, args.step, args.warm);
    let mut folds: Vec<usize> = Vec::new();
    let mut s = 0usize;
    while s + train + test <= n {
        folds.push(s);
        s += step;
    }
    if folds.is_empty() {
        die(&format!(
            "[!] фолдов 0: данных {} баров не хватает даже на один фолд (train {} + test {})",
            n, train, test
        ));
    }
    println!("bars={} folds={} train={} test={}", n, folds.len(), train, test);

    let prod = prod_cfg(&args);
    let grid = build_grid(&prod);
    let mut per_fold: Vec<FoldRow> = Vec::new();
    let mut chosen_oos: Vec<Trade> = Vec::new();
    let mut prod_oos: Vec<Trade> = Vec::new();
    let mut grid_oos: Vec<Vec<Trade>> = vec![Vec::new(); grid.len()];
    let mut prod_counters = Counters::default();
    let mut last_cfg = prod;

    for (k, &s0) in folds.iter().enumerate() {
        let (t0, t1, e0, e1) = (s0, s0 + train, s0 + train, s0 + train + test);
        let sig_train_from = if s0 == 0 { t0 + warm } else { t0 };
        let mut dummy = Counters::default();
        // --- train: выбор конфига ---
        let mut best_cfg = last_cfg;
        let mut best_r = f64::NEG_INFINITY;
        let mut best_stats: Option<St> = None;
        for g in &grid {
            let tr = run(&bars, *g, sig_train_from, t1, t0, args.fee, args.spread, args.funding, cost_unit, &mut dummy);
            let st = stats(&tr);
            if st.n >= args.n_min_train && st.total_r > best_r {
                best_cfg = *g;
                best_r = st.total_r;
                best_stats = Some(st);
            }
        }
        if best_stats.is_none() {
            best_cfg = last_cfg; // нечего выбрать — катим предыдущий
            best_stats = Some(stats(&run(&bars, best_cfg, sig_train_from, t1, t0, args.fee, args.spread, args.funding, cost_unit, &mut dummy)));
        }
        last_cfg = best_cfg;
        let train_st = best_stats.unwrap();
        // --- OOS: выбранный + прод + вся сетка (плато) ---
        let mut chosen_counters = Counters::default();
        let oos_ch = run(&bars, best_cfg, e0, e1, e0 - warm, args.fee, args.spread, args.funding, cost_unit, &mut chosen_counters);
        let mut pc = Counters::default();
        let oos_pr = run(&bars, prod, e0, e1, e0 - warm, args.fee, args.spread, args.funding, cost_unit, &mut pc);
        // агрегат воронки по prod-OOS
        prod_counters.total += pc.total;
        prod_counters.blocked_filter += pc.blocked_filter;
        prod_counters.blocked_warm += pc.blocked_warm;
        prod_counters.blocked_loss_streak += pc.blocked_loss_streak;
        prod_counters.blocked_vol_spike += pc.blocked_vol_spike;
        prod_counters.blocked_volume += pc.blocked_volume;
        prod_counters.blocked_atr += pc.blocked_atr;
        prod_counters.blocked_ofi += pc.blocked_ofi;
        prod_counters.blocked_momentum += pc.blocked_momentum;
        prod_counters.blocked_roc += pc.blocked_roc;
        prod_counters.blocked_mfi += pc.blocked_mfi;
        prod_counters.blocked_trend += pc.blocked_trend;
        prod_counters.blocked_regime += pc.blocked_regime;
        prod_counters.blocked_time += pc.blocked_time;
        prod_counters.blocked_limit_day += pc.blocked_limit_day;
        prod_counters.blocked_cooldown += pc.blocked_cooldown;
        prod_counters.blocked_net_rr += pc.blocked_net_rr;
        prod_counters.blocked_edge += pc.blocked_edge;
        prod_counters.blocked_ev += pc.blocked_ev;
        prod_counters.passed += pc.passed;
        let oos_ch_st = stats(&oos_ch);
        let oos_pr_st = stats(&oos_pr);
        chosen_oos.extend(oos_ch);
        prod_oos.extend(oos_pr);
        for (gi, g) in grid.iter().enumerate() {
            let tr = run(&bars, *g, e0, e1, e0 - warm, args.fee, args.spread, args.funding, cost_unit, &mut dummy);
            grid_oos[gi].extend(tr);
        }
        let chosen_tag = cfg_tag(&best_cfg);
        per_fold.push(FoldRow {
            fold: k + 1,
            train: format!("{}-{}", t0, t1),
            test: format!("{}-{}", e0, e1),
            chosen: chosen_tag.clone(),
            train_stats: train_st,
            oos_chosen: oos_ch_st,
            oos_prod: oos_pr_st,
        });
        println!(
            "fold {:2}: train[{}:{}] -> {} | train {} | OOS* {} | OOSprod {}",
            k + 1,
            t0,
            t1,
            chosen_tag,
            fmt_st(&per_fold.last().unwrap().train_stats),
            fmt_st(&per_fold.last().unwrap().oos_chosen),
            fmt_st(&per_fold.last().unwrap().oos_prod)
        );
    }

    // ---------- агрегаты по прод-конфигу ----------
    let st_prod = stats(&prod_oos);
    let rs: Vec<f64> = prod_oos.iter().map(|t| t.net_r).collect();
    let avg_sl = if !prod_oos.is_empty() {
        py_sum(prod_oos.iter().map(|t| t.sl_dist / t.entry)) / prod_oos.len() as f64
    } else {
        0.0
    };
    let cost_r = if avg_sl != 0.0 { cost_unit / avg_sl } else { 0.0 };
    let be_wr = if !rs.is_empty() {
        (1.0 + cost_r) / (prod.rr + cost_r) * 100.0
    } else {
        0.0
    };
    let days = (bars[folds[folds.len() - 1] + train].ts - bars[folds[0] + train].ts) as f64
        / 86400000.0;
    let trades_per_day = if days != 0.0 {
        prod_oos.len() as f64 / days
    } else {
        0.0
    };

    let mut rng = Mt19937::new(seed);
    let mut boots: Vec<f64> = Vec::with_capacity(5000);
    for _ in 0..5000 {
        if rs.is_empty() {
            boots.push(0.0);
            continue;
        }
        let mut acc = 0.0f64;
        let mut c = 0.0f64;
        for _ in 0..rs.len() {
            let x = rs[rng.below(rs.len())];
            let t = acc + x;
            if acc.abs() >= x.abs() {
                c += (acc - t) + x;
            } else {
                c += (x - t) + acc;
            }
            acc = t;
        }
        boots.push(acc + c);
    }
    boots.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let ci_lo = boots[(0.025 * boots.len() as f64) as usize];
    let ci_hi = boots[((0.975 * boots.len() as f64) as usize) - 1];
    let mean_r = if !rs.is_empty() {
        st_prod.total_r / rs.len() as f64
    } else {
        0.0
    };
    let sd_r = if rs.len() > 1 {
        (py_sum(rs.iter().map(|x| (x - mean_r) * (x - mean_r))) / (rs.len() - 1) as f64).sqrt()
    } else {
        0.0
    };
    let sharpe = if sd_r > 0.0 && days > 0.0 {
        mean_r / sd_r * (rs.len() as f64 / days).sqrt()
    } else {
        0.0
    };

    println!();
    println!("=== PRODUCTION CONFIG OOS (fix, 1:1 с живым) ===");
    println!("{}", fmt_st(&st_prod));
    println!(
        "breakeven WR ~ {:.1}% (cost {:.2}R при avg SL {:.2}%)",
        be_wr,
        cost_r,
        avg_sl * 100.0
    );
    println!(
        "95% CI total R: [{:+.2}; {:+.2}] | sharpe(годовой, по сделкам) {:.2} | trades/day {:.2}",
        ci_lo, ci_hi, sharpe, trades_per_day
    );

    println!();
    println!("=== ФИЛЬТР-ВОРОНКА (prod, сумма по OOS-окнам) ===");
    println!(
        "bars(сигнальные)={} | passed={} | vol={} atr={} ofi={} mom={} roc={} mfi={} trend={} regime={} time={} lim={} cd={} rr={} edge={} ev={} vspk={} lstreak={} filt={} warm={}",
        prod_counters.total,
        prod_counters.passed,
        prod_counters.blocked_volume,
        prod_counters.blocked_atr,
        prod_counters.blocked_ofi,
        prod_counters.blocked_momentum,
        prod_counters.blocked_roc,
        prod_counters.blocked_mfi,
        prod_counters.blocked_trend,
        prod_counters.blocked_regime,
        prod_counters.blocked_time,
        prod_counters.blocked_limit_day,
        prod_counters.blocked_cooldown,
        prod_counters.blocked_net_rr,
        prod_counters.blocked_edge,
        prod_counters.blocked_ev,
        prod_counters.blocked_vol_spike,
        prod_counters.blocked_loss_streak,
        prod_counters.blocked_filter,
        prod_counters.blocked_warm,
    );

    println!();
    println!("=== ПЛАТО (все конфиги сетки, OOS целиком) ===");
    for (gi, g) in grid.iter().enumerate() {
        let marker = if gi == 0 { " <- prod" } else { "" };
        println!("{:<40} {}{}", cfg_tag(g), fmt_st(&stats(&grid_oos[gi])), marker);
    }

    // ---------- файлы ----------
    fs::create_dir_all(&args.out)
        .unwrap_or_else(|e| die(&format!("[!] не создать каталог {}: {}", args.out, e)));

    // per_fold.csv (CRLF — как csv.writer в Python)
    {
        let mut f = File::create(format!("{}/per_fold.csv", args.out))
            .unwrap_or_else(|e| die(&format!("[!] per_fold.csv: {}", e)));
        write!(f, "fold,train_bars,test_bars,chosen_cfg,train_n,train_total_r,train_wr,train_pf,oos_chosen_n,oos_chosen_r,oos_prod_n,oos_prod_r,oos_prod_wr,oos_prod_pf\r\n")
            .unwrap_or_else(|e| die(&format!("[!] per_fold.csv: {}", e)));
        for p in &per_fold {
            let train_pf = if p.train_stats.n > 0 {
                format!("{:.2}", p.train_stats.pf)
            } else {
                String::new()
            };
            let oos_prod_pf = if p.oos_prod.n > 0 {
                format!("{:.2}", p.oos_prod.pf)
            } else {
                String::new()
            };
            write!(
                f,
                "{},{},{},{},{},{:.3},{:.1},{},{},{:.3},{},{:.3},{:.1},{}\r\n",
                p.fold,
                p.train,
                p.test,
                p.chosen,
                p.train_stats.n,
                p.train_stats.total_r,
                p.train_stats.wr,
                train_pf,
                p.oos_chosen.n,
                p.oos_chosen.total_r,
                p.oos_prod.n,
                p.oos_prod.total_r,
                p.oos_prod.wr,
                oos_prod_pf
            )
            .unwrap_or_else(|e| die(&format!("[!] per_fold.csv: {}", e)));
        }
    }

    // oos_trades_production.csv
    {
        let mut f = File::create(format!("{}/oos_trades_production.csv", args.out))
            .unwrap_or_else(|e| die(&format!("[!] oos_trades_production.csv: {}", e)));
        write!(f, "ts_in,ts_out,side,entry,sl,tp,exit,why,sl_dist,net_r\r\n")
            .unwrap_or_else(|e| die(&format!("[!] oos_trades_production.csv: {}", e)));
        for t in &prod_oos {
            write!(
                f,
                "{},{},{},{},{},{},{},{},{:.6},{:.4}\r\n",
                t.ts_in,
                t.ts_out,
                if t.side == 1 { "long" } else { "short" },
                py_f64(t.entry),
                py_f64(t.sl),
                py_f64(t.tp),
                py_f64(t.exit),
                t.why,
                t.sl_dist,
                t.net_r
            )
            .unwrap_or_else(|e| die(&format!("[!] oos_trades_production.csv: {}", e)));
        }
    }

    // summary.json (layout как json.dump(indent=1) в Python + params)
    {
        let pf_json = if st_prod.pf.is_finite() {
            format!("{:?}", st_prod.pf)
        } else {
            String::from("Infinity")
        };
        let json = format!(
            "{{\n \"n\": {},\n \"wr\": {:?},\n \"total_r\": {:?},\n \"pf\": {},\n \"maxdd\": {:?},\n \"be_wr\": {:?},\n \"ci_lo\": {:?},\n \"ci_hi\": {:?},\n \"sharpe\": {:?},\n \"trades_per_day\": {:?},\n \"days\": {:?},\n \"cost_r\": {:?},\n \"avg_sl_pct\": {:?},\n \"meta\": {{\n  \"data\": \"{}\",\n  \"bars\": {},\n  \"fee\": {:?},\n  \"spread\": {:?},\n  \"funding\": {:?},\n  \"train\": {},\n  \"test\": {},\n  \"step\": {},\n  \"warm\": {},\n  \"seed\": {},\n  \"params\": {{\n   \"lookback\": {},\n   \"atr_period\": {},\n   \"ofi_window\": {},\n   \"min_atr\": {:?},\n   \"ofi\": {:?},\n   \"vol_anom\": {:?},\n   \"tp_atr_mult\": {:?},\n   \"min_tp\": {:?},\n   \"rr\": {:?},\n   \"max_per_day\": {},\n   \"regime_win\": {},\n   \"regime_vol\": {:?},\n   \"regime_trend\": {:?},\n   \"min_net_rr\": {:?}\n  }}\n }}\n}}",
            st_prod.n,
            st_prod.wr,
            st_prod.total_r,
            pf_json,
            st_prod.maxdd,
            be_wr,
            ci_lo,
            ci_hi,
            sharpe,
            trades_per_day,
            days,
            cost_r,
            avg_sl * 100.0,
            Path::new(&args.data)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| args.data.clone()),
            n,
            args.fee,
            args.spread,
            args.funding,
            args.train,
            args.test,
            args.step,
            args.warm,
            args.seed,
            prod.lookback,
            prod.atr_period,
            prod.ofi_window,
            prod.min_atr,
            prod.ofi_thr,
            prod.vol_anom,
            prod.tp_atr_mult,
            prod.min_tp,
            prod.rr,
            prod.max_per_day,
            prod.regime_win,
            prod.regime_vol,
            prod.regime_trend,
            prod.min_net_rr
        );
        let mut f = File::create(format!("{}/summary.json", args.out))
            .unwrap_or_else(|e| die(&format!("[!] summary.json: {}", e)));
        f.write_all(json.as_bytes())
            .unwrap_or_else(|e| die(&format!("[!] summary.json: {}", e)));
    }

    println!();
    println!(
        "saved: per_fold.csv, oos_trades_production.csv, summary.json -> {}",
        args.out
    );
}
