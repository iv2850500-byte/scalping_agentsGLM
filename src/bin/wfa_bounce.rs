//! P30 v2: Walk-Forward Analysis BounceTrader (BTCUSDT 1h) — Rust-порт пакета P28.
//!
//! P30-ИЗМЕНЕНИЕ (только метки, логика и числа не тронуты): в chosen_tag и в
//! таблицу ПЛАТО добавлен 4-й параметр сетки search (se1.2%/se2.0%). Раньше
//! 16 конфигов печатались 8 уникальными метками — search 0.012/0.020 не
//! отображался, строки плато выглядели дублями.
//! Порт bounce_trader.rs (P24-редакция) 1:1: RSI Wilder 14, фракталы 5/5 (строгие),
//! буфер 0.05%, пол SL 0.3%, поиск уровня 1.2%, rr=2.0, net_rr с комиссией/спредом/
//! funding, flat-фильтр, SignalFilter (одна позиция). Исполнение: вход по open
//! следующей свечи, SL проверяется раньше TP внутри свечи (пессимистично),
//! гэки исполняются по open.
//!
//! Только std (без новых зависимостей). Запуск:
//!   cargo run --release --bin wfa_bounce
//!   cargo run --release --bin wfa_bounce -- --data klines/BTCUSDT-1h-full.csv --fee 0.0006
//!
//! Сверено с wfa_bounce.py (P28): сделки, все метрики И bootstrap-CI 1:1 (MT19937
//! совместим с CPython random.Random, сумма — как sum() в Python 3.12+).

use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::exit;

// ---------- константы стратегии (bounce_trader.rs) ----------
const FRACTAL_LB: usize = 5;
const MAX_FRACTALS: usize = 20;
const LEVEL_BUFFER: f64 = 0.0005;
const MIN_SL_PCT: f64 = 0.003;
const MAX_HISTORY: usize = 200;

fn die(msg: &str) -> ! {
    eprintln!("{}", msg);
    exit(1);
}

// формат числа как repr() в Python: 1.0 -> "1.0", 1.5 -> "1.5", 1.2 -> "1.2"
fn py_f64(v: f64) -> String {
    if v == v.trunc() {
        format!("{:.1}", v)
    } else {
        format!("{}", v)
    }
}

// сумма как в Python 3.12+: sum() для float использует компенсированную сумму
// Ноймайера (gh-100425); наивный фолд в Rust даёт расхождение в последнем бите
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
}

fn usage() {
    println!("P30 v2 WFA BounceTrader (Rust, только std) — метки конфигов включают search");
    println!("Запуск: cargo run --release --bin wfa_bounce [-- параметры]");
    println!("  --data ПУТЬ    CSV клайнов 1h (default: BTCUSDT-1h-full.csv, fallback klines/)");
    println!("  --out КАТАЛОГ  результаты (default: wfa_out)");
    println!("  --fee F        taker на сторону, доля (default 0.0005)");
    println!("  --spread F     спред полный круг, доля (default 0.0002)");
    println!("  --funding F    funding один интервал 8h, доля (default 0.0001)");
    println!("  --train N      train-окно, баров (default 2160 = 90 дней на 1h)");
    println!("  --test N       test-окно OOS, баров (default 720 = 30 дней)");
    println!("  --step N       шаг карусели, баров (default 720 = 30 дней)");
    println!("  --warm N       прогрев индикаторов, баров (default 250)");
    println!("  --seed N       seed bootstrap-CI (default 42)");
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
        data: String::from("BTCUSDT-1h-full.csv"),
        out: String::from("wfa_out"),
        fee: 0.0005,
        spread: 0.0002,
        funding: 0.0001,
        train: 2160,
        test: 720,
        step: 720,
        warm: 250,
        seed: 42,
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
            other => die(&format!(
                "[!] неизвестный аргумент '{}', см. --help",
                other
            )),
        }
    }
    // fallback: если дефолтного файла нет, смотрим klines/ рядом с запуском
    if !Path::new(&a.data).exists() && a.data == "BTCUSDT-1h-full.csv" {
        let alt = Path::new("klines").join("BTCUSDT-1h-full.csv");
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
        if cols.len() < 5 {
            die(&format!("[!] строка {}: ожидалось >= 5 колонок", i + 1));
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
        });
    }
    bars.sort_by_key(|b| b.ts);
    bars
}

// ---------- индикаторы ----------
struct RsiWilder {
    p: usize,
    prev: Option<f64>,
    avg_g: f64,
    avg_l: f64,
    sg: f64,
    sl: f64,
    count: usize,
    value: f64,
}

impl RsiWilder {
    fn new(p: usize) -> Self {
        RsiWilder {
            p,
            prev: None,
            avg_g: 0.0,
            avg_l: 0.0,
            sg: 0.0,
            sl: 0.0,
            count: 0,
            value: 50.0,
        }
    }

    fn next(&mut self, close: f64) -> f64 {
        let prev = match self.prev {
            None => {
                self.prev = Some(close);
                return self.value;
            }
            Some(v) => v,
        };
        let d = close - prev;
        self.prev = Some(close);
        let g = d.max(0.0);
        let l = (-d).max(0.0);
        self.count += 1;
        if self.count <= self.p {
            self.sg += g;
            self.sl += l;
            if self.count == self.p {
                self.avg_g = self.sg / self.p as f64;
                self.avg_l = self.sl / self.p as f64;
                let rs = if self.avg_l == 0.0 {
                    f64::INFINITY
                } else {
                    self.avg_g / self.avg_l
                };
                self.value = 100.0 - 100.0 / (1.0 + rs);
            }
        } else {
            let pf = self.p as f64;
            self.avg_g = (self.avg_g * (pf - 1.0) + g) / pf;
            self.avg_l = (self.avg_l * (pf - 1.0) + l) / pf;
            let rs = if self.avg_l == 0.0 {
                f64::INFINITY
            } else {
                self.avg_g / self.avg_l
            };
            self.value = 100.0 - 100.0 / (1.0 + rs);
        }
        self.value
    }
}

// ---------- конфиги ----------
#[derive(Clone, Copy)]
struct Cfg {
    rr: f64,
    min_rr: f64,
    rsi_lo: f64,
    rsi_hi: f64,
    trend_thr: f64,
    trend_win: usize,
    search: f64,
}

fn prod_cfg() -> Cfg {
    Cfg {
        rr: 2.0,
        min_rr: 1.2,
        rsi_lo: 30.0,
        rsi_hi: 70.0,
        trend_thr: 1.5,
        trend_win: 12,
        search: 0.012,
    }
}

fn build_grid() -> Vec<Cfg> {
    let mut grid = Vec::new();
    for &(lo, hi) in &[(30.0f64, 70.0f64), (25.0, 75.0)] {
        for &tt in &[1.5f64, 1.0] {
            for &mr in &[1.2f64, 1.5] {
                for &se in &[0.012f64, 0.020] {
                    let mut g = prod_cfg();
                    g.rsi_lo = lo;
                    g.rsi_hi = hi;
                    g.trend_thr = tt;
                    g.min_rr = mr;
                    g.search = se;
                    grid.push(g);
                }
            }
        }
    }
    grid
}

// ---------- симулятор стратегии ----------
#[derive(Clone, Copy)]
struct Pos {
    side: &'static str,
    entry: f64,
    sl: f64,
    tp: f64,
    sl_dist: f64,
    ts_in: i64,
}

#[derive(Clone, Copy)]
struct Pend {
    side: &'static str,
    sl: f64,
    tp: f64,
    sl_dist: f64,
}

#[derive(Clone)]
struct Trade {
    side: &'static str,
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

fn run(bars: &[Bar], cfg: Cfg, sig_from: usize, sig_to: usize, warmup_from: usize, cost_unit: f64) -> Vec<Trade> {
    let mut highs: Vec<f64> = Vec::new();
    let mut lows: Vec<f64> = Vec::new();
    let mut closes: Vec<f64> = Vec::new();
    let mut rsi = RsiWilder::new(14);
    let mut fh: Vec<f64> = Vec::new(); // фрактальные уровни (цены)
    let mut fl: Vec<f64> = Vec::new();
    #[allow(unused_assignments)] // 50.0 — зеркало Python-инициализации, перезаписывается на первом баре
    let mut rsi_val: f64 = 50.0;
    let mut trades: Vec<Trade> = Vec::new();
    let mut pos: Option<Pos> = None;
    let mut pending: Option<Pend> = None;
    let mut just_closed = false;

    let end = sig_to.min(bars.len());
    for i in warmup_from..end {
        let b = &bars[i];
        let (ts, o, h, l, c) = (b.ts, b.o, b.h, b.l, b.c);
        let in_sig = sig_from <= i && i < sig_to;

        // --- 1) исполнение ожидающего входа по open этой свечи ---
        if pending.is_some() && pos.is_none() {
            let p = pending.take().unwrap();
            pos = Some(Pos {
                side: p.side,
                entry: o, // вход по open (без look-ahead); та же свеча может дать выход
                sl: p.sl,
                tp: p.tp,
                sl_dist: p.sl_dist,
                ts_in: ts,
            });
        }

        // --- 2) проверки выхода на этой свече ---
        if let Some(p) = pos {
            let mut exit_px = f64::NAN;
            let mut why = "";
            let mut hit = false;
            if p.side == "long" {
                if o <= p.sl {
                    exit_px = o; why = "gap_sl"; hit = true;
                } else if o >= p.tp {
                    exit_px = o; why = "gap_tp"; hit = true;
                } else if l <= p.sl {
                    exit_px = p.sl; why = "sl"; hit = true;
                } else if h >= p.tp {
                    exit_px = p.tp; why = "tp"; hit = true;
                }
            } else {
                if o >= p.sl {
                    exit_px = o; why = "gap_sl"; hit = true;
                } else if o <= p.tp {
                    exit_px = o; why = "gap_tp"; hit = true;
                } else if h >= p.sl {
                    exit_px = p.sl; why = "sl"; hit = true;
                } else if l <= p.tp {
                    exit_px = p.tp; why = "tp"; hit = true;
                }
            }
            if hit {
                let d = if p.side == "long" { 1.0 } else { -1.0 };
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
                pos = None;
                just_closed = true;
            }
        }

        // --- 3) обновление индикаторов (как push_candle в Rust) ---
        highs.push(h);
        lows.push(l);
        closes.push(c);
        if highs.len() > MAX_HISTORY {
            highs.remove(0);
            lows.remove(0);
            closes.remove(0);
        }
        rsi_val = rsi.next(c);
        let n = highs.len();
        if n >= FRACTAL_LB * 2 + 1 {
            let ci = n - 1 - FRACTAL_LB;
            let hc = highs[ci];
            let lc = lows[ci];
            let mut up = true;
            for j in (ci - FRACTAL_LB)..=(ci + FRACTAL_LB) {
                if j != ci && !(hc > highs[j]) {
                    up = false;
                    break;
                }
            }
            if up {
                fh.push(hc);
                if fh.len() > MAX_FRACTALS {
                    fh.remove(0);
                }
            }
            let mut dn = true;
            for j in (ci - FRACTAL_LB)..=(ci + FRACTAL_LB) {
                if j != ci && !(lc < lows[j]) {
                    dn = false;
                    break;
                }
            }
            if dn {
                fl.push(lc);
                if fl.len() > MAX_FRACTALS {
                    fl.remove(0);
                }
            }
        }

        // --- 4) генерация сигнала (только в сигнальном окне) ---
        if !in_sig || pos.is_some() || pending.is_some() {
            continue;
        }
        if just_closed {
            just_closed = false; // cooldown 120с истёк к следующей свече (1h)
        }
        if closes.len() < cfg.trend_win + 1 {
            continue;
        }
        let recent = closes.len() - 1;
        let past = closes.len() - 1 - cfg.trend_win;
        let mom = (closes[recent] - closes[past]).abs() / closes[past] * 100.0;
        if mom >= cfg.trend_thr {
            continue;
        }
        let price = c;

        if rsi_val < cfg.rsi_lo {
            let mut best = 0.0f64;
            let mut bd = f64::INFINITY;
            let mut found = false;
            for &lv in &fl {
                let dist = (price - lv).abs() / price;
                if dist <= cfg.search && dist < bd {
                    bd = dist;
                    best = lv;
                    found = true;
                }
            }
            if !found {
                continue;
            }
            let buffer = price * LEVEL_BUFFER;
            let sd = ((price - best) + buffer).max(price * MIN_SL_PCT);
            let np_ = cfg.rr * sd - price * cost_unit;
            let nl_ = sd + price * cost_unit;
            if nl_ <= 0.0 || np_ <= 0.0 || np_ / nl_ < cfg.min_rr {
                continue;
            }
            pending = Some(Pend {
                side: "long",
                sl: price - sd,
                tp: price + sd * cfg.rr,
                sl_dist: sd,
            });
        } else if rsi_val > cfg.rsi_hi {
            let mut best = 0.0f64;
            let mut bd = f64::INFINITY;
            let mut found = false;
            for &lv in &fh {
                let dist = (price - lv).abs() / price;
                if dist <= cfg.search && dist < bd {
                    bd = dist;
                    best = lv;
                    found = true;
                }
            }
            if !found {
                continue;
            }
            let buffer = price * LEVEL_BUFFER;
            let sd = ((best - price) + buffer).max(price * MIN_SL_PCT);
            let np_ = cfg.rr * sd - price * cost_unit;
            let nl_ = sd + price * cost_unit;
            if nl_ <= 0.0 || np_ <= 0.0 || np_ / nl_ < cfg.min_rr {
                continue;
            }
            pending = Some(Pend {
                side: "short",
                sl: price + sd,
                tp: price - sd * cfg.rr,
                sl_dist: sd,
            });
        }
    }
    trades
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

    // как random_seed в CPython: init_by_array с 32-битными словами числа (LE)
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

    // getrandbits(k) для k<=32: как в CPython
    fn getrandbits(&mut self, k: u32) -> u32 {
        self.genrand_u32() >> (32 - k)
    }

    // _randbelow_with_getrandbits(n): выбор из [0, n) — как random.choice
    fn below(&mut self, n: usize) -> usize {
        let n = n as u32;
        let k = 32 - n.leading_zeros(); // bit_length
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
            "[!] файл данных не найден: {}\n    положи BTCUSDT-1h-full.csv в корень проекта или укажи: cargo run --release --bin wfa_bounce -- --data ПУТЬ",
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

    println!("[P30] data = {}", args.data);
    println!(
        "[P30] bars={} | fee={} spread={} funding={} | bootstrap seed={}",
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

    let grid = build_grid();
    let prod = prod_cfg();
    let mut per_fold: Vec<FoldRow> = Vec::new();
    let mut chosen_oos: Vec<Trade> = Vec::new();
    let mut prod_oos: Vec<Trade> = Vec::new();
    let mut grid_oos: Vec<Vec<Trade>> = vec![Vec::new(); grid.len()];
    let mut last_cfg = prod;

    for (k, &s0) in folds.iter().enumerate() {
        let (t0, t1, e0, e1) = (s0, s0 + train, s0 + train, s0 + train + test);
        let sig_train_from = if s0 == 0 { t0 + warm } else { t0 };
        // --- train: выбор конфига ---
        let mut best_cfg = last_cfg;
        let mut best_r = f64::NEG_INFINITY;
        let mut best_stats: Option<St> = None;
        for g in &grid {
            let tr = run(&bars, *g, sig_train_from, t1, t0, cost_unit);
            let st = stats(&tr);
            if st.n >= 8 && st.total_r > best_r {
                best_cfg = *g;
                best_r = st.total_r;
                best_stats = Some(st);
            }
        }
        if best_stats.is_none() {
            best_cfg = last_cfg; // нечего выбрать — катим предыдущий
            best_stats = Some(stats(&run(&bars, best_cfg, sig_train_from, t1, t0, cost_unit)));
        }
        last_cfg = best_cfg;
        let train_st = best_stats.unwrap();
        // --- OOS: выбранный + прод + вся сетка (плато) ---
        let oos_ch = run(&bars, best_cfg, e0, e1, e0 - warm, cost_unit);
        let oos_pr = run(&bars, prod, e0, e1, e0 - warm, cost_unit);
        let oos_ch_st = stats(&oos_ch);
        let oos_pr_st = stats(&oos_pr);
        chosen_oos.extend(oos_ch);
        prod_oos.extend(oos_pr);
        for (gi, g) in grid.iter().enumerate() {
            let tr = run(&bars, *g, e0, e1, e0 - warm, cost_unit);
            grid_oos[gi].extend(tr);
        }
        let chosen_tag = format!(
            "rsi{:.0}/{:.0} tr{} rr{} se{:.1}",
            best_cfg.rsi_lo,
            best_cfg.rsi_hi,
            py_f64(best_cfg.trend_thr),
            py_f64(best_cfg.min_rr),
            best_cfg.search * 100.0
        );
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
        // sum(rng.choice(rs) for _ in rs) — выбор и сумма как в Python
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
    boots.sort_by(f64::total_cmp); // ✅ P41/rel: NaN в бутстрэпе больше не паникует (был partial_cmp().unwrap())
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
    println!("=== ПЛАТО (все конфиги сетки, OOS целиком) ===");
    for (gi, g) in grid.iter().enumerate() {
        let tag = format!(
            "rsi{:.0}/{:.0} trend<{}% minRR {} se{:.1}%",
            g.rsi_lo,
            g.rsi_hi,
            py_f64(g.trend_thr),
            py_f64(g.min_rr),
            g.search * 100.0
        );
        println!("{:<44} {}", tag, fmt_st(&stats(&grid_oos[gi])));
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
                t.side,
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

    // summary.json (layout как json.dump(indent=1) в Python)
    {
        let pf_json = if st_prod.pf.is_finite() {
            format!("{:?}", st_prod.pf)
        } else {
            String::from("Infinity")
        };
        let json = format!(
            "{{\n \"n\": {},\n \"wr\": {:?},\n \"total_r\": {:?},\n \"pf\": {},\n \"maxdd\": {:?},\n \"be_wr\": {:?},\n \"ci_lo\": {:?},\n \"ci_hi\": {:?},\n \"sharpe\": {:?},\n \"trades_per_day\": {:?},\n \"days\": {:?},\n \"cost_r\": {:?},\n \"avg_sl_pct\": {:?},\n \"meta\": {{\n  \"data\": \"{}\",\n  \"bars\": {},\n  \"fee\": {:?},\n  \"spread\": {:?},\n  \"funding\": {:?},\n  \"train\": {},\n  \"test\": {},\n  \"step\": {},\n  \"warm\": {},\n  \"seed\": {}\n }}\n}}",
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
            args.seed
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
