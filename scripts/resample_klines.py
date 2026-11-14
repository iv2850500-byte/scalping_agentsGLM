#!/usr/bin/env python3
"""resample_klines.py — ресэмплинг свечей из 1h в 4h/1d."""
import argparse, csv, sys
from pathlib import Path
from datetime import datetime, timezone

INTERVAL_SECONDS = {"1m":60,"5m":300,"15m":900,"30m":1800,"1h":3600,"2h":7200,"4h":14400,"1d":86400,"1w":604800}

def parse_args():
    p = argparse.ArgumentParser()
    p.add_argument("input")
    p.add_argument("--to", required=True, choices=list(INTERVAL_SECONDS.keys()))
    p.add_argument("--out")
    return p.parse_args()

def main():
    args = parse_args()
    target_secs = INTERVAL_SECONDS[args.to]
    in_path = Path(args.input)
    if not in_path.exists(): sys.exit(f"❌ Not found: {in_path}")
    if args.out:
        out_path = Path(args.out)
    else:
        name = in_path.name
        for src_tf in INTERVAL_SECONDS:
            token = f"-{src_tf}-"
            if token in name: name = name.replace(token, f"-{args.to}-"); break
        out_path = in_path.parent / name
    print(f"Resampling: {in_path.name} → {out_path.name} ({args.to})")
    with open(in_path, "r", newline="", encoding="utf-8") as f:
        reader = csv.reader(f)
        header = next(reader)
        rows = list(reader)
    print(f"Input: {len(rows)} candles")
    if not rows: sys.exit("❌ No data")
    t0 = int(rows[0][0])
    t1 = int(rows[1][0]) if len(rows) > 1 else t0 + 3600000
    src_secs = (t1 - t0) // 1000 or 3600
    print(f"Source interval: {src_secs}с")
    if target_secs <= src_secs: sys.exit(f"❌ Target {args.to} must be > source {src_secs}с")
    groups = {}
    for r in rows:
        open_time = int(r[0])
        bucket_size_ms = target_secs * 1000
        bucket_start = (open_time // bucket_size_ms) * bucket_size_ms
        if bucket_start not in groups:
            groups[bucket_start] = {"open":r[1],"high":float(r[2]),"low":float(r[3]),"close":r[4],"volume":0.0,"quote_volume":0.0,"count":0,"taker_buy_volume":0.0,"taker_buy_quote_volume":0.0,"_n":0}
        g = groups[bucket_start]
        h = float(r[2])
        if h > g["high"]: g["high"] = h
        l = float(r[3])
        if g["_n"] == 0 or l < g["low"]: g["low"] = l
        g["close"] = r[4]
        g["volume"] += float(r[5])
        g["quote_volume"] += float(r[7]) if len(r) > 7 else 0.0
        g["count"] += int(r[8]) if len(r) > 8 else 0
        g["taker_buy_volume"] += float(r[9]) if len(r) > 9 else 0.0
        g["taker_buy_quote_volume"] += float(r[10]) if len(r) > 10 else 0.0
        g["_n"] += 1
    sorted_buckets = sorted(groups.items())
    with open(out_path, "w", newline="", encoding="utf-8") as f:
        w = csv.writer(f)
        w.writerow(header)
        for bucket_start, g in sorted_buckets:
            close_time = bucket_start + target_secs * 1000 - 1
            w.writerow([bucket_start, g["open"], f"{g['high']:.8f}", f"{g['low']:.8f}", g["close"], f"{g['volume']:.8f}", close_time, f"{g['quote_volume']:.8f}", g["count"], f"{g['taker_buy_volume']:.8f}", f"{g['taker_buy_quote_volume']:.8f}", 0])
    print(f"\n✅ {len(sorted_buckets)} свечей {args.to} → {out_path}")
    if sorted_buckets:
        first = datetime.fromtimestamp(sorted_buckets[0][0]/1000, tz=timezone.utc)
        last = datetime.fromtimestamp(sorted_buckets[-1][0]/1000, tz=timezone.utc)
        print(f"   Период: {first.strftime('%Y-%m-%d %H:%M')} → {last.strftime('%Y-%m-%d %H:%M')} UTC")

if __name__ == "__main__": main()