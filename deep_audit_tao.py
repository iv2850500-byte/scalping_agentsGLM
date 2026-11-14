#!/usr/bin/env python3
import os, sys, sqlite3
from pathlib import Path

def find_db():
    if len(sys.argv) > 1:
        return sys.argv[1]
    data_dir = Path("data")
    if not data_dir.exists():
        print("Нет папки data/. Запусти из корня проекта."); sys.exit(1)
    cands = sorted(data_dir.glob("*.db"), key=lambda p: p.stat().st_mtime, reverse=True)
    if not cands:
        print("В data/ нет .db файлов"); sys.exit(1)
    print("Доступные БД:")
    for i, c in enumerate(cands):
        print(f"  [{i}] {c.name}  ({c.stat().st_size/1024:.1f} KB)")
    print(f"\n→ Использую: {cands[0]}\n")
    return str(cands[0])

def q(conn, sql, title):
    print("=" * 70); print(f"=== {title} ==="); print("=" * 70)
    try:
        cur = conn.execute(sql); cols = [d[0] for d in cur.description]; rows = cur.fetchall()
        if not rows: print("  (нет данных)\n"); return
        widths = [max(len(str(c)), max((len(str(r[i])) for r in rows), default=0)) for i, c in enumerate(cols)]
        print(" | ".join(str(c).ljust(w) for c, w in zip(cols, widths))); print("-" * 70)
        for r in rows: print(" | ".join(str(v).ljust(w) for v, w in zip(r, widths)))
        print()
    except Exception as e: print(f"Ошибка SQL: {e}\n")

db = find_db()
conn = sqlite3.connect(db)

# Схема
print("=" * 70); print("=== СХЕМА ТАБЛИЦЫ TRADES ==="); print("=" * 70)
row = conn.execute("SELECT sql FROM sqlite_master WHERE name='trades'").fetchone()
print(row[0] if row else "Таблица trades не найдена"); print()

q(conn, """SELECT substr(opened_at,1,19) opened, substr(closed_at,1,19) closed, side,
    printf('%.4f',entry_price) entry, printf('%.4f',exit_price) exit_p,
    printf('%.2f',(exit_price-entry_price)*CASE WHEN side='LONG' THEN 1 ELSE -1 END) move,
    printf('%.4f',pnl_net) pnl, reason
    FROM trades ORDER BY closed_at DESC LIMIT 20""", "ПОСЛЕДНИЕ 20 СДЕЛОК")

q(conn, """SELECT strftime('%H',opened_at) hour_utc, COUNT(*) n,
    SUM(CASE WHEN pnl_net>0 THEN 1 ELSE 0 END) wins,
    SUM(CASE WHEN pnl_net<=0 THEN 1 ELSE 0 END) losses,
    printf('%.2f',AVG(pnl_net)) avg_pnl, printf('%.2f',SUM(pnl_net)) sum_pnl
    FROM trades GROUP BY hour_utc ORDER BY hour_utc""", "РАСПРЕДЕЛЕНИЕ ПО ЧАСУ UTC")

q(conn, """SELECT CASE strftime('%w',opened_at) WHEN '0' THEN 'Sun' WHEN '1' THEN 'Mon'
    WHEN '2' THEN 'Tue' WHEN '3' THEN 'Wed' WHEN '4' THEN 'Thu' WHEN '5' THEN 'Fri' WHEN '6' THEN 'Sat' END day,
    COUNT(*) n, SUM(CASE WHEN pnl_net>0 THEN 1 ELSE 0 END) wins, printf('%.2f',SUM(pnl_net)) pnl
    FROM trades GROUP BY day ORDER BY day""", "ПО ДНЮ НЕДЕЛИ")

q(conn, """SELECT reason, COUNT(*) n,
    printf('%.1f',AVG((julianday(closed_at)-julianday(opened_at))*24*60)) avg_min,
    printf('%.1f',MIN((julianday(closed_at)-julianday(opened_at))*24*60)) min_min,
    printf('%.1f',MAX((julianday(closed_at)-julianday(opened_at))*24*60)) max_min,
    printf('%.4f',AVG(pnl_net)) avg_pnl
    FROM trades GROUP BY reason ORDER BY avg_min""", "ВРЕМЯ УДЕРЖАНИЯ ПО ПРИЧИНЕ (мин)")

q(conn, """SELECT side, COUNT(*) n,
    SUM(CASE WHEN pnl_net>0 THEN 1 ELSE 0 END) wins,
    printf('%.1f',100.0*SUM(CASE WHEN pnl_net>0 THEN 1 ELSE 0 END)/COUNT(*)) wr_pct,
    printf('%.4f',AVG(pnl_net)) avg_pnl, printf('%.4f',SUM(pnl_net)) total_pnl
    FROM trades GROUP BY side""", "LONG vs SHORT")

q(conn, """WITH o AS (SELECT *, ROW_NUMBER() OVER (ORDER BY opened_at) rn FROM trades)
    SELECT substr(opened_at,1,19) loss_time, printf('%.4f',pnl_net) pnl,
    (SELECT COUNT(*) FROM o b WHERE b.rn<a.rn AND b.rn>=a.rn-4 AND b.pnl_net<0) losses_in_prev_5
    FROM o a WHERE pnl_net<0 ORDER BY losses_in_prev_5 DESC LIMIT 15""", "ТОП-15 ЛОССОВ С ПРЕДШЕСТВУЮЩИМИ ЛОССАМИ")

q(conn, """SELECT COUNT(*) total_trades,
    printf('%.1f%%',100.0*SUM(CASE WHEN pnl_net>0 THEN 1 ELSE 0 END)/COUNT(*)) win_rate,
    printf('%.2f',SUM(pnl_net)) total_pnl,
    printf('%.4f',AVG(CASE WHEN pnl_net>0 THEN pnl_net END)) avg_win,
    printf('%.4f',AVG(CASE WHEN pnl_net<0 THEN pnl_net END)) avg_loss,
    printf('%.2f',SUM(CASE WHEN pnl_net>0 THEN pnl_net ELSE 0 END)/ABS(SUM(CASE WHEN pnl_net<0 THEN pnl_net ELSE 0 END))) profit_factor
    FROM trades""", "СВОДКА")

print("\n✅ Аудит завершён.")
