//! resample_klines — ресэмплинг свечей из 1h в 4h/1d.
//! Запуск: cargo run --release --bin resample_klines -- data/klines/XAUUSDT-1h-...csv --to 4h --out data/klines/XAUUSDT-4h-...csv

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufWriter, Write};
use std::path::PathBuf;

fn interval_seconds(interval: &str) -> Option<i64> {
    match interval {
        "1m" => Some(60), "5m" => Some(300), "15m" => Some(900), "30m" => Some(1800),
        "1h" => Some(3600), "2h" => Some(7200), "4h" => Some(14400),
        "1d" => Some(86400), "1w" => Some(604800), _ => None,
    }
}

fn parse_args() -> Result<(PathBuf, String, PathBuf), String> {
    let args: Vec<String> = std::env::args().collect();
    let mut input: Option<PathBuf> = None;
    let mut target: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--to" => { target = Some(args.get(i+1).ok_or("missing target")?.clone()); i+=2; }
            "--out" => { out = Some(PathBuf::from(args.get(i+1).ok_or("missing out")?)); i+=2; }
            x if !x.starts_with('-') => { input = Some(PathBuf::from(x)); i+=1; }
            _ => { i+=1; }
        }
    }
    let input = input.ok_or("input file required")?;
    let target = target.ok_or("--to required")?;
    let out_path = out.unwrap_or_else(|| {
        let name = input.file_name().unwrap().to_str().unwrap();
        let mut new_name = String::new();
        for src_tf in &["1m","5m","15m","30m","1h","2h","4h","1d","1w"] {
            let token = format!("-{}-", src_tf);
            if name.contains(&token) {
                new_name = name.replace(&token, &format!("-{}-", target));
                break;
            }
        }
        if new_name.is_empty() { new_name = format!("{}-{}.csv", name.trim_end_matches(".csv"), target); }
        input.with_file_name(new_name)
    });
    Ok((input, target, out_path))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (in_path, target_str, out_path) = parse_args()?;
    let target_secs = interval_seconds(&target_str).ok_or(format!("Bad target: {}", target_str))?;

    println!("Resampling: {} → {} ({})", in_path.display(), out_path.display(), target_str);
    println!("Target interval: {} = {}с", target_str, target_secs);

    let f = File::open(&in_path)?;
    let reader = std::io::BufReader::new(f);
    let mut lines = reader.lines();
    let header = lines.next().ok_or("empty file")??;

    let mut rows: Vec<Vec<String>> = Vec::new();
    for line in lines {
        let line = line?;
        let cols: Vec<String> = line.split(',').map(|s| s.to_string()).collect();
        if cols.len() >= 7 { rows.push(cols); }
    }

    println!("Input: {} candles", rows.len());
    if rows.len() < 2 { return Err("Not enough data".into()); }

    let t0: i64 = rows[0][0].parse().unwrap_or(0);
    let t1: i64 = rows[1][0].parse().unwrap_or(t0 + 3600000);
    let src_secs = ((t1 - t0) / 1000).max(60);
    println!("Source interval: {}с", src_secs);

    if target_secs <= src_secs {
        return Err(format!("Target {} must be > source {}с", target_str, src_secs).into());
    }

    let bucket_size_ms = target_secs * 1000;
    let mut groups: HashMap<i64, (String, f64, f64, String, f64, f64, i64, String, i64)> = HashMap::new();

    for r in &rows {
        let open_time: i64 = r[0].parse().unwrap_or(0);
        let bucket_start = (open_time / bucket_size_ms) * bucket_size_ms;
        let entry = groups.entry(bucket_start).or_insert_with(|| {
            (r[1].clone(), f64::NEG_INFINITY, f64::INFINITY, r[4].clone(), 0.0, 0.0, 0, String::new(), 0)
        });
        let h: f64 = r[2].parse().unwrap_or(0.0);
        if h > entry.1 { entry.1 = h; }
        let l: f64 = r[3].parse().unwrap_or(f64::INFINITY);
        if l < entry.2 { entry.2 = l; }
        entry.3 = r[4].clone();  // close = last
        entry.4 += r[5].parse::<f64>().unwrap_or(0.0);  // volume sum
        entry.5 += r.get(7).and_then(|s| s.parse().ok()).unwrap_or(0.0);  // quote_volume
        entry.7 = r.get(7).cloned().unwrap_or_default();  // keep turnover as is for simplicity
        entry.8 += r.get(8).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);  // count
    }

    let mut sorted: Vec<_> = groups.into_iter().collect();
    sorted.sort_by_key(|(k, _)| *k);

    let mut f = BufWriter::new(File::create(&out_path)?);
    writeln!(f, "{}", header)?;
    for (bucket_start, g) in &sorted {
        let close_time = bucket_start + target_secs * 1000 - 1;
        writeln!(f, "{},{},{:.8},{:.8},{},{:.8},{},{:.8},{},{:.8},{:.8},0",
                 bucket_start, g.0, g.1, g.2, g.3, g.4, close_time, g.5, g.8, 0.0, 0.0)?;
    }

    println!();
    println!("✅ Resampled {} candles {} → {}", sorted.len(), target_str, out_path.display());
    Ok(())
}