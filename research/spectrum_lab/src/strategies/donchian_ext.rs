//! P70: H7-H9 — финалисты с edge (PF>1.1 n≥100)
//! H7 INJ donch10, H8 XAU close_try 20_1.5 adx12+vol1.2, H9 портфель noEMA 15+25 ADX8
use crate::{Bar, SignalHint, DonchianGen};
use crate::indicators::{adx_series, ema_series};

fn vol_filter(bars: &[Bar], idx: usize, mult: f64) -> bool {
    if idx < 20 { return true; }
    let ma: f64 = bars[idx-20..idx].iter().map(|b| b.volume).sum::<f64>() / 20.0;
    if ma == 0.0 { return true; }
    bars[idx].volume >= ma * mult
}

// H7 — INJ donch10 + maker, без EMA (как в backtest_lab: Donchian 10 tp1.5)
pub fn h7_inj_donch10(bars: &[Bar]) -> Vec<SignalHint> {
    DonchianGen{ period:10, tp_mult:1.5 }.gen(bars)
}

// H8 — XAU close_try 20_1.5 + ADX12 + vol1.2 (PF1.81 n104)
pub fn h8_xau_close_try20_adx12(bars: &[Bar]) -> Vec<SignalHint> {
    let sigs = DonchianGen{ period:20, tp_mult:1.5 }.gen(bars);
    let closes: Vec<f64> = bars.iter().map(|b| b.close).collect();
    let highs: Vec<f64> = bars.iter().map(|b| b.high).collect();
    let lows: Vec<f64> = bars.iter().map(|b| b.low).collect();
    let adx = adx_series(&highs, &lows, &closes, 14);
    sigs.into_iter().filter(|s| {
        let idx = bars.iter().position(|b| b.ts == s.time).unwrap_or(0);
        if idx >= adx.len() { return false; }
        if adx[idx] < 12.0 { return false; }
        vol_filter(bars, idx, 1.2)
    }).collect()
}

// Вспомогательный noEMA ADX8
pub fn xau_noema_adx8(bars: &[Bar], period: usize, tp: f64) -> Vec<SignalHint> {
    let sigs = DonchianGen{ period, tp_mult: tp }.gen(bars);
    let closes: Vec<f64> = bars.iter().map(|b| b.close).collect();
    let highs: Vec<f64> = bars.iter().map(|b| b.high).collect();
    let lows: Vec<f64> = bars.iter().map(|b| b.low).collect();
    let adx = adx_series(&highs, &lows, &closes, 14);
    sigs.into_iter().filter(|s| {
        let idx = bars.iter().position(|b| b.ts == s.time).unwrap_or(0);
        idx < adx.len() && adx[idx] >= 8.0
    }).collect()
}

// H9 — портфель noEMA 15_1.2 (n105 PF1.30) + 25_1.2 (n114 PF1.13) → n114 PF1.30
pub fn h9_portfolio_noema_15_25(bars: &[Bar]) -> Vec<SignalHint> {
    let mut m: Vec<SignalHint> = Vec::new();
    for (p,tp) in [(15,1.2),(25,1.2)] {
        for s in xau_noema_adx8(bars, p, tp) {
            if !m.iter().any(|y| y.time == s.time) { m.push(s); }
        }
    }
    m
}