//! Backtest-бинарник. Режимы:
//!   --data trades      — только aggTrades
//!   --data klines      — только klines
//!   --data spread      — merged bookTicker + aggTrades (для spread_capture)
//!   --data book        — только bookTicker (без aggTrades, fill симулируется)
//!
//! Использование:
//!   backtest <book_dir> --bot <name> --data book
//!   backtest <trades_dir> --bot <name> --data spread --bookticker <book_dir>
//!   backtest <trades_dir> --bot <name> --data trades
//!   backtest <klines_dir> --bot <name> --data klines

use anyhow::{anyhow, Result};
use rust_decimal::prelude::ToPrimitive;
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Lines};
use std::iter::Peekable;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use scalping_agents::config::AppConfig;
use scalping_agents::execution::slippage::SlippageCalculator;
use scalping_agents::strategies::{self, OrderBookAnalyzer, OrderBookState, Strategy};
use scalping_agents::websocket::{
    BookTickerMessage, KlineMessage, TradeMessage, WebSocketMessage,
};

const TAKER_FEE_SPOT: f64 = 0.00075;
const TAKER_FEE_FUTURES: f64 = 0.0005;
const MAKER_FEE_FUTURES: f64 = 0.0002;
const MAKER_FEE_SPOT: f64 = 0.0002;
const MIN_NOTIONAL: f64 = 5.0;
const SL_SLIPPAGE_TICKS: f64 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq)]
enum DataType {
    Trades,
    Klines,
    Spread,
    Book, // ✅ НОВОЕ: только bookTicker, без aggTrades
}

#[derive(Debug, Clone)]
struct OpenPosition {
    side: String,
    entry: f64,
    qty: f64,
    sl: Option<f64>,
    tp: Option<f64>,
    opened_at_ms: i64,
}

#[derive(Debug, Clone)]
struct PendingLimit {
    side: String,
    price: f64,
    qty: f64,
    sl: f64,
    tp: f64,
    placed_at_ms: i64,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ClosedTrade {
    side: String,
    entry: f64,
    exit: f64,
    qty: f64,
    pnl_gross: f64,
    fees: f64,
    pnl_net: f64,
    reason: &'static str,
    duration_ms: i64,
    day_key: String,
}

#[derive(Debug, Default, Clone)]
#[allow(dead_code)]
struct Stats {
    n: usize,
    wins: usize,
    losses: usize,
    win_rate: f64,
    avg_win: f64,
    avg_loss: f64,
    ev: f64,
    profit_factor: f64,
    net_pnl: f64,
    gross_profit: f64,
    gross_loss: f64,
    max_dd: f64,
}

#[derive(Debug, Default)]
struct Args {
    primary_path: String,
    bookticker_path: Option<String>,
    bot_name: Option<String>,
    symbol_override: Option<String>,
    config_path: String,
    data_type: Option<DataType>,
}

fn parse_args() -> Result<Args> {
    let argv: Vec<String> = env::args().collect();
    if argv.len() < 2 {
        print_usage();
        std::process::exit(1);
    }
    let mut args = Args {
        primary_path: argv[1].clone(),
        bookticker_path: None,
        bot_name: None,
        symbol_override: None,
        config_path: "config.toml".to_string(),
        data_type: None,
    };
    let mut i = 2;
    while i < argv.len() {
        match argv[i].as_str() {
            "--bot" => { args.bot_name = Some(argv[i + 1].clone()); i += 2; }
            "--symbol" => { args.symbol_override = Some(argv[i + 1].to_uppercase()); i += 2; }
            "--config" => { args.config_path = argv[i + 1].clone(); i += 2; }
            "--bookticker" => { args.bookticker_path = Some(argv[i + 1].clone()); i += 2; }
            "--data" => {
                args.data_type = Some(match argv[i + 1].as_str() {
                    "trades" | "aggTrades" => DataType::Trades,
                    "klines" | "kline" => DataType::Klines,
                    "spread" | "merged" => DataType::Spread,
                    "book" | "bookticker" => DataType::Book,
                    other => return Err(anyhow!("Unknown --data: {}", other)),
                });
                i += 2;
            }
            "-h" | "--help" => { print_usage(); std::process::exit(0); }
            other => return Err(anyhow!("Unknown arg: {}", other)),
        }
    }
    Ok(args)
}

fn print_usage() {
    println!("Usage:");
    println!("  backtest <book_dir> --bot <name> --data book");
    println!("  backtest <trades_path> --bot <name> --data spread --bookticker <book_path>");
    println!("  backtest <trades_dir> --bot <name> --data trades");
    println!("  backtest <klines_dir> --bot <name> --data klines");
}

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let args = parse_args()?;

    let config = AppConfig::load_from(&args.config_path)?;
    let global = config.global.clone();

    let bot = match args.bot_name.as_ref() {
        Some(name) => config.bots.iter().find(|b| &b.name == name)
            .ok_or_else(|| {
                let avail: Vec<&str> = config.bots.iter().map(|b| b.name.as_str()).collect();
                anyhow!("Bot '{}' not found. Available: {:?}", name, avail)
            })?.clone(),
        None => config.bots.first().ok_or_else(|| anyhow!("No [[bots]]"))?.clone(),
    };

    let symbol = args.symbol_override.clone()
        .or_else(|| bot.symbol.clone())
        .unwrap_or_else(|| global.symbol.clone());

    let is_futures = bot.market_type == "futures";
    let maker_fee = if is_futures { MAKER_FEE_FUTURES } else { MAKER_FEE_SPOT };
    let taker_fee = if is_futures { TAKER_FEE_FUTURES } else { TAKER_FEE_SPOT };

    let config_fee = bot.fee_rate
        .map(|d| d.to_f64().unwrap_or(taker_fee))
        .unwrap_or(taker_fee);
    if config_fee < taker_fee {
        println!("⚠️  ВНИМАНИЕ: config fee_rate = {:.4}% — это maker fee, не taker!", config_fee * 100.0);
        println!("   Бэктест использует правильные ставки: maker={:.4}%, taker={:.4}%", maker_fee * 100.0, taker_fee * 100.0);
    }

    let data_type = args.data_type.unwrap_or(DataType::Trades);

    println!("╔═══════════════════════════════════════════════════════════╗");
    println!("║   BACKTEST                                                ║");
    println!("╠═══════════════════════════════════════════════════════════╣");
    println!("║   Bot:         {:42} ║", bot.name);
    println!("║   Symbol:      {:42} ║", symbol);
    println!("║   Strategy:    {:42} ║", bot.strategy.name);
    println!("║   Market:      {:42} ║", bot.market_type);
    println!("║   Data type:   {:42} ║", match data_type {
        DataType::Trades => "trades",
        DataType::Klines => "klines",
        DataType::Spread => "spread (bookTicker + aggTrades)",
        DataType::Book => "bookTicker only (fill simulated)",
    });
    println!("║   Maker fee:  {:>10.4}%                                  ║", maker_fee * 100.0);
    println!("║   Taker fee:  {:>10.4}%                                  ║", taker_fee * 100.0);
    println!("╚═══════════════════════════════════════════════════════════╝");

    if !bot.strategy.params.is_empty() {
        println!("⚙️  Params:");
        let mut keys: Vec<&String> = bot.strategy.params.keys().collect();
        keys.sort();
        for k in keys {
            if let Some(v) = bot.strategy.params.get(k) {
                println!("     {} = {}", k, v);
            }
        }
    }

    let tick_size = if symbol.contains("PEPE") || symbol.contains("DOGS") {
        0.00000001
    } else if symbol.contains("DOGE") {
        0.00001
    } else {
        0.00001
    };
    println!("   Tick size: {}", tick_size);

    let ob_analyzer = OrderBookAnalyzer::new(
        global.orderbook_density_threshold.to_f64().unwrap_or(5.0),
    );
    let ob_state = Arc::new(RwLock::new(OrderBookState::default()));
    let mut strategy = strategies::create_strategy(&bot, &symbol, ob_state, ob_analyzer)?;

    let max_duration_ms = (bot.max_position_duration_sec as i64) * 1000;
    let order_timeout_ms = (bot.order_timeout_sec as i64) * 1000;

    let mut all_trades: Vec<ClosedTrade> = Vec::new();
    let mut total_msgs = 0u64;
    let mut total_signals = 0u64;
    let mut first_ts = 0i64;
    let mut last_ts = 0i64;

    match data_type {
        DataType::Book => {
            let book_files = collect_csv_files(&args.primary_path)?;
            if book_files.is_empty() {
                return Err(anyhow!("No bookTicker CSV files found in {}", args.primary_path));
            }
            println!("\n📚 Found {} bookTicker files", book_files.len());

            for (idx, book_file) in book_files.iter().enumerate() {
                let day_key = extract_date_suffix(book_file)
                    .unwrap_or_else(|| {
                        book_file.file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("unknown")
                            .to_string()
                    });
                println!("\n[{}/{}] {}", idx + 1, book_files.len(), book_file.display());

                let (trades, msgs, sigs, f_ts, l_ts) = run_book_only(
                    book_file, &symbol, maker_fee, taker_fee, tick_size,
                    max_duration_ms, order_timeout_ms, &mut strategy, &day_key,
                )?;

                total_msgs += msgs;
                total_signals += sigs;
                if first_ts == 0 { first_ts = f_ts; }
                last_ts = l_ts;
                all_trades.extend(trades);
            }
        }
        DataType::Spread => {
            let book_path = args.bookticker_path.as_ref()
                .ok_or_else(|| anyhow!("--bookticker required for --data spread"))?;

            let trades_files = collect_csv_files(&args.primary_path)?;
            let book_files = collect_csv_files(book_path)?;

            let pairs = match_files_by_date(&trades_files, &book_files)?;
            if pairs.is_empty() {
                return Err(anyhow!("No matching trades/bookticker pairs found"));
            }

            for (trades_file, book_file) in &pairs {
                let day_key = extract_date_suffix(trades_file)
                    .unwrap_or_else(|| {
                        trades_file.file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("unknown")
                            .to_string()
                    });
                println!("\n▶ {} + {}", trades_file.display(), book_file.display());

                let (trades, msgs, sigs, f_ts, l_ts) = run_merged(
                    trades_file, book_file, &symbol, maker_fee, taker_fee, tick_size,
                    max_duration_ms, order_timeout_ms, &mut strategy, &day_key,
                )?;

                total_msgs += msgs;
                total_signals += sigs;
                if first_ts == 0 { first_ts = f_ts; }
                last_ts = l_ts;
                all_trades.extend(trades);
            }
        }
        DataType::Trades | DataType::Klines => {
            let files = collect_csv_files(&args.primary_path)?;
            let slippage = SlippageCalculator;

            for (idx, file) in files.iter().enumerate() {
                let day_key = extract_date_suffix(file)
                    .unwrap_or_else(|| {
                        file.file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("unknown")
                            .to_string()
                    });
                println!("\n[{}/{}] {}", idx + 1, files.len(), file.display());

                let (trades, msgs, sigs, f_ts, l_ts) = run_single(
                    file, &symbol, maker_fee, taker_fee, tick_size, max_duration_ms,
                    &mut strategy, &slippage, &day_key, data_type,
                )?;

                total_msgs += msgs;
                total_signals += sigs;
                if first_ts == 0 { first_ts = f_ts; }
                last_ts = l_ts;
                all_trades.extend(trades);
            }
        }
    }

    if let Some(diag) = strategy.diagnostics() {
        println!("\n🔍 DIAGNOSTICS:");
        println!("   {}", diag);
    }

    let stats = compute_stats(&all_trades);
    let days = if first_ts > 0 {
        ((last_ts - first_ts) as f64 / 86_400_000.0).max(1.0)
    } else {
        1.0
    };

    print_report(&stats, &all_trades, days, total_msgs, total_signals);
    print_breakdown(&all_trades);

    Ok(())
}

fn collect_csv_files(path: &str) -> Result<Vec<PathBuf>> {
    let p = Path::new(path);
    if p.is_file() {
        return Ok(vec![p.to_path_buf()]);
    }
    if p.is_dir() {
        let mut files: Vec<_> = fs::read_dir(p)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("csv"))
            .collect();
        files.sort();
        return Ok(files);
    }
    Err(anyhow!("Path not found: {}", path))
}

// ✅ ИСПРАВЛЕНО: было < 5, должно быть < 4
fn extract_date_suffix(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 4 { return None; } // ✅ БЫЛО < 5
    let n = parts.len();
    Some(format!("{}-{}-{}", parts[n-3], parts[n-2], parts[n-1]))
}

fn match_files_by_date(trades: &[PathBuf], books: &[PathBuf]) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut pairs = Vec::new();
    for t in trades {
        let t_date = match extract_date_suffix(t) { Some(d) => d, None => continue };
        for b in books {
            if let Some(b_date) = extract_date_suffix(b) {
                if b_date == t_date {
                    pairs.push((t.clone(), b.clone()));
                    break;
                }
            }
        }
    }
    Ok(pairs)
}
// ============================================================
// SINGLE STREAM (trades | klines)
// ============================================================
fn run_single(
    file: &Path,
    symbol: &str,
    maker_fee: f64,
    taker_fee: f64,
    tick_size: f64,
    max_duration_ms: i64,
    strategy: &mut Box<dyn Strategy>,
    slippage_calc: &SlippageCalculator,
    day_key: &str,
    data_type: DataType,
) -> Result<(Vec<ClosedTrade>, u64, u64, i64, i64)> {
    let reader = BufReader::new(File::open(file)?);
    let mut position: Option<OpenPosition> = None;
    let mut trades: Vec<ClosedTrade> = Vec::new();
    let mut msgs = 0u64;
    let mut sigs = 0u64;
    let mut first_ts = 0i64;
    let mut last_ts = 0i64;
    let mut last_price = 0.0f64;

    for (line_no, line) in reader.lines().enumerate() {
        let line = match line { Ok(l) => l, Err(_) => continue };
        if line_no == 0
            && !line.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
        { continue; }

        let (msg, price_now, ts_now) = match data_type {
            DataType::Trades => {
                let t = match parse_agg_trade(&line, symbol) { Some(t) => t, None => continue };
                (WebSocketMessage::Trade(t.clone()), t.price, t.timestamp)
            }
            DataType::Klines => {
                let k = match parse_kline(&line, symbol) { Some(k) => k, None => continue };
                (WebSocketMessage::Kline(k.clone()), k.close, k.timestamp)
            }
            _ => return Err(anyhow!("run_single called with Spread/Book")),
        };

        msgs += 1;
        if first_ts == 0 { first_ts = ts_now; }
        last_ts = ts_now;
        last_price = price_now;

        if let Some(pos) = &position {
            if let Some(reason) = check_exit(pos, price_now, ts_now, max_duration_ms, tick_size) {
                let exit = calc_exit_price(pos, price_now, reason, tick_size);
                let trade = close_position(pos, exit, reason, ts_now, maker_fee, taker_fee, day_key);
                // ✅ ИСПРАВЛЕНО: вызываем notify_position_closed
                strategy.notify_position_closed(trade.pnl_net);
                trades.push(trade);
                position = None;
            }
        }

        if position.is_none() {
            if let Some(signal) = strategy.process(&msg)? {
                sigs += 1;

                // ✅ НОВОЕ: проверка минимального нотационала
                let notional = signal.quantity * signal.price;
                if notional < MIN_NOTIONAL {
                    continue;
                }

                let slip = slippage_calc.calculate_slippage(symbol, signal.price, signal.quantity);
                let fill_price = match signal.side.as_str() {
                    "BUY" => signal.price * (1.0 + slip),
                    "SELL" => signal.price * (1.0 - slip),
                    _ => signal.price,
                };
                position = Some(OpenPosition {
                    side: signal.side.clone(),
                    entry: fill_price,
                    qty: signal.quantity,
                    sl: signal.stop_loss,
                    tp: signal.take_profit,
                    opened_at_ms: ts_now,
                });
                // ✅ ИСПРАВЛЕНО: вызываем notify_position_open
                strategy.notify_position_open();
            }
        }
    }

    if let Some(pos) = position.take() {
        if last_price > 0.0 {
            let trade = close_position(&pos, last_price, "EndOfData", last_ts, maker_fee, taker_fee, day_key);
            strategy.notify_position_closed(trade.pnl_net);
            trades.push(trade);
        }
    }

    Ok((trades, msgs, sigs, first_ts, last_ts))
}

// ============================================================
// MERGED STREAM (bookTicker + aggTrades)
// ============================================================
fn run_merged(
    trades_file: &Path,
    book_file: &Path,
    symbol: &str,
    maker_fee: f64,
    taker_fee: f64,
    tick_size: f64,
    max_duration_ms: i64,
    order_timeout_ms: i64,
    strategy: &mut Box<dyn Strategy>,
    day_key: &str,
) -> Result<(Vec<ClosedTrade>, u64, u64, i64, i64)> {
    let trades_reader: Lines<BufReader<File>> =
        BufReader::new(File::open(trades_file)?).lines();
    let book_reader: Lines<BufReader<File>> =
        BufReader::new(File::open(book_file)?).lines();

    let mut trades_iter = trades_reader.peekable();
    let mut book_iter = book_reader.peekable();

    skip_header(&mut trades_iter);
    skip_header(&mut book_iter);

    let mut position: Option<OpenPosition> = None;
    let mut pending: Option<PendingLimit> = None;
    let mut trades: Vec<ClosedTrade> = Vec::new();
    let mut msgs = 0u64;
    let mut sigs = 0u64;
    let mut first_ts = 0i64;
    let mut last_ts = 0i64;

    loop {
        let t_ts = peek_trade_ts(&mut trades_iter);
        let b_ts = peek_book_ts(&mut book_iter);

        let process_trade = match (t_ts, b_ts) {
            (Some(t), Some(b)) => t <= b,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };

        let now_ts;
        let msg: WebSocketMessage;

        if process_trade {
            let line = match trades_iter.next() {
                Some(Ok(l)) => l,
                Some(Err(_)) => continue,
                None => break,
            };
            let t = match parse_agg_trade(&line, symbol) {
                Some(t) => t,
                None => continue,
            };
            now_ts = t.timestamp;
            msg = WebSocketMessage::Trade(t);
        } else {
            let line = match book_iter.next() {
                Some(Ok(l)) => l,
                Some(Err(_)) => continue,
                None => break,
            };
            let bt = match parse_book_ticker(&line, symbol) {
                Some(bt) => bt,
                None => continue,
            };
            now_ts = bt.timestamp;
            msg = WebSocketMessage::BookTicker(bt);
        }

        msgs += 1;
        if first_ts == 0 { first_ts = now_ts; }
        last_ts = now_ts;

        // 1. Fill detection
        if let (Some(order), WebSocketMessage::Trade(t)) = (&pending, &msg) {
            let filled = match order.side.as_str() {
                "BUY" => t.is_buyer_maker && t.price <= order.price,
                "SELL" => !t.is_buyer_maker && t.price >= order.price,
                _ => false,
            };
            if filled {
                position = Some(OpenPosition {
                    side: order.side.clone(),
                    entry: order.price,
                    qty: order.qty,
                    sl: Some(order.sl),
                    tp: Some(order.tp),
                    opened_at_ms: t.timestamp,
                });
                pending = None;
                // ✅ ИСПРАВЛЕНО: вызываем notify_position_open
                strategy.notify_position_open();
            }
        }

        // 2. Timeout pending
        if let Some(order) = &pending {
            if now_ts - order.placed_at_ms > order_timeout_ms {
                pending = None;
            }
        }

        // 3. Exit check
        if let (Some(pos), WebSocketMessage::Trade(t)) = (&position, &msg) {
            if let Some(reason) = check_exit(pos, t.price, t.timestamp, max_duration_ms, tick_size) {
                let exit = calc_exit_price(pos, t.price, reason, tick_size);
                let trade = close_position(pos, exit, reason, t.timestamp, maker_fee, taker_fee, day_key);
                // ✅ ИСПРАВЛЕНО: вызываем notify_position_closed
                strategy.notify_position_closed(trade.pnl_net);
                trades.push(trade);
                position = None;
            }
        }

        // 4. Skip new signals if busy
        if pending.is_some() || position.is_some() {
            continue;
        }

        // 5. Try signal
        if let Some(signal) = strategy.process(&msg)? {
            sigs += 1;

            // ✅ НОВОЕ: проверка минимального нотационала
            let notional = signal.quantity * signal.price;
            if notional < MIN_NOTIONAL {
                continue;
            }

            let sl = signal.stop_loss.unwrap_or(signal.price * 0.998);
            let tp = signal.take_profit.unwrap_or(signal.price * 1.002);
            pending = Some(PendingLimit {
                side: signal.side.clone(),
                price: signal.price,
                qty: signal.quantity,
                sl,
                tp,
                placed_at_ms: now_ts,
            });
        }
    }

    if let Some(pos) = position.take() {
        if last_ts > 0 {
            let trade = close_position(&pos, pos.entry, "EndOfData", last_ts, maker_fee, taker_fee, day_key);
            strategy.notify_position_closed(trade.pnl_net);
            trades.push(trade);
        }
    }

    Ok((trades, msgs, sigs, first_ts, last_ts))
}

// ============================================================
// ✅ НОВАЯ ФУНКЦИЯ: BOOK-ONLY STREAM (только bookTicker)
// Симулирует fill по bookTicker данным, без aggTrades.
// ============================================================
fn run_book_only(
    book_file: &Path,
    symbol: &str,
    maker_fee: f64,
    taker_fee: f64,
    tick_size: f64,
    max_duration_ms: i64,
    _order_timeout_ms: i64,
    strategy: &mut Box<dyn Strategy>,
    day_key: &str,
) -> Result<(Vec<ClosedTrade>, u64, u64, i64, i64)> {
    let reader = BufReader::new(File::open(book_file)?);
    let mut position: Option<OpenPosition> = None;
    let mut trades: Vec<ClosedTrade> = Vec::new();
    let mut msgs = 0u64;
    let mut sigs = 0u64;
    let mut first_ts = 0i64;
    let mut last_ts = 0i64;
    let mut last_price = 0.0f64;

    for (line_no, line) in reader.lines().enumerate() {
        let line = match line { Ok(l) => l, Err(_) => continue };
        if line_no == 0
            && !line.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
        { continue; }

        let bt = match parse_book_ticker(&line, symbol) {
            Some(bt) => bt,
            None => continue,
        };

        if bt.best_bid <= 0.0 || bt.best_ask <= 0.0 { continue; }

        let mid = (bt.best_bid + bt.best_ask) / 2.0;
        let price_now = mid;

        msgs += 1;
        if first_ts == 0 { first_ts = bt.timestamp; }
        last_ts = bt.timestamp;
        last_price = price_now;

        // 1. Exit check (по mid price)
        if let Some(pos) = &position {
            if let Some(reason) = check_exit(pos, price_now, bt.timestamp, max_duration_ms, tick_size) {
                let exit = calc_exit_price(pos, price_now, reason, tick_size);
                let trade = close_position(pos, exit, reason, bt.timestamp, maker_fee, taker_fee, day_key);
                strategy.notify_position_closed(trade.pnl_net);
                trades.push(trade);
                position = None;
            }
        }

        // 2. Try signal (только если нет позиции)
        if position.is_none() {
            let msg = WebSocketMessage::BookTicker(bt.clone());
            if let Some(signal) = strategy.process(&msg)? {
                sigs += 1;

                // ✅ НОВОЕ: проверка минимального нотационала
                let notional = signal.quantity * signal.price;
                if notional < MIN_NOTIONAL {
                    continue;
                }

                // ✅ Fill simulation: maker fill = instant at signal price
                position = Some(OpenPosition {
                    side: signal.side.clone(),
                    entry: signal.price,
                    qty: signal.quantity,
                    sl: signal.stop_loss,
                    tp: signal.take_profit,
                    opened_at_ms: bt.timestamp,
                });
                strategy.notify_position_open();
            }
        }
    }

    if let Some(pos) = position.take() {
        if last_price > 0.0 {
            let trade = close_position(&pos, last_price, "EndOfData", last_ts, maker_fee, taker_fee, day_key);
            strategy.notify_position_closed(trade.pnl_net);
            trades.push(trade);
        }
    }

    Ok((trades, msgs, sigs, first_ts, last_ts))
}
fn skip_header<I: Iterator<Item = std::io::Result<String>>>(iter: &mut Peekable<I>) {
    if let Some(Ok(line)) = iter.peek() {
        if !line.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
            iter.next();
        }
    }
}

fn peek_trade_ts(iter: &mut Peekable<Lines<BufReader<File>>>) -> Option<i64> {
    let line = iter.peek()?.as_ref().ok()?;
    let parts: Vec<&str> = line.split(',').collect();
    if parts.len() < 7 { return None; }
    let ts_raw: i64 = parts[5].trim().parse().ok()?;
    Some(normalize_ts(ts_raw))
}

fn peek_book_ts(iter: &mut Peekable<Lines<BufReader<File>>>) -> Option<i64> {
    let line = iter.peek()?.as_ref().ok()?;
    let parts: Vec<&str> = line.split(',').collect();
    if parts.len() < 7 { return None; }
    let ts_raw: i64 = parts[6].trim().parse().ok()?;
    Some(normalize_ts(ts_raw))
}

fn normalize_ts(ts_raw: i64) -> i64 {
    if ts_raw > 100_000_000_000_000 { ts_raw / 1000 } else { ts_raw }
}

// ============================================================
// Parsers
// ============================================================
fn parse_agg_trade(line: &str, symbol: &str) -> Option<TradeMessage> {
    let parts: Vec<&str> = line.split(',').collect();
    if parts.len() < 7 { return None; }
    let price: f64 = parts[1].trim().parse().ok()?;
    let qty: f64 = parts[2].trim().parse().ok()?;
    let ts_raw: i64 = parts[5].trim().parse().ok()?;
    let ts = normalize_ts(ts_raw);
    let is_buyer_maker = parts[6].trim().eq_ignore_ascii_case("true");
    Some(TradeMessage {
        symbol: symbol.to_string(),
        price, volume: qty,
        timestamp: ts, event_time: ts,
        is_buyer_maker,
    })
}

fn parse_kline(line: &str, symbol: &str) -> Option<KlineMessage> {
    let parts: Vec<&str> = line.split(',').collect();
    if parts.len() < 11 { return None; }
    let open: f64 = parts[1].trim().parse().ok()?;
    let high: f64 = parts[2].trim().parse().ok()?;
    let low: f64 = parts[3].trim().parse().ok()?;
    let close: f64 = parts[4].trim().parse().ok()?;
    let volume: f64 = parts[5].trim().parse().ok()?;
    let close_time_raw: i64 = parts[6].trim().parse().ok()?;
    let taker_buy_volume: f64 = parts[9].trim().parse().unwrap_or(0.0);
    Some(KlineMessage {
        symbol: symbol.to_string(),
        open, high, low, close, volume,
        taker_buy_volume,
        timestamp: normalize_ts(close_time_raw),
        is_closed: true,
        is_backfill: false, // ✅ ДОБАВЛЕНО (было пропущено)
    })
}

fn parse_book_ticker(line: &str, symbol: &str) -> Option<BookTickerMessage> {
    let parts: Vec<&str> = line.split(',').collect();
    if parts.len() < 7 { return None; }
    let best_bid: f64 = parts[1].trim().parse().ok()?;
    let bid_qty: f64 = parts[2].trim().parse().unwrap_or(0.0);
    let best_ask: f64 = parts[3].trim().parse().ok()?;
    let ask_qty: f64 = parts[4].trim().parse().unwrap_or(0.0);
    let timestamp = normalize_ts(parts[6].trim().parse().ok()?);
    Some(BookTickerMessage {
        symbol: symbol.to_string(), best_bid, best_ask, bid_qty, ask_qty, timestamp,
    })
}

// ============================================================
// Exit logic — ✅ ИСПРАВЛЕНО: добавлен tick_size для слиппеджа
// ============================================================
fn check_exit(
    pos: &OpenPosition,
    price: f64,
    now_ms: i64,
    max_ms: i64,
    _tick_size: f64,
) -> Option<&'static str> {
    if now_ms - pos.opened_at_ms >= max_ms { return Some("Timeout"); }
    match pos.side.as_str() {
        "BUY" => {
            if let Some(sl) = pos.sl { if price <= sl { return Some("StopLoss"); } }
            if let Some(tp) = pos.tp { if price >= tp { return Some("TakeProfit"); } }
        }
        "SELL" => {
            if let Some(sl) = pos.sl { if price >= sl { return Some("StopLoss"); } }
            if let Some(tp) = pos.tp { if price <= tp { return Some("TakeProfit"); } }
        }
        _ => {}
    }
    None
}

// ✅ НОВАЯ ФУНКЦИЯ: расчёт цены выхода с учётом слиппеджа стопа
fn calc_exit_price(
    pos: &OpenPosition,
    market_price: f64,
    reason: &'static str,
    tick_size: f64,
) -> f64 {
    let slippage = SL_SLIPPAGE_TICKS * tick_size;
    match reason {
        "StopLoss" => {
            let sl = pos.sl.unwrap_or(market_price);
            // Слиппедж: стоп исполняется ХУЖЕ цены триггера
            match pos.side.as_str() {
                "BUY" => sl - slippage,   // Long SL: продаём ниже стопа
                "SELL" => sl + slippage,   // Short SL: покупаем выше стопа
                _ => sl,
            }
        }
        "TakeProfit" => pos.tp.unwrap_or(market_price),
        _ => market_price, // Timeout, EndOfData — по текущей цене
    }
}

// ============================================================
// ✅ ИСПРАВЛЕНО: раздельные maker/taker комиссии
// Вход всегда maker (GTX), выход maker (TP) или taker (SL/Timeout)
// ============================================================
fn close_position(
    pos: &OpenPosition,
    exit: f64,
    reason: &'static str,
    now_ms: i64,
    maker_fee: f64,
    taker_fee: f64,
    day_key: &str,
) -> ClosedTrade {
    let gross = match pos.side.as_str() {
        "BUY" => (exit - pos.entry) * pos.qty,
        "SELL" => (pos.entry - exit) * pos.qty,
        _ => 0.0,
    };

    // ✅ ИСПРАВЛЕНО: раздельные комиссии
    // Вход: всегда maker (GTX)
    let entry_fee = pos.entry * pos.qty * maker_fee;

    // Выход: maker при TP, taker при SL/Timeout
    let exit_fee = match reason {
        "TakeProfit" | "EndOfData" => exit * pos.qty * maker_fee,  // maker exit
        _ => exit * pos.qty * taker_fee,  // taker exit (SL, Timeout)
    };

    let fees = entry_fee + exit_fee;
    let net = gross - fees;

    ClosedTrade {
        side: pos.side.clone(),
        entry: pos.entry, exit, qty: pos.qty,
        pnl_gross: gross, fees, pnl_net: net,
        reason,
        duration_ms: now_ms - pos.opened_at_ms,
        day_key: day_key.to_string(),
    }
}

fn compute_stats(trades: &[ClosedTrade]) -> Stats {
    let n = trades.len();
    if n == 0 { return Stats::default(); }
    let wins: Vec<_> = trades.iter().filter(|t| t.pnl_net > 0.0).collect();
    let losses: Vec<_> = trades.iter().filter(|t| t.pnl_net < 0.0).collect();
    let gross_profit: f64 = wins.iter().map(|t| t.pnl_net).sum();
    let gross_loss: f64 = losses.iter().map(|t| t.pnl_net.abs()).sum();
    let win_rate = wins.len() as f64 / n as f64;
    let avg_win = if !wins.is_empty() { gross_profit / wins.len() as f64 } else { 0.0 };
    let avg_loss = if !losses.is_empty() { gross_loss / losses.len() as f64 } else { 0.0 };
    let ev = win_rate * avg_win - (1.0 - win_rate) * avg_loss;
    let pf = if gross_loss > 1e-9 { gross_profit / gross_loss } else if gross_profit > 0.0 { f64::INFINITY } else { 0.0 };
    let net_pnl: f64 = trades.iter().map(|t| t.pnl_net).sum();

    let mut cum = 0.0; let mut peak = 0.0; let mut max_dd = 0.0;
    for t in trades {
        cum += t.pnl_net;
        if cum > peak { peak = cum; }
        let dd = peak - cum;
        if dd > max_dd { max_dd = dd; }
    }

    Stats { n, wins: wins.len(), losses: losses.len(),
        win_rate, avg_win, avg_loss, ev, profit_factor: pf,
        net_pnl, gross_profit, gross_loss, max_dd }
}

fn print_report(stats: &Stats, trades: &[ClosedTrade], days: f64, msgs: u64, sigs: u64) {
    let mut reasons: std::collections::HashMap<&str, (usize, f64)> = std::collections::HashMap::new();
    for t in trades {
        let e = reasons.entry(t.reason).or_insert((0, 0.0));
        e.0 += 1; e.1 += t.pnl_net;
    }
    let longs: Vec<&ClosedTrade> = trades.iter().filter(|t| t.side == "BUY").collect();
    let shorts: Vec<&ClosedTrade> = trades.iter().filter(|t| t.side == "SELL").collect();
    let (lw, lp) = count_pnl(&longs);
    let (sw, sp) = count_pnl(&shorts);
    let tpd = stats.n as f64 / days.max(1.0);
    let avg_dur = if stats.n > 0 {
        trades.iter().map(|t| t.duration_ms).sum::<i64>() as f64 / stats.n as f64 / 1000.0
    } else { 0.0 };

    println!();
    println!("╔═══════════════════════════════════════════════════════════╗");
    println!("║                    BACKTEST REPORT                        ║");
    println!("╠═══════════════════════════════════════════════════════════╣");
    println!("║   Messages:          {:>15}                       ║", msgs);
    println!("║   Signals:           {:>15}                       ║", sigs);
    println!("║   Trades closed:     {:>15}                       ║", stats.n);
    println!("║   Days:              {:>15.2}                       ║", days);
    println!("║   Trades per day:    {:>15.2}                       ║", tpd);
    println!("╠═══════════════════════════════════════════════════════════╣");
    println!("║   Win Rate:          {:>15.2}%                      ║", stats.win_rate * 100.0);
    println!("║   Wins / Losses:     {:>7} / {:>7}                  ║", stats.wins, stats.losses);
    println!("║   Avg Win:           {:>15.6} USDT                 ║", stats.avg_win);
    println!("║   Avg Loss:          {:>15.6} USDT                 ║", stats.avg_loss);
    println!("║   EV per trade:      {:>15.6} USDT                 ║", stats.ev);
    println!("║   Profit Factor:     {:>15.2}                       ║", stats.profit_factor);
    println!("║   Net PnL:           {:>15.4} USDT                 ║", stats.net_pnl);
    println!("║   Max Drawdown:      {:>15.4} USDT                 ║", stats.max_dd);
    println!("║   Avg Duration:      {:>15.2} sec                  ║", avg_dur);
    println!("╠═══════════════════════════════════════════════════════════╣");
    println!("║   LONG:  {:>5} trades | {:>4} wins | PnL {:>+10.4}    ║", longs.len(), lw, lp);
    println!("║   SHORT: {:>5} trades | {:>4} wins | PnL {:>+10.4}    ║", shorts.len(), sw, sp);
    println!("╠═══════════════════════════════════════════════════════════╣");
    println!("║   Exit reasons:                                           ║");
    let mut sorted: Vec<_> = reasons.iter().collect();
    sorted.sort_by(|a, b| b.1.0.cmp(&a.1.0));
    for (reason, (count, pnl)) in sorted {
        println!("║     {:<14} {:>5} trades | PnL {:>+10.4} USDT  ║", reason, count, pnl);
    }
    println!("╚═══════════════════════════════════════════════════════════╝");

    println!("\n📋 VERDICT:");
    verdict("Win Rate", stats.win_rate * 100.0, 55.0, "%", true);
    verdict("Profit Factor", stats.profit_factor, 1.3, "", true);
    verdict("EV per trade", stats.ev, 0.000001, " USDT", true);
    verdict("Trades per day", tpd, 20.0, "", true);
}

fn print_breakdown(trades: &[ClosedTrade]) {
    if trades.is_empty() { return; }
    let mut by_day: std::collections::BTreeMap<String, Vec<&ClosedTrade>> = Default::default();
    for t in trades { by_day.entry(t.day_key.clone()).or_default().push(t); }

    println!("\n╔═══════════════════════════════════════════════════════════╗");
    println!("║                    DAILY BREAKDOWN                        ║");
    println!("╠═══════════════════════════════════════════════════════════╣");
    println!("║   {:<28} {:>5} {:>7} {:>10}  ║", "Day", "N", "WR%", "PnL");
    println!("╠═══════════════════════════════════════════════════════════╣");
    for (day, day_trades) in &by_day {
        let n = day_trades.len();
        let wins = day_trades.iter().filter(|t| t.pnl_net > 0.0).count();
        let wr = if n > 0 { wins as f64 / n as f64 * 100.0 } else { 0.0 };
        let pnl: f64 = day_trades.iter().map(|t| t.pnl_net).sum();
        let short = if day.len() > 28 { &day[..28] } else { day };
        println!("║   {:<28} {:>5} {:>6.1}% {:>+10.4}  ║", short, n, wr, pnl);
    }
    println!("╚═══════════════════════════════════════════════════════════╝");
}

fn count_pnl(trades: &[&ClosedTrade]) -> (usize, f64) {
    let wins = trades.iter().filter(|t| t.pnl_net > 0.0).count();
    let pnl = trades.iter().map(|t| t.pnl_net).sum();
    (wins, pnl)
}

fn verdict(name: &str, value: f64, threshold: f64, suffix: &str, higher_is_better: bool) {
    let pass = if higher_is_better { value >= threshold } else { value <= threshold };
    let mark = if pass { "✅" } else { "❌" };
    let op = if higher_is_better { "≥" } else { "≤" };
    println!("   {} {:<18} {:>15.6}{}  (target {} {:.6}{})", mark, name, value, suffix, op, threshold, suffix);
}