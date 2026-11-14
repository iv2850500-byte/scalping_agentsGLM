# P49/k6-preflight tool, rev P50. Mirrors the Rust K6 checks (P42/P45 semantics):
# budget base = [global].account_capital_usdt (>0) else SUM of initial_balance.
# Unset max_daily_loss_amount/initial_balance fall back to Rust defaults inside
# the bot and are counted here as 0 with a label - set them explicitly for exact math.
# [P50] fixes: stray 'Stop' line removed; bot split tolerant to leading spaces.
Set-Location 'C:\Users\BASE_PC\scalping_agentsGLM'
 $t = [IO.File]::ReadAllText((Join-Path (Get-Location) 'config.toml'))
function GetNum([string]$text, [string]$key) {
    $m = [regex]::Match($text, '(?m)^\s*' + [regex]::Escape($key) + '\s*=\s*([0-9]+(?:\.[0-9]+)?)')
    if ($m.Success) { return [double]$m.Groups[1].Value } else { return $null }
}
 $acct = GetNum $t 'account_capital_usdt'
 $mcp  = GetNum $t 'max_concurrent_positions'
 $botSecs = ($t -split '(?m)^[ \t]*\[\[bots\]\]') | Select-Object -Skip 1
 $sumDaily = 0.0; $sumBal = 0.0; $rows = @()
foreach ($s in $botSecs) {
    $name = [regex]::Match($s, '(?m)^\s*name\s*=\s*"([^"]+)"').Groups[1].Value
    $d = GetNum $s 'max_daily_loss_amount'
    $b = GetNum $s 'initial_balance'
    $dTxt = 'UNSET(Rust default)'; $dVal = 0.0; if ($null -ne $d) { $dTxt = '{0}' -f $d; $dVal = $d }
    $bTxt = 'UNSET(Rust default)'; $bVal = 0.0; if ($null -ne $b) { $bTxt = '{0}' -f $b; $bVal = $b }
    $rows += ('  {0,-14} daily={1,-20} initial_balance={2}' -f $name, $dTxt, $bTxt)
    $sumDaily += $dVal; $sumBal += $bVal
}
Write-Host ("bots: {0}" -f $botSecs.Count)
 $rows | ForEach-Object { Write-Host $_ }
if ($null -ne $mcp) { Write-Host ("max_concurrent_positions = {0}" -f $mcp) } else { Write-Host 'max_concurrent_positions: UNSET' }
if ($null -ne $acct) { Write-Host ("account_capital_usdt = {0}" -f $acct) } else { Write-Host 'account_capital_usdt: UNSET' }
Write-Host ("sum(max_daily_loss_amount) = {0:0.00} USDT (unset counted as 0)" -f $sumDaily)
 $base = $sumBal; if ($null -ne $acct -and $acct -gt 0) { $base = $acct }
if ($base -gt 0) {
    $lim = $base * 0.05
    if ($sumDaily -gt $lim) {
        Write-Host ("K6 PREDICTION: WARN WILL FIRE: daily budget {0:0.00} > 5% of base {1:0.00} (= {2:0.00})" -f $sumDaily, $base, $lim)
    } else {
        Write-Host ("K6 PREDICTION: silent: daily budget {0:0.00} <= 5% of base {1:0.00} (= {2:0.00})" -f $sumDaily, $base, $lim)
    }
}
if ($null -ne $mcp -and $botSecs.Count -gt 1 -and $mcp -lt $botSecs.Count) {
    Write-Host ("K6 PREDICTION: WARN WILL FIRE: max_concurrent_positions ({0}) < bots ({1})" -f $mcp, $botSecs.Count)
} else {
    Write-Host 'K6 PREDICTION: concurrency ok'
}