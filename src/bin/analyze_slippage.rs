//! analyze_slippage.rs — калибровка модели проскальзывания на bookTicker-датасете.
//!
//! Запуск:
//!   cargo run --release --bin analyze_slippage -- data/bookticker_live
//!   cargo run --release --bin analyze_slippage -- data/bookticker_live --symbol 1000PEPEUSDT
//!   cargo run --release --bin analyze_slippage -- data/bookticker_live --sizes 100,500,1000,5000
//!
//! Что делает:
//!   1. Сканирует директорию на CSV-файлы bookTicker (формат: SYMBOL-YYYY-MM-DD.csv)
//!   2. Парсит строки: update_id,bid,bid_qty,ask,ask_qty,txn_time,event_time
//!   3. Для каждого тика считает spread, L1 depth, slippage для разных размеров ордеров
//!   4. Использует reservoir sampling — память ограничена ~10K образцов на статистику
//!   5. Выводит mean/median/p95 для spread, depth, slippage
//!
//! Память: ~640 KB на символ (reservoir по 10K образцов × 8 stats).
//! Время: ~1-3 минуты на 18M тиков.

use std::env;
use std::fs;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::collections::HashMap;
use std::time::Instant;

const RESERVOIR_CAPACITY: usize = 10_000;

/// Streaming-статистика с reservoir sampling для оценки percentile.
/// Память: O(capacity) — фиксированная.
struct StreamingStats {
    n: u64,
    sum: f64,
    sum_sq: f64,
    min: f64,
    max: f64,
    reservoir: Vec<f64>,
    capacity: usize,
    lcg_state: u64,
}

impl StreamingStats {
    fn new() -> Self {
        Self {
            n: 0,
            sum: 0.0,
            sum_sq: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            reservoir: Vec::with_capacity(RESERVOIR_CAPACITY),
            capacity: RESERVOIR_CAPACITY,
            lcg_state: 0x6d5f9c3a8b2e1f04, // seed
        }
    }

    /// Linear Congruential Generator — pseudo-random без зависимости от rand crate.
    fn next_rand(&mut self) -> u64 {
        // Используем Numerical Recipes LCG
        self.lcg_state = self
            .lcg_state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.lcg_state
    }

    fn add(&mut self, value: f64) {
        self.n += 1;
        self.sum += value;
        self.sum_sq += value * value;
        if value < self.min {
            self.min = value;
        }
        if value > self.max {
            self.max = value;
        }

        // Reservoir sampling (Algorithm R):
        //   Если буфер не заполнен — добавляем
        //   Иначе — заменяем случайный элемент с вероятностью capacity/n
        if self.reservoir.len() < self.capacity {
            self.reservoir.push(value);
        } else {
            let j = self.next_rand() % self.n;
            if (j as usize) < self.capacity {
                self.reservoir[j as usize] = value;
            }
        }
    }

    fn mean(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.sum / self.n as f64
        }
    }

    fn percentile(&self, p: f64) -> f64 {
        if self.reservoir.is_empty() {
            return 0.0;
        }
        let mut sorted = self.reservoir.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((sorted.len() as f64 - 1.0) * p / 100.0).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }
}

struct SymbolStats {
    n: u64,
    spreads: StreamingStats,
    bid_depths: StreamingStats,
    ask_depths: StreamingStats,
    mids: StreamingStats,
    buy_slippage: HashMap<u32, StreamingStats>,
    sell_slippage: HashMap<u32, StreamingStats>,
}

impl SymbolStats {
    fn new() -> Self {
        Self {
            n: 0,
            spreads: StreamingStats::new(),
            bid_depths: StreamingStats::new(),
            ask_depths: StreamingStats::new(),
            mids: StreamingStats::new(),
            buy_slippage: HashMap::new(),
            sell_slippage: HashMap::new(),
        }
    }
}

fn parse_line(line: &str) -> Option<(f64, f64, f64, f64, i64)> {
    let parts: Vec<&str> = line.split(',').collect();
    if parts.len() < 7 {
        return None;
    }
    let bid: f64 = parts[1].trim().parse().ok()?;
    let bid_qty: f64 = parts[2].trim().parse().ok()?;
    let ask: f64 = parts[3].trim().parse().ok()?;
    let ask_qty: f64 = parts[4].trim().parse().ok()?;
    let ts: i64 = parts[6].trim().parse().ok().unwrap_or(0);
    Some((bid, bid_qty, ask, ask_qty, ts))
}

/// Оценка slippage для рыночного ордера размером size_qty через L1 стакан.
/// Возвращает slippage в процентах от mid.
fn estimate_slippage(size_qty: f64, l1_qty: f64, half_spread_pct: f64) -> f64 {
    if l1_qty <= 0.0 {
        return half_spread_pct * 2.0;
    }
    if size_qty <= l1_qty {
        return half_spread_pct;
    }
    let excess_ratio = (size_qty - l1_qty) / l1_qty;
    half_spread_pct * (1.0 + excess_ratio)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: analyze_slippage <dir> [--symbol SYMBOL] [--sizes 100,500,1000,5000] [--out report.csv]");
        std::process::exit(1);
    }
    let dir = &args[1];
    let mut symbol_filter: Option<String> = None;
    let mut sizes: Vec<u32> = vec![100, 500, 1000, 5000];
    let mut out_path: Option<String> = None;

    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--symbol" => {
                symbol_filter = Some(args[i + 1].to_uppercase());
                i += 2;
            }
            "--sizes" => {
                sizes = args[i + 1]
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
                i += 2;
            }
            "--out" => {
                out_path = Some(args[i + 1].clone());
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }

    // Собираем список CSV
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map(|e| e == "csv").unwrap_or(false) {
            if let Some(s) = &symbol_filter {
                let fname = path
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default();
                if !fname.starts_with(s) {
                    continue;
                }
            }
            files.push(path);
        }
    }
    files.sort();

    if files.is_empty() {
        eprintln!("❌ Не найдено CSV-файлов в {}", dir);
        std::process::exit(1);
    }

    println!("📂 Найдено {} CSV-файлов в {}", files.len(), dir);
    if let Some(s) = &symbol_filter {
        println!("   Фильтр по символу: {}", s);
    }
    println!("   Размеры ордеров: {:?}", sizes);
    println!();

    let start = Instant::now();
    let mut stats: HashMap<String, SymbolStats> = HashMap::new();

    for (idx, file) in files.iter().enumerate() {
        let symbol = file
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .split('-')
            .next()
            .unwrap_or("UNKNOWN")
            .to_string();

        let stat = stats.entry(symbol.clone()).or_insert_with(|| {
            let mut s = SymbolStats::new();
            for &size in &sizes {
                s.buy_slippage.insert(size, StreamingStats::new());
                s.sell_slippage.insert(size, StreamingStats::new());
            }
            s
        });

        let reader = BufReader::new(File::open(file)?);

        for (lineno, line) in reader.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            if lineno == 0
                && !line
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false)
            {
                continue;
            }

            let (bid, bid_qty, ask, ask_qty, _ts) = match parse_line(&line) {
                Some(x) => x,
                None => continue,
            };
            if bid <= 0.0 || ask <= 0.0 || bid_qty < 0.0 || ask_qty < 0.0 {
                continue;
            }

            let mid = (bid + ask) / 2.0;
            if mid <= 0.0 {
                continue;
            }
            let spread_pct = (ask - bid) / mid * 100.0;
            let half_spread_pct = spread_pct / 2.0;
            let bid_depth_usdt = bid_qty * mid;
            let ask_depth_usdt = ask_qty * mid;

            stat.n += 1;
            stat.spreads.add(spread_pct);
            stat.bid_depths.add(bid_depth_usdt);
            stat.ask_depths.add(ask_depth_usdt);
            stat.mids.add(mid);

            // Slippage для каждого размера ордера
            for &size_usdt in &sizes {
                let size_qty = size_usdt as f64 / mid;

                let buy_slip = estimate_slippage(size_qty, ask_qty, half_spread_pct);
                let sell_slip = estimate_slippage(size_qty, bid_qty, half_spread_pct);

                if let Some(s) = stat.buy_slippage.get_mut(&size_usdt) {
                    s.add(buy_slip);
                }
                if let Some(s) = stat.sell_slippage.get_mut(&size_usdt) {
                    s.add(sell_slip);
                }
            }
        }

        if (idx + 1) % 5 == 0 || idx + 1 == files.len() {
            println!(
                "  [{}] обработано {}/{} файлов, {} тиков в текущем символе",
                idx + 1,
                idx + 1,
                files.len(),
                stat.n
            );
        }
    }

    let elapsed = start.elapsed();
    println!("\n⏱ Готово за {:.2}s", elapsed.as_secs_f64());

    // === Вывод результатов ===
    let mut out_writer: Option<std::io::BufWriter<std::fs::File>> = out_path
        .as_ref()
        .and_then(|p| File::create(p).ok())
        .map(std::io::BufWriter::new);

    if let Some(w) = &mut out_writer {
        writeln!(w, "symbol,ticks,spread_mean%,spread_median%,spread_p95%,bid_depth_usdt_median,ask_depth_usdt_median,order_size_usdt,buy_slip_mean%,buy_slip_p95%,sell_slip_mean%,sell_slip_p95%")?;
    }

    // Сортируем символы по алфавиту для предсказуемого вывода
    let mut symbols: Vec<&String> = stats.keys().collect();
    symbols.sort();

    for symbol in symbols {
        let stat = stats.get(symbol).unwrap();

        println!("\n{}", "═".repeat(70));
        println!("📊 {} — {} тиков", symbol, stat.n);

        if stat.spreads.n == 0 {
            println!("   (нет данных)");
            continue;
        }

        let spread_mean = stat.spreads.mean();
        let spread_median = stat.spreads.percentile(50.0);
        let spread_p95 = stat.spreads.percentile(95.0);
        let bid_depth_median = stat.bid_depths.percentile(50.0);
        let ask_depth_median = stat.ask_depths.percentile(50.0);
        let mid_median = stat.mids.percentile(50.0);

        println!("   Средняя цена (mid, медиана): {:.6}", mid_median);
        println!(
            "   Spread:  mean={:.4}%  median={:.4}%  p95={:.4}%",
            spread_mean, spread_median, spread_p95
        );
        println!(
            "   L1 depth bid (USDT, медиана): {:.2}",
            bid_depth_median
        );
        println!(
            "   L1 depth ask (USDT, медиана): {:.2}",
            ask_depth_median
        );

        println!("\n   📐 Slippage по размерам ордеров:");
        println!(
            "   {:>10}  {:>13}  {:>13}  {:>13}  {:>13}",
            "Size USDT", "BUY mean", "BUY p95", "SELL mean", "SELL p95"
        );
        println!("   {}", "─".repeat(70));

        for &size in &sizes {
            if let (Some(buys), Some(sells)) =
                (stat.buy_slippage.get(&size), stat.sell_slippage.get(&size))
            {
                let bm = buys.mean();
                let bp = buys.percentile(95.0);
                let sm = sells.mean();
                let sp = sells.percentile(95.0);
                println!(
                    "   {:>10}  {:>12.4}%  {:>12.4}%  {:>12.4}%  {:>12.4}%",
                    size, bm, bp, sm, sp
                );

                if let Some(w) = &mut out_writer {
                    writeln!(
                        w,
                        "{},{},{:.4},{:.4},{:.4},{:.2},{:.2},{},{:.4},{:.4},{:.4},{:.4}",
                        symbol,
                        stat.n,
                        spread_mean,
                        spread_median,
                        spread_p95,
                        bid_depth_median,
                        ask_depth_median,
                        size,
                        bm,
                        bp,
                        sm,
                        sp
                    )?;
                }
            }
        }

        // === Сводка для strategy-design решений ===
        println!("\n   🎯 Рекомендация для $200-бота (1 USDT risk per trade):");
        for &size in &sizes {
            if let Some(buys) = stat.buy_slippage.get(&size) {
                let bp95 = buys.percentile(95.0);
                let verdict = if bp95 < 0.05 {
                    "✅ безопасно (slip < 5bps)"
                } else if bp95 < 0.20 {
                    "⚠️ приемлемо (slip < 20bps)"
                } else if bp95 < 0.50 {
                    "🔴 высокий slip (50bps) — нужен maker entry"
                } else {
                    "❌ неприемлемо — обязательно limit"
                };
                println!(
                    "     ${} notional:  p95 slip = {:.4}%  {}",
                    size, bp95, verdict
                );
            }
        }
    }

    if let Some(path) = out_path {
        println!("\n💾 Отчёт сохранён: {}", path);
    }

    Ok(())
}
