.bail off
ATTACH DATABASE 'C:/Users/BASE_PC/scalping_agentsGLM/reports/pnl_20261028_130133/state_btc.db' AS dbx_state_btc;
ATTACH DATABASE 'C:/Users/BASE_PC/scalping_agentsGLM/reports/pnl_20261028_130133/state_gold.db' AS dbx_state_gold;
ATTACH DATABASE 'C:/Users/BASE_PC/scalping_agentsGLM/reports/pnl_20261028_130133/state_gold_dca.db' AS dbx_state_gold_dca;
ATTACH DATABASE 'C:/Users/BASE_PC/scalping_agentsGLM/reports/pnl_20261028_130133/state_inj.db' AS dbx_state_inj;
ATTACH DATABASE 'C:/Users/BASE_PC/scalping_agentsGLM/reports/pnl_20261028_130133/state_pepe.db' AS dbx_state_pepe;
ATTACH DATABASE 'C:/Users/BASE_PC/scalping_agentsGLM/reports/pnl_20261028_130133/state_tao_v3.db' AS dbx_state_tao_v3;

.headers on
.mode csv
.once 'C:/Users/BASE_PC/scalping_agentsGLM/reports/pnl_20261028_130133/01_per_bot_summary.csv'
SELECT bot_name,
       COUNT(*) AS trades,
       SUM(CASE WHEN pnl > 0 THEN 1 ELSE 0 END) AS wins,
       SUM(CASE WHEN pnl < 0 THEN 1 ELSE 0 END) AS losses,
       ROUND(100.0 * SUM(CASE WHEN pnl > 0 THEN 1 ELSE 0 END)
             / NULLIF(SUM(CASE WHEN pnl <> 0 THEN 1 ELSE 0 END), 0), 1) AS winrate_pct,
       ROUND(SUM(CASE WHEN pnl > 0 THEN pnl ELSE 0 END), 4) AS gross_profit,
       ROUND(-SUM(CASE WHEN pnl < 0 THEN pnl ELSE 0 END), 4) AS gross_loss,
       ROUND(SUM(CASE WHEN pnl > 0 THEN pnl ELSE 0 END)
             / NULLIF(-SUM(CASE WHEN pnl < 0 THEN pnl ELSE 0 END), 0), 2) AS profit_factor,
       ROUND(SUM(pnl), 4) AS net_pnl,
       ROUND(SUM(pnl) / COUNT(*), 4) AS expectancy,
       ROUND(MAX(pnl), 4) AS best_trade,
       ROUND(MIN(pnl), 4) AS worst_trade,
       MIN(substr(timestamp, 1, 10)) AS first_trade,
       MAX(substr(timestamp, 1, 10)) AS last_trade
FROM main.trade_journal
WHERE event = 'CLOSE'
GROUP BY bot_name
ORDER BY net_pnl DESC;

.once 'C:/Users/BASE_PC/scalping_agentsGLM/reports/pnl_20261028_130133/02_funnel.csv'
SELECT bot_name,
       SUM(CASE WHEN event = 'SIGNAL' THEN 1 ELSE 0 END) AS signals,
       SUM(CASE WHEN event = 'EXECUTION' THEN 1 ELSE 0 END) AS executions,
       SUM(CASE WHEN event = 'CLOSE' THEN 1 ELSE 0 END) AS closes,
       SUM(CASE WHEN event = 'EXECUTION' AND details = 'PLACED' THEN 1 ELSE 0 END) AS placed,
       SUM(CASE WHEN event = 'EXECUTION' AND details = 'MISSED' THEN 1 ELSE 0 END) AS missed
FROM main.trade_journal
WHERE 1 = 1
GROUP BY bot_name
ORDER BY bot_name;

.once 'C:/Users/BASE_PC/scalping_agentsGLM/reports/pnl_20261028_130133/03_execution_status.csv'
SELECT bot_name, COALESCE(details, '') AS status, COUNT(*) AS cnt
FROM main.trade_journal
WHERE event = 'EXECUTION'
GROUP BY bot_name, details
ORDER BY bot_name, cnt DESC;

.once 'C:/Users/BASE_PC/scalping_agentsGLM/reports/pnl_20261028_130133/04_daily_pnl_bot.csv'
SELECT substr(timestamp, 1, 10) AS day,
       bot_name,
       COUNT(*) AS closes,
       ROUND(SUM(pnl), 4) AS net_pnl,
       ROUND(SUM(CASE WHEN pnl > 0 THEN pnl ELSE 0 END), 4) AS gross_profit,
       ROUND(-SUM(CASE WHEN pnl < 0 THEN pnl ELSE 0 END), 4) AS gross_loss
FROM main.trade_journal
WHERE event = 'CLOSE'
GROUP BY day, bot_name
ORDER BY day, bot_name;

.once 'C:/Users/BASE_PC/scalping_agentsGLM/reports/pnl_20261028_130133/05_daily_pnl_portfolio.csv'
SELECT substr(timestamp, 1, 10) AS day,
       COUNT(*) AS closes,
       ROUND(SUM(pnl), 4) AS net_pnl,
       ROUND(SUM(CASE WHEN pnl > 0 THEN pnl ELSE 0 END), 4) AS gross_profit,
       ROUND(-SUM(CASE WHEN pnl < 0 THEN pnl ELSE 0 END), 4) AS gross_loss
FROM main.trade_journal
WHERE event = 'CLOSE'
GROUP BY day
ORDER BY day;

.once 'C:/Users/BASE_PC/scalping_agentsGLM/reports/pnl_20261028_130133/06_equity_curve.csv'
WITH raw AS (
    SELECT bot_name, id, timestamp, pnl
    FROM main.trade_journal
    WHERE event = 'CLOSE'
    UNION ALL
    SELECT bot_name, 0 AS id, '' AS timestamp, 0.0 AS pnl
    FROM (SELECT DISTINCT bot_name FROM main.trade_journal WHERE event = 'CLOSE')
),
cum AS (
    SELECT bot_name, id, timestamp, pnl,
           SUM(pnl) OVER (PARTITION BY bot_name ORDER BY id, timestamp) AS cum_pnl
    FROM raw
),
eq AS (
    SELECT bot_name, id, timestamp, pnl, cum_pnl,
           100 + cum_pnl AS equity,
           MAX(100 + cum_pnl) OVER (
               PARTITION BY bot_name ORDER BY id, timestamp
               ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS peak
    FROM cum
)
SELECT bot_name, id, timestamp,
       ROUND(pnl, 4) AS pnl,
       ROUND(equity, 4) AS equity,
       ROUND(peak, 4) AS peak,
       ROUND(peak - equity, 4) AS drawdown,
       ROUND(100.0 * (peak - equity) / NULLIF(peak, 0), 2) AS drawdown_pct
FROM eq
WHERE id > 0
ORDER BY bot_name, id, timestamp;

.once 'C:/Users/BASE_PC/scalping_agentsGLM/reports/pnl_20261028_130133/07_max_drawdown.csv'
WITH raw AS (
    SELECT bot_name, id, timestamp, pnl
    FROM main.trade_journal
    WHERE event = 'CLOSE'
    UNION ALL
    SELECT bot_name, 0 AS id, '' AS timestamp, 0.0 AS pnl
    FROM (SELECT DISTINCT bot_name FROM main.trade_journal WHERE event = 'CLOSE')
),
cum AS (
    SELECT bot_name, id, timestamp, pnl,
           SUM(pnl) OVER (PARTITION BY bot_name ORDER BY id, timestamp) AS cum_pnl
    FROM raw
),
eq AS (
    SELECT bot_name, id, timestamp, pnl, cum_pnl,
           100 + cum_pnl AS equity,
           MAX(100 + cum_pnl) OVER (
               PARTITION BY bot_name ORDER BY id, timestamp
               ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS peak
    FROM cum
),
dd AS (
    SELECT bot_name, id, timestamp, pnl, equity, peak, peak - equity AS drawdown
    FROM eq
    WHERE id > 0
),
ranked AS (
    SELECT bot_name, id, timestamp, pnl,
           ROUND(equity, 4) AS equity_at,
           ROUND(peak, 4) AS peak_at,
           ROUND(drawdown, 4) AS max_dd,
           ROW_NUMBER() OVER (PARTITION BY bot_name ORDER BY drawdown DESC, id ASC) AS rn
    FROM dd
)
SELECT bot_name, max_dd, timestamp AS at_time, pnl AS trade_pnl, equity_at, peak_at
FROM ranked
WHERE rn = 1
ORDER BY bot_name;

.once 'C:/Users/BASE_PC/scalping_agentsGLM/reports/pnl_20261028_130133/08_risk_stats_recon.csv'
WITH rs_raw AS (SELECT bot_name, realized_pnl, total_trades, win_trades, loss_trades, daily_loss, stat_day, suspended, suspend_reason, 0 AS src_prio FROM dbx_state_btc.risk_stats UNION ALL SELECT bot_name, realized_pnl, total_trades, win_trades, loss_trades, daily_loss, stat_day, suspended, suspend_reason, 1 AS src_prio FROM dbx_state_gold.risk_stats UNION ALL SELECT bot_name, realized_pnl, total_trades, win_trades, loss_trades, daily_loss, stat_day, suspended, suspend_reason, 2 AS src_prio FROM dbx_state_gold_dca.risk_stats UNION ALL SELECT bot_name, realized_pnl, total_trades, win_trades, loss_trades, daily_loss, stat_day, suspended, suspend_reason, 3 AS src_prio FROM dbx_state_inj.risk_stats UNION ALL SELECT bot_name, realized_pnl, total_trades, win_trades, loss_trades, daily_loss, stat_day, suspended, suspend_reason, 4 AS src_prio FROM dbx_state_pepe.risk_stats UNION ALL SELECT bot_name, realized_pnl, total_trades, win_trades, loss_trades, daily_loss, stat_day, suspended, suspend_reason, 5 AS src_prio FROM dbx_state_tao_v3.risk_stats), rs AS (SELECT bot_name, realized_pnl, total_trades, win_trades, loss_trades, daily_loss, stat_day, suspended, suspend_reason FROM (SELECT *, ROW_NUMBER() OVER (PARTITION BY bot_name ORDER BY src_prio) AS rn FROM rs_raw) WHERE rn = 1)
SELECT j.bot_name,
       ROUND(j.journal_net, 4) AS journal_net_pnl,
       j.journal_closes,
       r.realized_pnl AS risk_realized_pnl,
       r.total_trades AS risk_total_trades,
       r.win_trades AS risk_wins,
       r.loss_trades AS risk_losses,
       r.daily_loss AS risk_daily_loss,
       r.stat_day AS risk_stat_day,
       r.suspended AS suspended,
       r.suspend_reason
FROM (
    SELECT bot_name, SUM(pnl) AS journal_net, COUNT(*) AS journal_closes
    FROM main.trade_journal
    WHERE event = 'CLOSE'
    GROUP BY bot_name
) j
LEFT JOIN rs r ON r.bot_name = j.bot_name
ORDER BY j.bot_name;

.once 'C:/Users/BASE_PC/scalping_agentsGLM/reports/pnl_20261028_130133/09_open_positions.csv'
SELECT 'state_btc' AS source_db, bot_name, symbol, side, entry_price, quantity, COALESCE(stop_loss, '') AS stop_loss, COALESCE(take_profit, '') AS take_profit, opened_at, COALESCE(origin_strategy, '') AS origin_strategy, COALESCE(funding_accrued, 0) AS funding_accrued, COALESCE(fees_paid, 0) AS fees_paid, COALESCE(partial_taken, 0) AS partial_taken FROM dbx_state_btc.positions UNION ALL SELECT 'state_gold' AS source_db, bot_name, symbol, side, entry_price, quantity, COALESCE(stop_loss, '') AS stop_loss, COALESCE(take_profit, '') AS take_profit, opened_at, COALESCE(origin_strategy, '') AS origin_strategy, COALESCE(funding_accrued, 0) AS funding_accrued, COALESCE(fees_paid, 0) AS fees_paid, COALESCE(partial_taken, 0) AS partial_taken FROM dbx_state_gold.positions UNION ALL SELECT 'state_gold_dca' AS source_db, bot_name, symbol, side, entry_price, quantity, COALESCE(stop_loss, '') AS stop_loss, COALESCE(take_profit, '') AS take_profit, opened_at, COALESCE(origin_strategy, '') AS origin_strategy, COALESCE(funding_accrued, 0) AS funding_accrued, COALESCE(fees_paid, 0) AS fees_paid, COALESCE(partial_taken, 0) AS partial_taken FROM dbx_state_gold_dca.positions UNION ALL SELECT 'state_inj' AS source_db, bot_name, symbol, side, entry_price, quantity, COALESCE(stop_loss, '') AS stop_loss, COALESCE(take_profit, '') AS take_profit, opened_at, COALESCE(origin_strategy, '') AS origin_strategy, COALESCE(funding_accrued, 0) AS funding_accrued, COALESCE(fees_paid, 0) AS fees_paid, COALESCE(partial_taken, 0) AS partial_taken FROM dbx_state_inj.positions UNION ALL SELECT 'state_pepe' AS source_db, bot_name, symbol, side, entry_price, quantity, COALESCE(stop_loss, '') AS stop_loss, COALESCE(take_profit, '') AS take_profit, opened_at, COALESCE(origin_strategy, '') AS origin_strategy, COALESCE(funding_accrued, 0) AS funding_accrued, COALESCE(fees_paid, 0) AS fees_paid, COALESCE(partial_taken, 0) AS partial_taken FROM dbx_state_pepe.positions UNION ALL SELECT 'state_tao_v3' AS source_db, bot_name, symbol, side, entry_price, quantity, COALESCE(stop_loss, '') AS stop_loss, COALESCE(take_profit, '') AS take_profit, opened_at, COALESCE(origin_strategy, '') AS origin_strategy, COALESCE(funding_accrued, 0) AS funding_accrued, COALESCE(fees_paid, 0) AS fees_paid, COALESCE(partial_taken, 0) AS partial_taken FROM dbx_state_tao_v3.positions ORDER BY bot_name, symbol;

.once 'C:/Users/BASE_PC/scalping_agentsGLM/reports/pnl_20261028_130133/10_recent_trades.csv'
SELECT id, bot_name, timestamp, symbol, side, price, quantity, pnl,
       COALESCE(details, '') AS details
FROM main.trade_journal
WHERE event = 'CLOSE'
ORDER BY id DESC
LIMIT 20;
