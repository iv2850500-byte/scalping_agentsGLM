#!/usr/bin/env python3
"""download_bybit_klines.py — загрузчик исторических свечей с Bybit V5 PROD."""
import argparse, csv, os, sys, time, datetime as dt, urllib.request, urllib.error, json
from pathlib import Path

FAPI_KLINES = "https://api.bybit.com/v5/market/kline"
INTERVAL_MAP = {"1m":"1","5m":"5","15m":"15","30m":"30","1h":"60","2h":"120","4h":"240","1d":"D","1w":"W","1M":"M"}
INTERVAL_SECONDS = {"1m":60,"5m":300,"15m":900,"30m":1800,"1h":3600,"2h":7200,"4h":14400,"1d":86400,"1w":604800,"1M":2592000}

def parse_args():
    p = argparse.ArgumentParser()
    p.add_argument("--symbol", required=True)
    p.add_argument("--interval", default="1h")
    p.add_argument("--days", type=int, default=365)
    p.add_argument("--start")
    p.add_argument("--end")
    p.add_argument("--out", default="data/klines")
    p.add_argument("--limit", type=int, default=1000)
    p.add_argument("--sleep_ms", type=int, default=300)
    p.add_argument("--retries", type=int, default=5)
    return p.parse_args()

def fetch_klines(symbol, interval_bybit, start_ms, end_ms, limit, retries):
    url = f"{FAPI_KLINES}?category=linear&symbol={symbol}&interval={interval_bybit}&start={start_ms}&end={end_ms}&limit={limit}"
    last_err = None
    for attempt in range(1, retries + 1):
        try:
            req = urllib.request.Request(url, headers={"User-Agent":"Mozilla/5.0","Accept":"application/json"})
            with urllib.request.urlopen(req, timeout=30) as resp:
                data = json.loads(resp.read().decode("utf-8"))
            if data.get("retCode", -1) != 0:
                raise RuntimeError(f"Bybit error retCode={data.get('retCode')} msg={data.get('retMsg')}")
            return data.get("result", {}).get("list", [])
        except urllib.error.HTTPError as e:
            body = e.read().decode("utf-8", errors="replace")[:500]
            last_err = RuntimeError(f"HTTP {e.code}: {body}")
            if e.code == 429:
                print(f"  ⚠ 429 rate limit, ждём {10*attempt}с"); time.sleep(10*attempt); continue
            if 500 <= e.code < 600:
                print(f"  ⚠ HTTP {e.code}, ждём {5*attempt}с"); time.sleep(5*attempt); continue
            raise last_err
        except Exception as e:
            last_err = RuntimeError(f"Network: {e}")
            print(f"  ⚠ {e}, ждём {3*attempt}с"); time.sleep(3*attempt)
    raise last_err or RuntimeError("unknown")

def bybit_row_to_csv(row, interval_secs):
    open_time_ms = int(row[0])
    close_time_ms = open_time_ms + interval_secs * 1000 - 1
    return [open_time_ms, row[1], row[2], row[3], row[4], row[5], close_time_ms, row[6], 0, 0, 0, 0]

def main():
    args = parse_args()
    if args.interval not in INTERVAL_MAP: sys.exit(f"Unknown interval: {args.interval}")
    interval_bybit = INTERVAL_MAP[args.interval]
    interval_secs = INTERVAL_SECONDS[args.interval]
    end_date = dt.datetime.strptime(args.end, "%Y-%m-%d").replace(tzinfo=dt.timezone.utc) if args.end else dt.datetime.now(dt.timezone.utc)
    start_date = dt.datetime.strptime(args.start, "%Y-%m-%d").replace(tzinfo=dt.timezone.utc) if args.start else end_date - dt.timedelta(days=args.days)
    end_ms = int(end_date.timestamp() * 1000)
    start_ms = int(start_date.timestamp() * 1000)
    print(f"=== Bybit klines ===\nSymbol: {args.symbol}\nInterval: {args.interval} → '{interval_bybit}' ({interval_secs}с)\nRange: {start_date.strftime('%Y-%m-%d')} → {end_date.strftime('%Y-%m-%d')}\n")
    out_dir = Path(args.out); out_dir.mkdir(parents=True, exist_ok=True)
    out_file = out_dir / f"{args.symbol}-{args.interval}-{start_date.strftime('%Y%m%d')}_{end_date.strftime('%Y%m%d')}.csv"
    all_rows = []
    cursor_end_ms = end_ms
    request_count = 0
    while cursor_end_ms > start_ms:
        request_count += 1
        rows = fetch_klines(args.symbol, interval_bybit, start_ms, cursor_end_ms, args.limit, args.retries)
        if not rows: print(f"  запрос {request_count}: 0 свечей — конец"); break
        rows.reverse()
        csv_rows = [bybit_row_to_csv(r, interval_secs) for r in rows]
        csv_rows = [r for r in csv_rows if start_ms <= r[0] <= end_ms]
        if all_rows:
            existing = {r[0] for r in all_rows}
            csv_rows = [r for r in csv_rows if r[0] not in existing]
        if not csv_rows: print(f"  запрос {request_count}: 0 новых — конец"); break
        all_rows.extend(csv_rows)
        oldest = csv_rows[0][0]; newest = csv_rows[-1][0]
        oldest_str = dt.datetime.fromtimestamp(oldest/1000, tz=dt.timezone.utc).strftime("%Y-%m-%d %H:%M")
        newest_str = dt.datetime.fromtimestamp(newest/1000, tz=dt.timezone.utc).strftime("%Y-%m-%d %H:%M")
        print(f"  запрос {request_count}: {len(csv_rows)} свечей | {oldest_str} → {newest_str} | итого {len(all_rows)}")
        if len(rows) < args.limit: break
        cursor_end_ms = oldest - 1
        time.sleep(args.sleep_ms / 1000.0)
    if not all_rows: sys.exit("❌ Нет свечей")
    all_rows.sort(key=lambda r: r[0])
    with open(out_file, "w", newline="", encoding="utf-8") as f:
        w = csv.writer(f)
        w.writerow(["open_time","open","high","low","close","volume","close_time","quote_volume","count","taker_buy_volume","taker_buy_quote_volume","ignore"])
        w.writerows(all_rows)
    print(f"\n✅ Скачано {len(all_rows)} свечей в {out_file}")
    closes = [float(r[4]) for r in all_rows]
    print(f"   Цена: min={min(closes):.2f} max={max(closes):.2f} avg={sum(closes)/len(closes):.2f}")

if __name__ == "__main__": main()