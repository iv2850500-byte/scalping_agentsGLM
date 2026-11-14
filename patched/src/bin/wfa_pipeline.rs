//! P64/: WFA-конвейер — прогон ИСТОРИИ через ЖИВУЮ MomentumStrategy (lib).
//!
//! Отличие от wfa_momentum.rs (P31/P36): там логика стратегии была
//! СКОПИРОВАНА в бин и неизбежно расходилась с живым кодом. Здесь
//! конвейер крутит реальный `MomentumStrategy::process()` из lib —
//! те же 17 фильтров, тот же SignalFilter, тот же loss-streak cooldown.
//! Внешние эффекты только через WFA_REPLAY_NOW_MS (реплей-часы) и
//! симулированные колбэки позиции (notify_position_open/closed) — как
//! их делает живой оркестратор.
//!
//! Воронка 17 фильтров снимается методом wfa_funnel() (P64) — счётчики
//! 1:1 со строкой diagnostics() живого бота.
//!
//! НЕ часть релиза: required-features = ["wfa"] (P44/rel) —
//! `cargo build --release` этот бин не собирает.
//!
//! Запуск:
//!   cargo run --features wfa --bin wfa_pipeline -- --data TAOUSDT-1h.csv --test 720 --step 720
//!
//! Отчёт: wfa_out/pipeline_report.txt + .json. Exit-коды: 0 = PASS,
//! 2 = FAIL (результат исследования, НЕ ошибка инструмента), 1 = ошибка.
//!
//! P66/: пороги входных фильтров env-изованы - MOM_ATR_MIN_PCT,
//! MOM_OFI_MIN, MOM_VOLUME_MIN (дефолты = живые дефолты config.toml:
//! 0.05 / 0.15 / 1.5, т.е. без env поведение идентично P64). Повторный
//! WFA с тюнингом из threshold_grid:
//!   $env:MOM_VOLUME_MIN="1.2"; $env:MOM_ATR_MIN_PCT="0.3"; $env:MOM_OFI_MIN="0.05"
//!   powershell -ExecutionPolicy Bypass -File .\wfa_pipeline.ps1 -Days 450

use std::io::Write as _;
use std::path::Path;

use scalping_agents::strategies::momentum::{MomentumStrategy, WFA_REPLAY_NOW_MS, WfaFunnel};
use scalping_agents::strategies::{Signal, Strategy};
use scalping_agents::websocket::{BookTickerMessage, KlineMessage, WebSocketMessage};

// P64/: фраза-помощник для die() — единый формат ошибки инструмента.
fn die(msg: &str) -> ! {
    eprintln!("[wfa] ERROR: {msg}");
    std::process::exit(1);
}

// P64/: параметры конвейера (CLI + cost-модель).
struct Args {
    data: String,
    warm: usize,
    test: usize,
    step: usize,
    fee: f64,
    spread: f64,
    funding: f64,
    balance: f64,
    risk_pct: f64,
}

fn usage() -> ! {
    eprintln!(
        "usage: wfa_pipeline --data <CSV> [--warm 250] [--test 720] [--step 720]\n\
         --data путь к 1h-свечам (формат P31: close_time,open,high,low,close,volume,taker_buy_volume)\n\
         --warm  баров прогрева (backfill, сигналы запрещены)\n\
         --test  баров теста в окне (720 = 30 дней на 1h)\n\
         --step  шаг окна (720 = окна стыкуются без перекрытия)\n\
         --fee 0.0005 --spread 0.0002 --funding 0.0001 --balance 200 --risk-pct 0.5"
    );
    std::process::exit(1);
}

fn parse_f64(args: &mut impl Iterator<Item = String>, flag: &str) -> f64 {
    args.next()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or_else(|| die(&format!("missing/invalid value for {flag}")))
}

fn parse_usize(args: &mut impl Iterator<Item = String>, flag: &str) -> usize {
    args.next()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(|| die(&format!("missing/invalid value for {flag}")))
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        data: String::new(),
        warm: 250,
        test: 720,
        step: 720,
        fee: 0.0005,
        spread: 0.0002,
        funding: 0.0001,
        balance: 200.0,
        risk_pct: 0.5,
    };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--data" => a.data = it.next().unwrap_or_else(|| die("--data required")),
            "--warm" => a.warm = parse_usize(&mut it, "--warm"),
            "--test" => a.test = parse_usize(&mut it, "--test"),
            "--step" => a.step = parse_usize(&mut it, "--step"),
            "--fee" => a.fee = parse_f64(&mut it, "--fee"),
            "--spread" => a.spread = parse_f64(&mut it, "--spread"),
            "--funding" => a.funding = parse_f64(&mut it, "--funding"),
            "--balance" => a.balance = parse_f64(&mut it, "--balance"),
            "--risk-pct" => a.risk_pct = parse_f64(&mut it, "--risk-pct"),
            "-h" | "--help" => usage(),
            other => die(&format!("unknown arg: {other}")),
        }
    }
    if a.data.is_empty() {
        usage();
    }
    if a.warm < 50 || a.test < 50 || a.step < 1 {
        die("--warm/--test >= 50, --step >= 1");
    }
    a
}

// P64/: бар истории в нейтральном формате (как в отчёте P31).
struct Bar {
    ts: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
    taker_buy: f64,
}

// P64/: загрузчик CSV. Формат P31: close_time,open,high,low,close,volume,taker_buy_volume.
// Заголовок опционален (строка без числа в 1-й колонке пропускается).
// Bybit-экспорт с taker=0 помечаем: OFI вырождается — печатаем warning.
fn load_bars(path: &str) -> (Vec<Bar>, bool) {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| die(&format!("cannot read {path}: {e}")));
    let mut bars = Vec::new();
    let mut taker_zero = 0usize;
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() < 7 {
            continue;
        }
        let ts: i64 = match cols[0].trim().parse() {
            Ok(v) => v,
            Err(_) => {
                if i == 0 {
                    continue; // заголовок
                }
                die(&format!("line {}: bad close_time '{}'", i + 1, cols[0]));
            }
        };
        let f = |s: &str| -> f64 {
            s.trim()
                .parse()
                .unwrap_or_else(|_| die(&format!("line {}: bad number '{s}'", i + 1)))
        };
        let (o, h, l, c, v, tb) =
            (f(cols[1]), f(cols[2]), f(cols[3]), f(cols[4]), f(cols[5]), f(cols[6]));
        if h < l || c <= 0.0 || o <= 0.0 {
            die(&format!("line {}: degenerate bar", i + 1));
        }
        if tb == 0.0 {
            taker_zero += 1;
        }
        bars.push(Bar { ts, open: o, high: h, low: l, close: c, volume: v, taker_buy: tb });
    }
    if bars.len() < 400 {
        die(&format!("too few bars: {} (need >= 400)", bars.len()));
    }
    let warn = taker_zero * 10 > bars.len();
    (bars, warn)
}

// P64/: одна сделка реплея.
#[derive(Clone)]
struct Trade {
    ts: i64,
    side: String,
    entry: f64,
    exit: f64,
    rr_gross: f64,
    net_r: f64,
    outcome: &'static str, // "tp" | "sl"
}

// P64/: результат окна (трейды + снимок воронки).
struct WindowResult {
    trades: Vec<Trade>,
    funnel: WfaFunnel,
    from_ts: i64,
    to_ts: i64,
    open_at_end: bool,
}

// P64/: параметры прогона окна.
struct ReplayParams {
    symbol: String,
    balance: f64,
    risk_pct: f64,
    fee: f64,
    spread: f64,
    funding: f64,
    cost_unit: f64, // 2*fee + spread + funding (доля от номинала, round-trip)
}

// P66/: парсер env -> f64 с дефолтом (мусор/отсутствие -> default).
fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(default)
}

// P66/: пороги входных фильтров. Дефолты = дефолты config.toml (mod.rs:
// min_atr_threshold 0.05, ofi_threshold 0.15, volume_anomaly_threshold
// 1.5) => без env раскладка воронки идентична P64. with_thresholds
// клампит: ofi в [0.02..0.95], volume >= 1.0, atr >= 0.
fn build_strategy(p: &ReplayParams) -> MomentumStrategy {
    // P23_MAX_SPREAD_PCT / P24_FUNDING_PCT стратегия читает из process env
    // сама (как живой бот); wfa_pipeline.ps1 импортирует их из .env.
    let atr_min_pct = env_f64("MOM_ATR_MIN_PCT", 0.05); // P66/
    let ofi_min = env_f64("MOM_OFI_MIN", 0.15); // P66/
    let volume_min = env_f64("MOM_VOLUME_MIN", 1.5); // P66/
    MomentumStrategy::new(p.symbol.clone(), p.balance, p.risk_pct)
        .with_thresholds(atr_min_pct, ofi_min, volume_min)
}

// P64/: синтетический bookTicker с фиксированным спредом вокруг close —
// живой фильтр спреда (P23) видит стабильную стоимость входа.
fn synthetic_bookticker(p: &ReplayParams, close: f64, ts: i64) -> BookTickerMessage {
    let half = close * p.spread / 2.0;
    BookTickerMessage {
        symbol: p.symbol.clone(),
        best_bid: close - half,
        best_ask: close + half,
        bid_qty: 1.0,
        ask_qty: 1.0,
        timestamp: ts,
    }
}

fn mk_kline(p: &ReplayParams, b: &Bar, backfill: bool) -> KlineMessage {
    KlineMessage {
        symbol: p.symbol.clone(),
        open: b.open,
        high: b.high,
        low: b.low,
        close: b.close,
        volume: b.volume,
        taker_buy_volume: b.taker_buy,
        timestamp: b.ts,
        is_closed: true,
        is_backfill: backfill,
    }
}

// P64/: одно окно walk-forward. from — первый бар ТЕСТА; warm баров перед
// ним — прогрев (backfill). Стратегия живёт внутри окна своя: воронка
// и счётчики не перетекают между окнами (как отдельные запуски бота).
fn run_window(bars: &[Bar], p: &ReplayParams, from: usize, warm: usize, test: usize) -> WindowResult {
    if from < warm || from + test > bars.len() {
        die("window out of range (internal)");
    }
    let mut st = build_strategy(p);
    // ---- прогрев: индикаторы строятся, сигналы запрещены (is_backfill) ----
    for b in &bars[from - warm..from] {
        WFA_REPLAY_NOW_MS.store(b.ts, std::sync::atomic::Ordering::Relaxed);
        let _ = st.process(&WebSocketMessage::BookTicker(synthetic_bookticker(p, b.close, b.ts)));
        let _ = st.process(&WebSocketMessage::Kline(mk_kline(p, b, true)));
    }
    // ---- тест: реплей-часы = время бара; сигналы разрешены ----
    let mut trades: Vec<Trade> = Vec::new();
    let mut open: Option<(Trade0, usize)> = None; // сделка + индекс бара входа
    for (i, b) in bars[from..from + test].iter().enumerate() {
        WFA_REPLAY_NOW_MS.store(b.ts, std::sync::atomic::Ordering::Relaxed);
        let _ = st.process(&WebSocketMessage::BookTicker(synthetic_bookticker(p, b.close, b.ts)));
        let sig: Option<Signal> = st
            .process(&WebSocketMessage::Kline(mk_kline(p, b, false)))
            .unwrap_or_else(|e| {
                eprintln!("[wfa] WARN: process() error at bar {i}: {e}");
                None
            });

        // ---- менеджмент открытой позиции: SL/TP по high/low текущего бара ----
        if let Some((t, entry_i)) = open.as_mut() {
            if i > *entry_i {
                let hit_sl = if t.side == "long" { b.low <= t.sl } else { b.high >= t.sl };
                let hit_tp = if t.side == "long" { b.high >= t.tp } else { b.low <= t.tp };
                // консервативно: если в одном баре и SL и TP — считаем SL
                if hit_sl {
                    close_trade(&mut st, &mut trades, t, if t.side == "long" { t.sl } else { t.sl }, "sl", p);
                    open = None;
                } else if hit_tp {
                    close_trade(&mut st, &mut trades, t, if t.side == "long" { t.tp } else { t.tp }, "tp", p);
                    open = None;
                }
            }
        }

        // ---- вход по сигналу (если плоско) ----
        if let Some(s) = sig {
            if open.is_some() {
                // сигнал при открытой позиции: фильтр живой стратегии
                // обычно сам блокирует (in_position) — тут просто игнор
            } else if s.stop_loss.is_none() || s.take_profit.is_none() || s.price <= 0.0 {
                // сигналы без рельсов SL/TP не торгуем
            } else {
                // P64/: стратегия эмитит BUY/SELL (как живой оркестратор)
                let side = match s.side.to_ascii_lowercase().as_str() {
                    "long" | "buy" => "long".to_string(),
                    "short" | "sell" => "short".to_string(),
                    other => {
                        eprintln!("[wfa] WARN: unknown side '{other}' - skip");
                        continue;
                    }
                };
                let sl = s.stop_loss.unwrap();
                let tp = s.take_profit.unwrap();
                let rf = if side == "long" {
                    (s.price - sl) / s.price
                } else {
                    (sl - s.price) / s.price
                };
                if rf <= 0.0 || !rf.is_finite() {
                    continue;
                }
                let rr = if side == "long" {
                    (tp - s.price) / s.price / rf
                } else {
                    (s.price - tp) / s.price / rf
                };
                st.notify_position_open();
                open = Some((
                    Trade0 { side, entry: s.price, sl, tp, rf, rr, ts: b.ts },
                    i,
                ));
            }
        }
    }
    // окно кончилось при открытой позиции — фиксируем как open_at_end (без PnL)
    let open_at_end = open.is_some();

    let to_ts = bars[from + test - 1].ts;
    let from_ts = bars[from].ts;
    let funnel = st.wfa_funnel();
    WFA_REPLAY_NOW_MS.store(0, std::sync::atomic::Ordering::Relaxed);
    WindowResult { trades, funnel, from_ts, to_ts, open_at_end }
}

// P64/: черновик сделки до закрытия.
struct Trade0 {
    side: String,
    entry: f64,
    sl: f64,
    tp: f64,
    rf: f64, // risk_frac: доля стопа от входа
    rr: f64, // gross RR
    ts: i64,
}

// P64/: закрытие сделки + прокидка результата в ЖИВУЮ стратегию
// (notify_position_closed -> record_trade_result -> loss-streak cooldown
// через реплей-часы). PnL передаётся как знак (стратегия использует
// только знак), net-R считается в cost-модели конвейера.
fn close_trade(st: &mut MomentumStrategy, out: &mut Vec<Trade>, t: &Trade0, exit: f64, outcome: &'static str, p: &ReplayParams) {
    let pnl = if t.side == "long" { exit - t.entry } else { t.entry - exit };
    st.notify_position_closed(pnl);
    let net_r = match outcome {
        "tp" => t.rr - p.cost_unit / t.rf,
        _ => -(1.0 + p.cost_unit / t.rf),
    };
    out.push(Trade {
        ts: t.ts,
        side: t.side.clone(),
        entry: t.entry,
        exit,
        rr_gross: t.rr,
        net_r,
        outcome,
    });
}

// P64/: агрегат по списку сделок.
struct St {
    n: usize,
    wr: f64,
    r: f64,
    pf: f64,
    max_dd_r: f64,
    avg: f64,
    avg_win: f64,
    avg_loss: f64,
    rr_mean: f64,
}

fn stats(trades: &[Trade]) -> St {
    let n = trades.len();
    let r: f64 = trades.iter().map(|t| t.net_r).sum();
    let wins: Vec<f64> = trades.iter().map(|t| t.net_r).filter(|x| *x > 0.0).collect();
    let losses: Vec<f64> = trades.iter().map(|t| t.net_r).filter(|x| *x <= 0.0).collect();
    let gross_win: f64 = wins.iter().sum();
    let gross_loss: f64 = losses.iter().map(|x| -x).sum();
    let wr = if n > 0 { wins.len() as f64 / n as f64 } else { 0.0 };
    let pf = if gross_loss > 0.0 { gross_win / gross_loss } else { f64::INFINITY };
    // maxDD по кривой накопленного R
    let mut eq = 0.0;
    let mut peak = 0.0;
    let mut dd = 0.0;
    for t in trades {
        eq += t.net_r;
        if eq > peak {
            peak = eq;
        }
        if peak - eq > dd {
            dd = peak - eq;
        }
    }
    let rr_mean = if n > 0 {
        trades.iter().map(|t| t.rr_gross).sum::<f64>() / n as f64
    } else {
        0.0
    };
    St {
        n,
        wr,
        r,
        pf,
        max_dd_r: dd,
        avg: if n > 0 { r / n as f64 } else { 0.0 },
        avg_win: if wins.is_empty() { 0.0 } else { gross_win / wins.len() as f64 },
        avg_loss: if losses.is_empty() { 0.0 } else { gross_loss / losses.len() as f64 },
        rr_mean,
    }
}

fn fmt_st(s: &St) -> String {
    format!(
        "n={} WR={:.1}% R={:+.2} PF={:.2} maxDD={:.2}R avg={:+.3}R",
        s.n, s.wr * 100.0, s.r, s.pf, s.max_dd_r, s.avg
    )
}

// P64/: строка воронки — имена строго в порядке WfaFunnel::labels().
fn funnel_line(f: &WfaFunnel) -> String {
    let v = f.values();
    let l = WfaFunnel::labels();
    let mut s = String::new();
    for (i, name) in l.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{}={}", name, v[i]));
    }
    s.push_str(&format!(" passed={}", f.passed));
    s
}

fn iso_ts(ms: i64) -> String {
    // без chrono в бине: UTC-дата из epoch-мс (конвейер исследовательский,
    // точность до минут достаточна)
    let secs = ms / 1000;
    let days = secs / 86400;
    let (y, mo, d) = civil_from_days(days);
    let rem = secs % 86400;
    format!("{y:04}-{mo:02}-{d:02} {:02}:{:02}", rem / 3600, (rem % 3600) / 60)
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn main() {
    let args = parse_args();
    let (bars, taker_warn) = load_bars(&args.data);
    let p = ReplayParams {
        symbol: "TAOUSDT".to_string(),
        balance: args.balance,
        risk_pct: args.risk_pct,
        fee: args.fee,
        spread: args.spread,
        funding: args.funding,
        cost_unit: 2.0 * args.fee + args.spread + args.funding,
    };
    if taker_warn {
        eprintln!("[wfa] WARN: >90% баров с taker_buy_volume=0 — OFI вырожден (Bybit-экспорт?), фильтр OFI ведёт себя не как в живом боте");
    }

    if bars.len() < args.warm + args.test {
        die(&format!(
            "bars {} < warm+test {}",
            bars.len(),
            args.warm + args.test
        ));
    }
    let n_windows = (bars.len() - args.warm) / args.step;
    if n_windows < 2 {
        die(&format!("only {n_windows} walk-forward windows - увеличьте данные или уменьшите --step"));
    }

    // ---- self-check детерминизма: окно 0 дважды, результаты совпадают ----
    let w0a = run_window(&bars, &p, args.warm, args.warm, args.test);
    let w0b = run_window(&bars, &p, args.warm, args.warm, args.test);
    let same = w0a.trades.len() == w0b.trades.len()
        && (w0a.trades.iter().map(|t| t.net_r).sum::<f64>()
            - w0b.trades.iter().map(|t| t.net_r).sum::<f64>()).abs() < 1e-9
        && w0a.funnel.values() == w0b.funnel.values();
    let det_line = if same {
        "self-check детерминизм: OK".to_string()
    } else {
        "self-check детерминизм: MISMATCH!".to_string()
    };

    // ---- окна walk-forward: тестовые диапазоны стыкуются шагом --step;
    //      окно 0 уже прогнано дважды (self-check) — results[0] = w0a ----
    let mut results: Vec<WindowResult> = vec![w0a];
    let mut from = args.warm + args.step;
    while from + args.test <= bars.len() {
        results.push(run_window(&bars, &p, from, args.warm, args.test));
        from += args.step;
    }
    let _ = w0b; // участие в сравнении выше

    let all_trades: Vec<Trade> = results.iter().flat_map(|w| w.trades.iter().cloned()).collect();
    let total = stats(&all_trades);

    // воронка: сумма по окнам (WfaFunnel: Copy, поля pub)
    let mut ft = WfaFunnel::default();
    for w in &results {
        ft.total_closed_candles += w.funnel.total_closed_candles;
        ft.passed += w.funnel.passed;
        let v = w.funnel.values();
        ft.volume += v[0]; ft.atr += v[1]; ft.momentum += v[2]; ft.ofi += v[3];
        ft.regime += v[4]; ft.time += v[5]; ft.limit_day += v[6]; ft.cooldown += v[7];
        ft.net_rr += v[8]; ft.spread += v[9]; ft.edge += v[10]; ft.ema200 += v[11];
        ft.vol_spike += v[12]; ft.loss_streak += v[13]; ft.supertrend += v[14];
        ft.mfi += v[15]; ft.roc += v[16];
    }

    // ---- гейты (как в отчёте P64) ----
    let be = if total.rr_mean > 0.0 { 1.0 / (1.0 + total.rr_mean) } else { 1.0 };
    let mut gates: Vec<(String, bool)> = Vec::new();
    gates.push(("n>=20".into(), total.n >= 20));
    gates.push(("total R > 0".into(), total.r > 0.0));
    gates.push((format!("WR > breakeven + 3п.п. (WR={:.1}% vs be={:.1}%)", total.wr * 100.0, be * 100.0), total.wr > be + 0.03));
    let win_pos = results.iter().filter(|w| w.trades.iter().map(|t| t.net_r).sum::<f64>() > 0.0).count();
    gates.push((format!(">=50% окон OOS с R>0 ({}/{})", win_pos, results.len()), results.len() > 0 && win_pos * 2 >= results.len()));
    // свежесть: последняя треть сделок (по времени входа) суммарно R>0
    let third = all_trades.len() / 3;
    let fresh: f64 = if third == 0 { 0.0 } else { all_trades[all_trades.len() - third..].iter().map(|t| t.net_r).sum() };
    gates.push((format!("свежесть: R>0 в последней трети (R={:+.2} (n={}))", fresh, third), fresh > 0.0));
    let passed_gates = gates.iter().filter(|(_, ok)| *ok).count();

    // ---- TOP-блокеры: доля среди всех блокировок ----
    let blocked_sum: u64 = ft.values().iter().sum();
    let mut block_pairs: Vec<(u64, &str)> = ft.values().iter().cloned().zip(WfaFunnel::labels().iter().cloned()).collect();
    block_pairs.sort_by(|a, b| b.0.cmp(&a.0));
    let mut top = String::new();
    for (i, (v, name)) in block_pairs.iter().take(3).enumerate() {
        if *v == 0 {
            break;
        }
        if i > 0 {
            top.push_str(", ");
        }
        top.push_str(&format!(
            "{name}={v} ({:.1}%)",
            if blocked_sum > 0 { *v as f64 * 100.0 / blocked_sum as f64 } else { 0.0 }
        ));
    }

    // ---- отчёт txt ----
    let mut rpt = String::new();
    rpt.push_str("[P64] WFA pipeline — ЖИВАЯ MomentumStrategy (lib), воронка 17 фильтров\n");
    rpt.push_str(&format!(
        "[P64] data = {} (bars={})\n",
        args.data,
        bars.len()
    ));
    rpt.push_str(&format!(
        "[P64] warm={} test={} step={} окон={} | fee={:.4} spread={:.4} funding={:.4} cost_unit={:.4} | balance={} risk={}%\n",
        args.warm, args.test, args.step, results.len(), args.fee, args.spread, args.funding, p.cost_unit, args.balance, args.risk_pct
    ));
    // P66/: пороги в отчёт (аудируемость: какая раскладка воронки)
    rpt.push_str(&format!(
        "[P66] thresholds: atr_min_pct={:.3} ofi_min={:.3} volume_min={:.3} (env MOM_ATR_MIN_PCT / MOM_OFI_MIN / MOM_VOLUME_MIN)\n",
        env_f64("MOM_ATR_MIN_PCT", 0.05),
        env_f64("MOM_OFI_MIN", 0.15),
        env_f64("MOM_VOLUME_MIN", 1.5)
    ));
    rpt.push_str(&det_line);
    rpt.push('\n');
    for (i, w) in results.iter().enumerate() {
        let s = stats(&w.trades);
        rpt.push_str(&format!(
            "window {:>2}: {} .. {} | {} | open_at_end={} | funnel: {}\n",
            i,
            iso_ts(w.from_ts),
            iso_ts(w.to_ts),
            fmt_st(&s),
            w.open_at_end as u8,
            funnel_line(&w.funnel)
        ));
    }
    rpt.push_str(&format!("\nTOTAL: {} | RR_gross_mean={:.2}\n", fmt_st(&total), total.rr_mean));
    rpt.push_str(&format!("FUNNEL TOTAL: {} | closed={}\n", funnel_line(&ft), ft.total_closed_candles));
    rpt.push_str(&format!("TOP-блокеры: {}\n", if top.is_empty() { "-" } else { &top }));
    rpt.push('\n');
    for (name, ok) in &gates {
        rpt.push_str(&format!("[{}] {}\n", if *ok { "PASS" } else { "FAIL" }, name));
    }
    rpt.push_str(&format!(
        "\nVERDICT: {} ({}/{} гейтов)\n",
        if passed_gates == gates.len() { "PASS" } else { "FAIL" },
        passed_gates,
        gates.len()
    ));
    rpt.push_str("Примечание: bootstrap-CI и сетка плато — см. wfa_momentum (P31); гейт P64 консервативнее без CI.\n");
    rpt.push_str("Примечание: BE=1.5R / chandelier-трейл / Timeout не моделируются (как в P31); BE-гейт по среднему gross-RR.\n");
    rpt.push_str("Примечание: costs: net_r = gross_r - cost_unit/risk_frac (SL-сторона: -(1 + cost/rf)); SL приоритетнее TP в одном баре.\n");

    // ---- JSON-отчёт ----
    let mut js = String::from("{\n");
    js.push_str(&format!("  \"data\": {}, \"bars\": {},\n", json_str(&args.data), bars.len()));
    js.push_str(&format!(
        "  \"params\": {{\"warm\": {}, \"test\": {}, \"step\": {}, \"fee\": {}, \"spread\": {}, \"funding\": {}, \"cost_unit\": {:.6}, \"balance\": {}, \"risk_pct\": {}}},\n",
        args.warm, args.test, args.step, args.fee, args.spread, args.funding, p.cost_unit, args.balance, args.risk_pct
    ));
    js.push_str(&format!(
        "  \"thresholds\": {{\"atr_min_pct\": {}, \"ofi_min\": {}, \"volume_min\": {}}},\n",
        env_f64("MOM_ATR_MIN_PCT", 0.05),
        env_f64("MOM_OFI_MIN", 0.15),
        env_f64("MOM_VOLUME_MIN", 1.5)
    )); // P66/
    js.push_str("  \"windows\": [\n");
    for (i, w) in results.iter().enumerate() {
        let s = stats(&w.trades);
        let f = w.funnel.values();
        js.push_str(&format!(
            "    {{\"idx\": {}, \"from_ts\": {}, \"to_ts\": {}, \"n\": {}, \"wr\": {:.4}, \"r\": {:.4}, \"pf\": {:.4}, \"max_dd_r\": {:.4}, \"open_at_end\": {}, \"funnel\": [{}], \"passed\": {}}},\n",
            i, w.from_ts, w.to_ts, s.n, s.wr, s.r, if s.pf.is_infinite() { 999.0 } else { s.pf }, s.max_dd_r, w.open_at_end as u8,
            f.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", "),
            w.funnel.passed
        ));
    }
    js.push_str("  ],\n");
    js.push_str(&format!(
        "  \"total\": {{\"n\": {}, \"wr\": {:.4}, \"r\": {:.4}, \"pf\": {:.4}, \"max_dd_r\": {:.4}, \"rr_mean\": {:.4}, \"be_wr\": {:.4}}},\n",
        total.n, total.wr, total.r, if total.pf.is_infinite() { 999.0 } else { total.pf }, total.max_dd_r, total.rr_mean, be
    ));
    js.push_str(&format!("  \"funnel_total\": [{}], \"passed_total\": {}, \"closed\": {},\n",
        ft.values().iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", "), ft.passed, ft.total_closed_candles));
    js.push_str("  \"gates\": [\n");
    for (name, ok) in &gates {
        js.push_str(&format!("    {{\"name\": {}, \"pass\": {}}},\n", json_str(name), *ok));
    }
    js.push_str(&format!("  ],\n  \"verdict\": {}, \"gates_passed\": {}\n}}\n",
        json_str(if passed_gates == gates.len() { "PASS" } else { "FAIL" }), passed_gates));

    // ---- печать + файлы wfa_out/ ----
    print!("{rpt}");
    let _ = std::io::stdout().flush();
    let out_dir = Path::new("wfa_out");
    let _ = std::fs::create_dir_all(out_dir);
    let _ = std::fs::write(out_dir.join("pipeline_report.txt"), &rpt);
    let _ = std::fs::write(out_dir.join("pipeline_report.json"), &js);

    // hygiene: реплей-часы не должны утекать в живой режим
    WFA_REPLAY_NOW_MS.store(0, std::sync::atomic::Ordering::Relaxed);

    if passed_gates == gates.len() {
        std::process::exit(0);
    } else {
        std::process::exit(2);
    }
}

fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}
