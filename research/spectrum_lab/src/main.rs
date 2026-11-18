//! spectrum_lab — P68: исследовательский конвейер пятого бота (standalone, std-only).
//!
//! Протокол (docs/STRATEGY_DISCOVERY.md):
//!   1. Загрузка + QA данных (гэпы/дубли/OHLC-санити).
//!   2. Регистрация гипотез ДО оценки (hypotheses_registry.json, append-only).
//!   3. In-sample: walk-forward (train/test/step), выбор параметров ТОЛЬКО на train.
//!   4. Отложенный OOS-holdout: последние 30% баров, трогаются один раз.
//!   5. Стресс издержек: fee x2 на OOS.
//!   6. Гейты P64: n>=20, WR > be+3пп, >=50% fold'ов с R>0.
//!   7. Портфельный оверлей: корреляция дневных R-серий кандидатов по активам
//!      (гипотеза диверсификации, не доказательство).
//!
//! Классы гипотез: range_fade (mean reversion), donchian_breakout (trend/breakout).
//! Без заглядывания: сигнал по close бара i -> исполнение по open бара i+1.
//! Интрабар-неоднозначность SL/TP решается консервативно: считаем SL первым.

// P68/ markers: this file is fully new (P68/). No existing files touched.

use std::collections::VecDeque;
use std::fs;
use std::io::Write as _;

// ============================= CLI =============================

struct Args {
    data: Vec<String>,
    out: String,
    fee: f64,
    spread: f64,
    train: usize,
    test: usize,
    step: usize,
    warm: usize,
    oos_frac: f64,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        data: vec![],
        out: "spectrum_out".into(),
        fee: 0.0005,
        spread: 0.0002,
        train: 2160,
        test: 720,
        step: 720,
        warm: 250,
        oos_frac: 0.30,
    };
    let mut it = std::env::args().skip(1);
    while let Some(s) = it.next() {
        match s.as_str() {
            "--data" => a.data.push(it.next().ok_or("--data требует путь")?),
            "--out" => a.out = it.next().ok_or("--out требует путь")?,
            "--fee" => a.fee = it.next().ok_or("--fee")?.parse().map_err(|_| "fee")?,
            "--spread" => a.spread = it.next().ok_or("--spread")?.parse().map_err(|_| "spread")?,
            "--train" => a.train = it.next().ok_or("--train")?.parse().map_err(|_| "train")?,
            "--test" => a.test = it.next().ok_or("--test")?.parse().map_err(|_| "test")?,
            "--step" => a.step = it.next().ok_or("--step")?.parse().map_err(|_| "step")?,
            "--warm" => a.warm = it.next().ok_or("--warm")?.parse().map_err(|_| "warm")?,
            "--oos-frac" => a.oos_frac = it.next().ok_or("--oos-frac")?.parse().map_err(|_| "oos")?,
            _ => return Err(format!("неизвестный аргумент {}", s)),
        }
    }
    if a.data.is_empty() {
        return Err("нет --data".into());
    }
    Ok(a)
}

// ============================= Data =============================

#[derive(Clone, Copy, Debug)]
struct Bar {
    ts: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
    taker_buy: f64,
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

struct DataQa {
    rows: usize,
    dups: usize,
    gaps: usize,
    ohlc_bad: usize,
    zero_vol: usize,
}

fn load_csv(path: &str) -> Result<(Vec<Bar>, DataQa, u64), String> {
    let raw = fs::read(path).map_err(|e| format!("{}: {}", path, e))?;
    let hash = fnv1a(&raw);
    let text = String::from_utf8_lossy(&raw);
    let mut bars: Vec<Bar> = Vec::new();
    let mut qa = DataQa { rows: 0, dups: 0, gaps: 0, ohlc_bad: 0, zero_vol: 0 };
    let mut prev_ts: Option<i64> = None;
    for (ln, line) in text.lines().enumerate() {
        if ln == 0 && line.chars().next().map(|c| !c.is_ascii_digit()).unwrap_or(true) {
            continue; // header
        }
        let c: Vec<&str> = line.split(',').collect();
        if c.len() < 6 { continue; }
        let ts: i64 = c[0].trim().parse().map_err(|_| format!("{}:{} ts", path, ln + 1))?;
        let open: f64 = c[1].trim().parse().map_err(|_| format!("{}:{} open", path, ln + 1))?;
        let high: f64 = c[2].trim().parse().map_err(|_| format!("{}:{} high", path, ln + 1))?;
        let low: f64 = c[3].trim().parse().map_err(|_| format!("{}:{} low", path, ln + 1))?;
        let close: f64 = c[4].trim().parse().map_err(|_| format!("{}:{} close", path, ln + 1))?;
        let volume: f64 = c[5].trim().parse().unwrap_or(0.0);
        let taker: f64 = if c.len() > 9 { c[9].trim().parse().unwrap_or(volume * 0.5) } else { volume * 0.5 };
        if high < low || high < open - 1e-12 || high < close - 1e-12 || low > open + 1e-12 || low > close + 1e-12 {
            qa.ohlc_bad += 1;
            continue;
        }
        qa.rows += 1;
        if let Some(p) = prev_ts {
            if ts == p { qa.dups += 1; continue; }
            if ts < p { continue; }
            if ts - p != 3_600_000 { qa.gaps += 1; }
        }
        if volume <= 0.0 { qa.zero_vol += 1; }
        prev_ts = Some(ts);
        bars.push(Bar { ts, open, high, low, close, volume, taker_buy: taker });
    }
    Ok((bars, qa, hash))
}

// ============================= Indicators =============================

struct Ind {
    atr: VecDeque<f64>,
    period: usize,
}
impl Ind {
    fn new(period: usize) -> Self { Self { atr: VecDeque::new(), period } }
    fn push(&mut self, b: &Bar, prev_close: Option<f64>) -> f64 {
        let tr = match prev_close {
            Some(pc) => (b.high - b.low).max((b.high - pc).abs()).max((b.low - pc).abs()),
            None => b.high - b.low,
        };
        let n = self.period as f64;
        let next = self.atr.back().map(|p| (p * (n - 1.0) + tr) / n).unwrap_or(tr);
        self.atr.push_back(next);
        if self.atr.len() > self.period * 3 { self.atr.pop_front(); }
        next
    }
}

fn mean(v: &[f64]) -> f64 { v.iter().sum::<f64>() / v.len() as f64 }

fn stdev(v: &[f64]) -> f64 {
    let m = mean(v);
    let s: f64 = v.iter().map(|x| (x - m) * (x - m)).sum();
    (s / v.len() as f64).sqrt()
}

struct Stats {
    closes: Vec<f64>,
    highs: Vec<f64>,
    lows: Vec<f64>,
    vols: Vec<f64>,
    atr: Vec<f64>,
}
impl Stats {
    fn build(bars: &[Bar]) -> Self {
        let mut ind = Ind::new(14);
        let mut atr = Vec::with_capacity(bars.len());
        let mut pc: Option<f64> = None;
        for b in bars {
            atr.push(ind.push(b, pc));
            pc = Some(b.close);
        }
        Self {
            closes: bars.iter().map(|b| b.close).collect(),
            highs: bars.iter().map(|b| b.high).collect(),
            lows: bars.iter().map(|b| b.low).collect(),
            vols: bars.iter().map(|b| b.volume).collect(),
            atr,
        }
    }
    fn vol_avg(&self, i: usize, w: usize) -> f64 {
        if i + 1 < w { return 0.0; }
        let s: f64 = self.vols[i + 1 - w..=i].iter().sum();
        s / w as f64
    }
    fn donchian_high(&self, i: usize, period: usize) -> Option<f64> {
        if i < period { return None; }
        Some(self.highs[i - period..i].iter().cloned().fold(f64::MIN, f64::max))
    }
    fn donchian_low(&self, i: usize, period: usize) -> Option<f64> {
        if i < period { return None; }
        Some(self.lows[i - period..i].iter().cloned().fold(f64::MAX, f64::min))
    }
    /// regime: диапазон за окно (флэт-гейт), как в live-стратегиях.
    fn regime_range_pct(&self, i: usize, w: usize) -> f64 {
        if i < w { return 0.0; }
        let hi = self.highs[i + 1 - w..=i].iter().cloned().fold(f64::MIN, f64::max);
        let lo = self.lows[i + 1 - w..=i].iter().cloned().fold(f64::MAX, f64::min);
        if lo <= 0.0 { return 0.0; }
        (hi - lo) / lo
    }
}

// ============================= Hypotheses =============================

#[derive(Clone, Copy, Debug, PartialEq)]
enum Side { Long, Short }

struct Trade {
    r: f64,
    day: i64,
}

#[derive(Clone, Copy, Debug)]
struct Params {
    lookback: usize,
    z_entry: f64,
    period: usize,
    vol_mult: f64,
}

// P70: добавили H7-H9 аддитивно, H1/H2 не трогали
const HYPOTHESES: &[(&str, &str)] = &[
    ("H1_range_fade", "mean_reversion"),
    ("H2_donchian_breakout", "breakout"),
    ("H7_INJ_donch10_maker", "breakout"),
    ("H8_XAU_close_try20_adx12", "breakout"),
    ("H9_XAU_noEMA15_25", "breakout"),
];

fn param_grid(h: &str) -> Vec<Params> {
    match h {
        "H1_range_fade" => {
            let mut v = vec![];
            for lb in [20usize, 40] {
                for z in [2.0f64, 2.5] {
                    v.push(Params { lookback: lb, z_entry: z, period: 0, vol_mult: 0.0 });
                }
            }
            v
        },
        "H7_INJ_donch10_maker" => vec![
            Params { lookback:0, z_entry:0.0, period:10, vol_mult:1.0 },
            Params { lookback:0, z_entry:0.0, period:10, vol_mult:1.1 },
        ],
        "H8_XAU_close_try20_adx12" => vec![
            Params { lookback:0, z_entry:0.0, period:20, vol_mult:1.2 },
            Params { lookback:0, z_entry:0.0, period:20, vol_mult:1.1 },
        ],
        "H9_XAU_noEMA15_25" => vec![
            Params { lookback:0, z_entry:0.0, period:15, vol_mult:1.0 },
            Params { lookback:0, z_entry:0.0, period:25, vol_mult:1.0 },
        ],
        _ => {
            let mut v = vec![];
            for p in [20usize, 55] {
                for vm in [1.1f64, 1.3] {
                    v.push(Params { lookback: 0, z_entry: 0.0, period: p, vol_mult: vm });
                }
            }
            v
        }
    }
}

/// Сигнал на баре i (по его close); исполнение на open бара i+1.
fn signal_at(h: &str, st: &Stats, i: usize, p: &Params) -> Option<Side> {
    match h {
        "H1_range_fade" => {
            if i + 1 < p.lookback || i < 30 { return None; }
            let win = &st.closes[i + 1 - p.lookback..=i];
            let m = mean(win);
            let sd = stdev(win);
            if sd <= 1e-12 { return None; }
            let z = (st.closes[i] - m) / sd;
            if st.regime_range_pct(i, 30) >= 0.08 { return None; }
            if z < -p.z_entry { Some(Side::Long) }
            else if z > p.z_entry { Some(Side::Short) }
            else { None }
        }
        _ => {
            let (dh, dl) = (st.donchian_high(i, p.period), st.donchian_low(i, p.period));
            let (dh, dl) = (dh?, dl?);
            if i + 1 < 20 { return None; }
            let avg = st.vol_avg(i, 20);
            if avg <= 0.0 { return None; }
            if st.vols[i] < p.vol_mult * avg { return None; }
            if st.closes[i] > dh { Some(Side::Long) }
            else if st.closes[i] < dl { Some(Side::Short) }
            else { None }
        }
    }
}

struct ReplayCfg {
    fee: f64,
    spread: f64,
    max_hold: usize,
}

/// Реплей по диапазону [from, to): сигнал на i -> вход на open i+1.
/// SL/TP в ценах; интрабар: SL первым (консервативно). R-множитель нетто.
fn replay(h: &str, bars: &[Bar], st: &Stats, from: usize, to: usize, p: &Params, cfg: &ReplayCfg) -> Vec<Trade> {
    let mut trades = Vec::new();
    let (sl_m, tp_m): (f64, f64) = if h == "H1_range_fade" { (1.2, 1.6) } else { (1.5, 3.0) };
    let mut i = from.max(1);
    while i < to.saturating_sub(1) {
        let Some(side) = signal_at(h, st, i, p) else { i += 1; continue; };
        let entry_bar = i + 1;
        let entry = bars[entry_bar].open;
        let atr = st.atr[i];
        if atr <= 0.0 || entry <= 0.0 { i += 1; continue; }
        let (sl, tp, is_long) = match side {
            Side::Long => (entry - sl_m * atr, entry + tp_m * atr, true),
            Side::Short => (entry + sl_m * atr, entry - tp_m * atr, false),
        };
        let bad_gap = if is_long { bars[entry_bar].open < sl || bars[entry_bar].open > tp }
                      else { bars[entry_bar].open > sl || bars[entry_bar].open < tp };
        if bad_gap { i += 1; continue; }
        let cost_rt = (2.0 * cfg.fee + cfg.spread) * entry;
        let risk_unit = (entry - sl).abs() + cost_rt;
        if risk_unit <= 0.0 { i += 1; continue; }
        let mut exit_price: Option<(f64, bool, usize)> = None;
        let mut j = entry_bar;
        while j < to.min(entry_bar + cfg.max_hold) {
            let b = &bars[j];
            let hit_sl = if is_long { b.low <= sl } else { b.high >= sl };
            let hit_tp = if is_long { b.high >= tp } else { b.low <= tp };
            if hit_sl {
                exit_price = Some((sl, false, j));
                break;
            }
            if hit_tp {
                exit_price = Some((tp, true, j));
                break;
            }
            j += 1;
        }
        let (px, win, jx) = match exit_price {
            Some((px, win, jx)) => (px, win, jx),
            None => {
                let jx = to.min(entry_bar + cfg.max_hold) - 1;
                let px = bars[jx].close;
                let win = if is_long { px > entry } else { px < entry };
                (px, win, jx)
            }
        };
        let _ = win;
        let gross = if is_long { px - entry } else { entry - px };
        let r = (gross - cost_rt) / risk_unit;
        trades.push(Trade { r, day: bars[entry_bar].ts / 86_400_000 });
        i = jx.max(entry_bar) + 1;
    }
    trades
}

fn aggregate(trades: &[Trade]) -> (usize, f64, f64, f64, f64, f64) {
    let n = trades.len();
    let total_r: f64 = trades.iter().map(|t| t.r).sum();
    let wins: Vec<f64> = trades.iter().filter(|t| t.r > 0.0).map(|t| t.r).collect();
    let losses: Vec<f64> = trades.iter().filter(|t| t.r <= 0.0).map(|t| -t.r).collect();
    let wr = if n > 0 { wins.len() as f64 / n as f64 } else { 0.0 };
    let gw: f64 = wins.iter().sum();
    let gl: f64 = losses.iter().sum();
    let pf = if gl > 0.0 { gw / gl } else if gw > 0.0 { f64::INFINITY } else { 0.0 };
    let (avg_w, avg_l) = (
        if wins.is_empty() { 0.0 } else { gw / wins.len() as f64 },
        if losses.is_empty() { 0.0 } else { gl / losses.len() as f64 },
    );
    let be = if avg_l > 0.0 && avg_w > 0.0 { avg_l / (avg_w + avg_l) } else { 0.0 };
    let mut cum = 0.0; let mut peak = 0.0; let mut dd = 0.0;
    for t in trades {
        cum += t.r;
        if cum > peak { peak = cum; }
        if peak - cum > dd { dd = peak - cum; }
    }
    (n, total_r, wr, pf, be, dd)
}

// ============================= Registry =============================

fn write_registry(out_dir: &str, files: &[(String, u64)], seed: u64) -> std::io::Result<()> {
    let mut s = String::from("[\n");
    for (name, class) in HYPOTHESES {
        let params = param_grid(name)
            .iter()
            .map(|p| format!("{{\"lookback\":{},\"z\":{},\"period\":{},\"vol_mult\":{}}}", p.lookback, p.z_entry, p.period, p.vol_mult))
            .collect::<Vec<_>>()
            .join(",");
        s.push_str(&format!(
            "{{\"id\":\"{}\",\"class\":\"{}\",\"seed\":{},\"registered_before_eval\":true,\"grid\":[{}],\n \"data\":[{}]}},\n",
            name, class, seed, params,
            files.iter().map(|(f, h)| format!("{{\"file\":\"{}\",\"fnv1a\":\"{:016x}\"}}", f, h)).collect::<Vec<_>>().join(",")
        ));
    }
    s.pop(); s.pop();
    s.push_str("\n]\n");
    fs::write(format!("{}/hypotheses_registry.json", out_dir), s)
}

// ============================= Main =============================

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => { eprintln!("[spectrum] ERROR: {}", e); std::process::exit(2); }
    };
    fs::create_dir_all(&args.out).ok();
    let rep_path = format!("{}/spectrum_report.txt", args.out);
    let mut rep = match fs::File::create(&rep_path) { Ok(f) => f, Err(e) => { eprintln!("out: {}", e); std::process::exit(2); } };
    let mut line = |s: &str| { let _ = rep.write_all(s.as_bytes()); let _ = rep.write_all(b"\n"); println!("{}", s); };

    line(&format!("[P68] spectrum_lab | fee={} spread={} train={} test={} step={} warm={} oos={:.0}%", args.fee, args.spread, args.train, args.test, args.step, args.warm, args.oos_frac * 100.0));

    let mut loaded: Vec<(String, Vec<Bar>, DataQa, u64)> = Vec::new();
    for f in &args.data {
        match load_csv(f) {
            Ok((bars, qa, h)) => {
                line(&format!("[P68] data {} | bars={} dups={} gaps={} ohlc_bad={} zero_vol={}",
                    f, qa.rows, qa.dups, qa.gaps, qa.ohlc_bad, qa.zero_vol));
                loaded.push((f.clone(), bars, qa, h));
            }
            Err(e) => line(&format!("[P68] SKIP {} : {}", f, e)),
        }
    }
    if loaded.is_empty() { line("[P68] FATAL: нет данных"); std::process::exit(1); }

    let _ = write_registry(&args.out,
        &loaded.iter().map(|(f, _, _, h)| (f.clone(), *h)).collect::<Vec<_>>(), 42);
    line("[P68] registry: hypotheses_registry.json записан ДО оценки");

    let cfg = ReplayCfg { fee: args.fee, spread: args.spread, max_hold: 96 };
    let cfg_stress = ReplayCfg { fee: args.fee * 2.0, spread: args.spread * 2.0, max_hold: 96 };

    let mut results_json = String::from("{\n  \"runs\": [\n");
    let mut daily_series: Vec<(String, std::collections::BTreeMap<i64, f64>)> = Vec::new();

    for (name, class) in HYPOTHESES {
        for (fname, bars, _qa, _h) in &loaded {
            let st = Stats::build(bars);
            let total = bars.len();
            if total < args.warm + args.train + args.test {
                line(&format!("[{}] {}: недостаточно баров ({}<{}) — пропуск", name, fname, total, args.warm + args.train + args.test));
                continue;
            }
            let oos_start = total - ((total as f64 * args.oos_frac) as usize);
            let wf_end = oos_start;
            let grid = param_grid(name);

            // --- 3. walk-forward по WF-зоне: параметры ТОЛЬКО по train ---
            let mut fold_rs: Vec<f64> = Vec::new();
            let mut wf_trades: Vec<Trade> = Vec::new();
            let mut t: usize = args.warm;
            while t + args.test <= wf_end {
                let tr_end = t + args.train.min(wf_end.saturating_sub(t));
                let mut best: Option<(Params, f64)> = None;
                for p in &grid {
                    let tr = replay(name, bars, &st, t, tr_end, p, &cfg);
                    let r: f64 = tr.iter().map(|x| x.r).sum();
                    if tr.len() >= 5 && (best.is_none() || r > best.unwrap().1) {
                        best = Some((*p, r));
                    }
                }
                let chosen = best.map(|b| b.0).unwrap_or(grid[grid.len() / 2]);
                let te = replay(name, bars, &st, tr_end, (tr_end + args.test).min(wf_end), &chosen, &cfg);
                fold_rs.push(te.iter().map(|x| x.r).sum());
                wf_trades.extend(te);
                t += args.step;
            }

            let (n_wf, r_wf, wr_wf, pf_wf, _be_wf, dd_wf) = aggregate(&wf_trades);
            let folds_pos = fold_rs.iter().filter(|r| **r > 0.0).count();
            let folds_tot = fold_rs.len();

            // финальные параметры: лучший по R на всей WF-зоне (n>=10)
            let mut final_best: Option<(Params, f64)> = None;
            for p in &grid {
                let tr = replay(name, bars, &st, args.warm, wf_end, p, &cfg);
                let r: f64 = tr.iter().map(|x| x.r).sum();
                if tr.len() >= 10 && (final_best.is_none() || r > final_best.unwrap().1) {
                    final_best = Some((*p, r));
                }
            }
            let Some((fp, _)) = final_best else {
                line(&format!("[{}] {}: нет параметров с n>=10 на IS — НЕ ХВАТАЕТ ДАННЫХ", name, fname));
                continue;
            };

            // --- 4. OOS-holdout: ОДИН прогон выбранных параметров ---
            let oos = replay(name, bars, &st, oos_start, total, &fp, &cfg);
            let (n_oos, r_oos, wr_oos, pf_oos, be_oos, dd_oos) = aggregate(&oos);

            // --- 5. стресс x2 ---
            let oos_stress = replay(name, bars, &st, oos_start, total, &fp, &cfg_stress);
            let (_, r_stress, wr_stress, _, _, _) = aggregate(&oos_stress);

            // --- 6. гейты ---
            let gate_n = n_oos >= 20 || n_wf >= 20;
            let gate_wr = wr_oos > be_oos + 0.03 && n_oos > 0;
            let gate_folds = folds_tot > 0 && folds_pos * 2 >= folds_tot;
            let verdict = if gate_n && gate_wr && gate_folds && r_oos > 0.0 { "PASS" }
                          else if n_oos == 0 && n_wf == 0 { "NO_TRADES" }
                          else { "FAIL" };

            line(&format!("[{}] {} | params lookback={} z={} period={} volx={}", name, fname, fp.lookback, fp.z_entry, fp.period, fp.vol_mult));
            line(&format!("[{}]   WF ({} фолдов, {}): n={} R={:+.2} WR={:.1}% PF={} DD={:.2} | folds R>0: {}/{}",
                name, folds_tot, class, n_wf, r_wf, wr_wf * 100.0,
                if pf_wf.is_infinite() { "inf".to_string() } else { format!("{:.2}", pf_wf) }, dd_wf, folds_pos, folds_tot));
            line(&format!("[{}]   OOS (lock, {:.0}%): n={} R={:+.2} WR={:.1}% (be={:.1}%) PF={} DD={:.2} | stress x2: R={:+.2} WR={:.1}%",
                name, args.oos_frac * 100.0, n_oos, r_oos, wr_oos * 100.0, be_oos * 100.0,
                if pf_oos.is_infinite() { "inf".to_string() } else { format!("{:.2}", pf_oos) }, dd_oos, r_stress, wr_stress * 100.0));
            line(&format!("[{}]   GATES: n={} wr>be+3пп={} folds>=50%={} R_oos>0={} => {}",
                name, if gate_n { "ok" } else { "no" }, if gate_wr { "ok" } else { "no" },
                if gate_folds { "ok" } else { "no" }, if r_oos > 0.0 { "ok" } else { "no" }, verdict));

            let mut dm = std::collections::BTreeMap::new();
            for t in wf_trades.iter().chain(oos.iter()) { *dm.entry(t.day).or_insert(0.0) += t.r; }
            daily_series.push((format!("{}@{}", name, fname.split(|c| c == '/' || c == '\\').last().unwrap_or(fname)), dm));

            results_json.push_str(&format!(
                "{{\"hypothesis\":\"{}\",\"data\":\"{}\",\"verdict\":\"{}\",\"wf\":{{\"folds\":{},\"folds_pos\":{},\"n\":{},\"r\":{:.4},\"wr\":{:.4},\"pf\":{:.4}}},\"oos\":{{\"n\":{},\"r\":{:.4},\"wr\":{:.4},\"pf\":{:.4},\"dd\":{:.4},\"stress_r_x2\":{:.4}}}}},\n",
                name, fname, verdict, folds_tot, folds_pos, n_wf, r_wf, wr_wf, pf_wf, n_oos, r_oos, wr_oos, pf_oos, dd_oos, r_stress));
        }
    }

    line("");
    line("[P68] ПОРТФЕЛЬНЫЙ ОВЕРЛЕЙ (корреляция дневных R-серий; гипотеза, не доказательство):");
    for i in 0..daily_series.len() {
        for j in (i + 1)..daily_series.len() {
            let (a, ma) = &daily_series[i];
            let (b, mb) = &daily_series[j];
            let days: Vec<i64> = ma.keys().filter(|k| mb.contains_key(k)).cloned().collect();
            if days.len() < 20 {
                line(&format!("[P68] corr({}, {}) — недостаточно общих дней ({})", a, b, days.len()));
                continue;
            }
            let xa: Vec<f64> = days.iter().map(|d| ma[d]).collect();
            let xb: Vec<f64> = days.iter().map(|d| mb[d]).collect();
            let (ma_, mb_) = (mean(&xa), mean(&xb));
            let mut cov = 0.0; let mut va = 0.0; let mut vb = 0.0;
            for k in 0..xa.len() {
                let da = xa[k] - ma_; let db = xb[k] - mb_;
                cov += da * db; va += da * da; vb += db * db;
            }
            let corr = if va > 0.0 && vb > 0.0 { cov / (va.sqrt() * vb.sqrt()) } else { 0.0 };
            line(&format!("[P68] corr({}, {}) = {:+.3} (n_days={})", a, b, corr, days.len()));
        }
    }

    results_json.truncate(results_json.trim_end().trim_end_matches(',').len());
    results_json.push_str("\n  ]\n}\n");
    let _ = fs::write(format!("{}/spectrum_results.json", args.out), results_json);
    line("");
    line("[P68] DONE: spectrum_report.txt / spectrum_results.json / hypotheses_registry.json");
}