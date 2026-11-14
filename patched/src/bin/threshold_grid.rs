//! P66/: threshold_grid — сетка порогов volume/atr/ofi по воронке WFA.
//!
//! Мотив (воронка P64 на TAOUSDT): volume режет ~62-84% баров, atr-фильтр
//! мёртвый (0 блокировок при min_atr 0.05%), ofi блокирует ~26%. Сетка
//! перебирает пороги входных фильтров через ЖИВУЮ MomentumStrategy (lib,
//! builder with_thresholds) на полной истории и ранжирует раскладки по
//! net-R с cost-моделью конвейера. Живой бот берёт пороги из config.toml
//! (strategy.params: min_atr_threshold / ofi_threshold /
//! volume_anomaly_threshold) — победителя сетки валидируем повторным
//! walk-forward (wfa_pipeline с MOM_* env) и только потом правим
//! config.toml.
//!
//! НЕ часть релиза: required-features = ["wfa"] (P44/rel).
//!
//! Ограничения (честно):
//!   - это ONE-SHOT перебор на всей истории (in-sample): финальный отбор
//!     ТОЛЬКО через повторный WFA (окна walk-forward, гейты P64);
//!   - builder клампит: ofi в [0.02..0.95], volume >= 1.0, atr >= 0.
//!
//! Запуск (обычно через wfa_grid.ps1):
//!   cargo run --features wfa --bin threshold_grid -- --data TAOUSDT-1h.csv
//! Отчёт: wfa_out/grid_report.txt + .json. Exit: 0 = ок, 1 = ошибка.

use std::io::Write as _;
use std::path::Path;

use scalping_agents::strategies::momentum::{MomentumStrategy, WFA_REPLAY_NOW_MS, WfaFunnel};
use scalping_agents::strategies::{Signal, Strategy};
use scalping_agents::websocket::{BookTickerMessage, KlineMessage, WebSocketMessage};

// P66/: параметры сетки.
struct Args {
    data: String,
    warm: usize,
    vol: Vec<f64>, // volume_anomaly_threshold (>= 1.0)
    atr: Vec<f64>, // min_atr_threshold_pct (в ПРОЦЕНТАХ, как в config.toml)
    ofi: Vec<f64>, // ofi_threshold (|OFI|, кламп 0.02..0.95)
    fee: f64,
    spread: f64,
    funding: f64,
    balance: f64,
    risk_pct: f64,
    min_n: usize, // порог сделок для пометки "ok"
    top: usize,   // сколько лучших печатать подробно
}

fn die(msg: &str) -> ! {
    eprintln!("[grid] ERROR: {msg}");
    std::process::exit(1);
}

fn usage() -> ! {
    eprintln!(
        "usage: threshold_grid --data <CSV> [--warm 250] [--min-n 12] [--top 3]\n\
         --vol  1.0,1.1,1.2,1.3,1.5     (volume_anomaly_threshold)\n\
         --atr  0.05,0.15,0.3,0.5,0.8   (min_atr_threshold, %)\n\
         --ofi  0.02,0.05,0.1,0.15,0.25 (ofi_threshold, |OFI|)\n\
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

// P66/: "1.0,1.2,1.5" -> Vec<f64> (пустое/мусор -> die).
fn parse_list(args: &mut impl Iterator<Item = String>, flag: &str) -> Vec<f64> {
    let raw = args.next().unwrap_or_else(|| die(&format!("missing value for {flag}")));
    let mut out = Vec::new();
    for tok in raw.split(',') {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        match t.parse::<f64>() {
            Ok(v) if v.is_finite() => out.push(v),
            _ => die(&format!("bad {flag} item '{t}'")),
        }
    }
    if out.is_empty() {
        die(&format!("empty list for {flag}"));
    }
    out
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        data: String::new(),
        warm: 250,
        vol: vec![1.0, 1.1, 1.2, 1.3, 1.5],
        atr: vec![0.05, 0.15, 0.3, 0.5, 0.8],
        ofi: vec![0.02, 0.05, 0.1, 0.15, 0.25],
        fee: 0.0005,
        spread: 0.0002,
        funding: 0.0001,
        balance: 200.0,
        risk_pct: 0.5,
        min_n: 12,
        top: 3,
    };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--data" => a.data = it.next().unwrap_or_else(|| die("--data required")),
            "--warm" => a.warm = parse_usize(&mut it, "--warm"),
            "--vol" => a.vol = parse_list(&mut it, "--vol"),
            "--atr" => a.atr = parse_list(&mut it, "--atr"),
            "--ofi" => a.ofi = parse_list(&mut it, "--ofi"),
            "--fee" => a.fee = parse_f64(&mut it, "--fee"),
            "--spread" => a.spread = parse_f64(&mut it, "--spread"),
            "--funding" => a.funding = parse_f64(&mut it, "--funding"),
            "--balance" => a.balance = parse_f64(&mut it, "--balance"),
            "--risk-pct" => a.risk_pct = parse_f64(&mut it, "--risk-pct"),
            "--min-n" => a.min_n = parse_usize(&mut it, "--min-n"),
            "--top" => a.top = parse_usize(&mut it, "--top"),
            "-h" | "--help" => usage(),
            other => die(&format!("unknown arg: {other}")),
        }
    }
    if a.data.is_empty() {
        usage();
    }
    if a.warm < 250 {
        die("--warm >= 250 (EMA200 + lookback должны прогреться)");
    }
    if a.top == 0 {
        a.top = 1;
    }
    a
}

// P66/: бар истории (формат P31: close_time,open,high,low,close,volume,taker_buy_volume).
struct Bar {
    ts: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
    taker_buy: f64,
}

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

// P66/: одна сделка (как в wfa_pipeline).
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

struct Trade0 {
    side: String,
    entry: f64,
    sl: f64,
    tp: f64,
    rf: f64,
    rr: f64,
    ts: i64,
}

// P66/: параметры прогона (cost-модель идентична wfa_pipeline).
struct ReplayParams {
    symbol: String,
    balance: f64,
    risk_pct: f64,
    fee: f64,
    spread: f64,
    funding: f64,
    cost_unit: f64,
}

// P66/: пороги одного combos (уже клампнутые).
#[derive(Clone, Copy, PartialEq)]
struct Th {
    vol: f64,
    atr_pct: f64,
    ofi: f64,
}

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

// P66/: один прогон полной истории с заданными порогами. Живая стратегия,
// fresh state на каждый combos; сигналы -> входы плоско, SL/TP по high/low
// следующих баров (SL приоритетнее в одном баре, как в конвейере P64).
fn run_pass(bars: &[Bar], p: &ReplayParams, th: Th, warm: usize) -> (Vec<Trade>, WfaFunnel) {
    let mut st = MomentumStrategy::new(p.symbol.clone(), p.balance, p.risk_pct)
        .with_thresholds(th.atr_pct, th.ofi, th.vol); // P66/: (atr, ofi, volume)

    for b in &bars[..warm] {
        WFA_REPLAY_NOW_MS.store(b.ts, std::sync::atomic::Ordering::Relaxed);
        let _ = st.process(&WebSocketMessage::BookTicker(synthetic_bookticker(p, b.close, b.ts)));
        let _ = st.process(&WebSocketMessage::Kline(mk_kline(p, b, true)));
    }

    let mut trades: Vec<Trade> = Vec::new();
    let mut open: Option<(Trade0, usize)> = None;
    for (i, b) in bars[warm..].iter().enumerate() {
        WFA_REPLAY_NOW_MS.store(b.ts, std::sync::atomic::Ordering::Relaxed);
        let _ = st.process(&WebSocketMessage::BookTicker(synthetic_bookticker(p, b.close, b.ts)));
        let sig: Option<Signal> = st
            .process(&WebSocketMessage::Kline(mk_kline(p, b, false)))
            .unwrap_or(None);

        if let Some((t, entry_i)) = open.as_mut() {
            if i > *entry_i {
                let hit_sl = if t.side == "long" { b.low <= t.sl } else { b.high >= t.sl };
                let hit_tp = if t.side == "long" { b.high >= t.tp } else { b.low <= t.tp };
                if hit_sl {
                    close_trade(&mut st, &mut trades, t, t.sl, "sl", p);
                    open = None;
                } else if hit_tp {
                    close_trade(&mut st, &mut trades, t, t.tp, "tp", p);
                    open = None;
                }
            }
        }

        if let Some(s) = sig {
            if open.is_some() {
                // уже в позиции — игнор (фильтр in_position живой стратегии)
            } else if s.stop_loss.is_none() || s.take_profit.is_none() || s.price <= 0.0 {
                // без рельсов не торгуем
            } else {
                let side = match s.side.to_ascii_lowercase().as_str() {
                    "long" | "buy" => "long".to_string(),
                    "short" | "sell" => "short".to_string(),
                    _ => continue,
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

    let funnel = st.wfa_funnel();
    WFA_REPLAY_NOW_MS.store(0, std::sync::atomic::Ordering::Relaxed);
    (trades, funnel)
}

fn close_trade(
    st: &mut MomentumStrategy,
    out: &mut Vec<Trade>,
    t: &Trade0,
    exit: f64,
    outcome: &'static str,
    p: &ReplayParams,
) {
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

// P66/: агрегат по сделкам (как stats в wfa_pipeline).
struct St {
    n: usize,
    wr: f64,
    r: f64,
    pf: f64,
    rr_mean: f64,
    be: f64,
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
    let rr_mean = if n > 0 {
        trades.iter().map(|t| t.rr_gross).sum::<f64>() / n as f64
    } else {
        0.0
    };
    let be = if rr_mean > 0.0 { 1.0 / (1.0 + rr_mean) } else { 1.0 };
    St { n, wr, r, pf, rr_mean, be }
}

// P66/: строка результата одного combos.
#[derive(Clone)]
struct Row {
    th: Th,
    n: usize,
    wr: f64,
    r: f64,
    pf: f64,
    passed: u64,
    vol_blocks: u64,
    blocks_total: u64,
    ok: bool,
}

fn fmt_row(row: &Row) -> String {
    format!(
        "vol={:.2} atr%={:.2} ofi={:.2} | n={} WR={:.1}% R={:+.2} PF={} passed={} vol_blk={} ({:.0}% blk) {}",
        row.th.vol,
        row.th.atr_pct,
        row.th.ofi,
        row.n,
        row.wr * 100.0,
        row.r,
        if row.pf.is_infinite() { "inf".to_string() } else { format!("{:.2}", row.pf) },
        row.passed,
        row.vol_blocks,
        if row.blocks_total > 0 { row.vol_blocks as f64 * 100.0 / row.blocks_total as f64 } else { 0.0 },
        if row.ok { "[ok]" } else { "" }
    )
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
        eprintln!("[grid] WARN: >90% баров с taker_buy_volume=0 — OFI вырожден (Bybit-экспорт?), сетка по ofi ведёт себя не как в живом боте");
    }
    if bars.len() < args.warm + 50 {
        die(&format!("bars {} < warm+50 {}", bars.len(), args.warm));
    }

    // P66/: клампы как в with_thresholds + дедуп.
    let clamp = |v: f64, a: f64, o: f64| Th { vol: v.max(1.0), atr_pct: a.max(0.0), ofi: o.clamp(0.02, 0.95) };
    let mut combos: Vec<Th> = Vec::new();
    for &v in &args.vol {
        for &a in &args.atr {
            for &o in &args.ofi {
                let th = clamp(v, a, o);
                if !combos.contains(&th) {
                    combos.push(th);
                }
            }
        }
    }

    eprintln!(
        "[grid] data={} bars={} warm={} combos={} (vol={} atr={} ofi={})",
        args.data,
        bars.len(),
        args.warm,
        combos.len(),
        args.vol.len(),
        args.atr.len(),
        args.ofi.len()
    );

    let mut rows: Vec<Row> = Vec::with_capacity(combos.len());
    for (i, &th) in combos.iter().enumerate() {
        let (trades, funnel) = run_pass(&bars, &p, th, args.warm);
        let s = stats(&trades);
        let v = funnel.values();
        let blocks_total: u64 = v.iter().sum();
        rows.push(Row {
            th,
            n: s.n,
            wr: s.wr,
            r: s.r,
            pf: s.pf,
            passed: funnel.passed,
            vol_blocks: v[0],
            blocks_total,
            ok: s.n >= args.min_n && s.r > 0.0 && s.wr > s.be + 0.03,
        });
        if (i + 1) % 20 == 0 || i + 1 == combos.len() {
            eprintln!("[grid] {}/{} done", i + 1, combos.len());
        }
    }

    // P66/: сортировка по R убыв. (ok-строки вперёд при равном R нет — R и есть критерий).
    rows.sort_by(|a, b| b.r.partial_cmp(&a.r).unwrap_or(std::cmp::Ordering::Equal));

    let ok_count = rows.iter().filter(|r| r.ok).count();
    let baseline = rows
        .iter()
        .find(|r| (r.th.vol - 1.5).abs() < 1e-9 && (r.th.atr_pct - 0.05).abs() < 1e-9 && (r.th.ofi - 0.15).abs() < 1e-9);

    // ---- отчёт ----
    let mut rpt = String::new();
    rpt.push_str("[P66] threshold_grid — сетка volume/atr/ofi через ЖИВУЮ MomentumStrategy (lib)\n");
    rpt.push_str(&format!(
        "[P66] data = {} (bars={}, warm={})\n",
        args.data,
        bars.len(),
        args.warm
    ));
    rpt.push_str(&format!(
        "[P66] cost: fee={:.4} spread={:.4} funding={:.4} cost_unit={:.4} | balance={} risk={}%",
        args.fee, args.spread, args.funding, p.cost_unit, args.balance, args.risk_pct
    ));
    rpt.push('\n');
    rpt.push_str(&format!(
        "[P66] ok-критерий: n>={} И R>0 И WR>be+3п.п. | ok-строк: {}/{}\n\n",
        args.min_n,
        ok_count,
        rows.len()
    ));
    rpt.push_str("TOP по R (in-sample!):\n");
    for row in rows.iter().take(args.top.max(10)) {
        rpt.push_str(&format!("  {}\n", fmt_row(row)));
    }
    if let Some(b) = baseline {
        rpt.push_str(&format!("\nБАЗА (дефолты config.toml): {}\n", fmt_row(b)));
    }
    rpt.push_str("\nВСЕ СТРОКИ (vol asc, atr asc, ofi asc):\n");
    let mut sorted_all = rows.clone();
    sorted_all.sort_by(|a, b| {
        a.th.vol.partial_cmp(&b.th.vol).unwrap_or(std::cmp::Ordering::Equal)
            .then(a.th.atr_pct.partial_cmp(&b.th.atr_pct).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.th.ofi.partial_cmp(&b.th.ofi).unwrap_or(std::cmp::Ordering::Equal))
    });
    for row in &sorted_all {
        rpt.push_str(&format!("  {}\n", fmt_row(row)));
    }
    rpt.push_str(
        "\nВАЖНО: сетка in-sample (один прогон всей истории) — отсекает шум poorly.\n",
    );
    rpt.push_str("Финальный отбор: повторный walk-forward через wfa_pipeline с MOM_* env\n");
    rpt.push_str("победителя, гейты P64 (n>=20, WR>be+3п.п., >=50% окон R>0, свежесть). Только\n");
    rpt.push_str("после PASS правим config.toml (strategy.params: volume_anomaly_threshold /\n");
    rpt.push_str("min_atr_threshold / ofi_threshold). Пример:\n");
    if let Some(w) = rows.iter().find(|r| r.ok) {
        rpt.push_str(&format!(
            "  $env:MOM_VOLUME_MIN=\"{:.2}\"; $env:MOM_ATR_MIN_PCT=\"{:.2}\"; $env:MOM_OFI_MIN=\"{:.2}\"\n",
            w.th.vol, w.th.atr_pct, w.th.ofi
        ));
        rpt.push_str("  powershell -ExecutionPolicy Bypass -File .\\wfa_pipeline.ps1 -Days 450\n");
    } else {
        rpt.push_str("  (ok-строк нет — оставить дефолты; тюнинг порогов не подтверждён данными)\n");
    }

    // ---- JSON ----
    let mut js = String::from("{\n");
    js.push_str(&format!("  \"data\": {}, \"bars\": {}, \"warm\": {},\n", json_str(&args.data), bars.len(), args.warm));
    js.push_str(&format!(
        "  \"params\": {{\"fee\": {}, \"spread\": {}, \"funding\": {}, \"cost_unit\": {:.6}, \"balance\": {}, \"risk_pct\": {}, \"min_n\": {}}},\n",
        args.fee, args.spread, args.funding, p.cost_unit, args.balance, args.risk_pct, args.min_n
    ));
    js.push_str("  \"rows\": [\n");
    for (i, row) in rows.iter().enumerate() {
        js.push_str(&format!(
            "    {{\"vol\": {:.3}, \"atr_pct\": {:.3}, \"ofi\": {:.3}, \"n\": {}, \"wr\": {:.4}, \"r\": {:.4}, \"pf\": {:.4}, \"passed\": {}, \"vol_blocks\": {}, \"ok\": {}}}{}\n",
            row.th.vol,
            row.th.atr_pct,
            row.th.ofi,
            row.n,
            row.wr,
            row.r,
            if row.pf.is_infinite() { 999.0 } else { row.pf },
            row.passed,
            row.vol_blocks,
            row.ok as u8,
            if i + 1 == rows.len() { "" } else { "," }
        ));
    }
    js.push_str(&format!("  ],\n  \"ok_rows\": {}, \"rows_total\": {}\n}}\n", ok_count, rows.len()));

    print!("{rpt}");
    let _ = std::io::stdout().flush();
    let out_dir = Path::new("wfa_out");
    let _ = std::fs::create_dir_all(out_dir);
    let _ = std::fs::write(out_dir.join("grid_report.txt"), &rpt);
    let _ = std::fs::write(out_dir.join("grid_report.json"), &js);

    WFA_REPLAY_NOW_MS.store(0, std::sync::atomic::Ordering::Relaxed);
    std::process::exit(0);
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
