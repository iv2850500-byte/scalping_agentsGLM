/// ⚠️ Сайзинг БЕЗ учёта комиссий.
/// Остаётся для отключённых стратегий (breakout/bounce/knife/density).
/// Для активных стратегий используй `calc_position_size_with_fees`.
pub fn calc_position_size(
    balance: f64,
    entry_price: f64,
    stop_loss: f64,
    risk_pct: f64,
) -> f64 {
    let risk_amount = balance * risk_pct / 100.0;
    let stop_dist = (entry_price - stop_loss).abs();
    if stop_dist < 1e-9 || risk_amount <= 0.0 {
        return 0.0;
    }
    risk_amount / stop_dist
}

/// ✅ 4г: сайзинг с учётом round-trip комиссии.
///
/// Худший случай (выход по SL) стоит на единицу:
/// расстояние до стопа + комиссии за круг (fee_rt × цена).
/// fee_rt — round-trip ДОЛЕЙ: futures taker ≈ 0.001, maker ≈ 0.0004, spot taker ≈ 0.0015.
pub fn calc_position_size_with_fees(
    balance: f64,
    entry_price: f64,
    stop_loss: f64,
    risk_pct: f64,
    fee_rt: f64,
) -> f64 {
    let risk_amount = balance * risk_pct / 100.0;
    let stop_dist = (entry_price - stop_loss).abs();
    let fee_per_unit = fee_rt * entry_price;
    let denom = stop_dist + fee_per_unit;
    // ✅ P24 (T-01): guard вырожденного стопа. SL==entry -> stop_dist=0 и
    // знаменатель = только комиссия: 1000 U × 0.5% / (0.001×100) = 50 ед.
    // Guard восстановлен из calc_position_size; вызывающие не меняются.
    if stop_dist < 1e-9 || denom < 1e-12 || risk_amount <= 0.0 {
        return 0.0;
    }
    risk_amount / denom
}

/// Округление цены до tick_size — Binance отвергает цену, некратную шагу.
pub fn round_to_tick(price: f64, tick_size: f64) -> f64 {
    if tick_size <= 0.0 {
        return price;
    }
    (price / tick_size).round() * tick_size
}

/// Округление объёма ВНИЗ до step_size.
/// ✅ P41/F11 (audit N7): float-артефакт деления — 0.3/0.1 =
/// 2.9999999999999996 → floor давал 2 → итог 0.2 вместо 0.3
/// (потеря ступени квантования прямо на границе шага).
/// Коррекция: если частное почти целое (в пределах 1e-9 относительной
/// погрешности), считаем его целым. Инвариант «результат ≤ quantity»
/// сохраняется клампом (n*step может дать +1 ulp: 3*0.1 = 0.3000…04).
pub fn floor_to_step(quantity: f64, step_size: f64) -> f64 {
    if step_size <= 0.0 {
        return quantity;
    }
    let ratio = quantity / step_size;
    let floored = ratio.floor();
    let eps = 1e-9 * ratio.abs().max(1.0);
    let n = if (ratio - (floored + 1.0)).abs() <= eps {
        floored + 1.0
    } else {
        floored
    };
    let q = n * step_size;
    if q > quantity { quantity } else { q }
}

/// Округление до N знаков через строку — убирает float-noise.
pub fn round_to_decimals(value: f64, decimals: u32) -> f64 {
    let formatted = format!("{:.*}", decimals as usize, value);
    formatted.parse::<f64>().unwrap_or(value)
}

/// Округление вниз до N знаков
pub fn floor_decimals(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor).floor() / factor
}

// ============================================================
// ✅ P56/S11-1: валидация защитных уровней сигнала (рельсы).
// Стоп, зафиксированный «за wick», обязан быть: конечным (is_finite),
// положительным, на ПРАВИЛЬНОЙ стороне от входа и не совпадать с ним
// (SL == entry = мгновенный ложный StopLoss на первом же тике).
// Проверяется ДО отправки ордера, в движке (execute_real/execute_simulated).
// Выход-сигналы (Exit_*) не проверяются — там рельсы не являются уровнем риска.
// Err(String) — причина отказа, движок превращает её в ExecOutcome::Missed.
// ============================================================
pub fn validate_rails(
    side: &str,
    entry: f64,
    stop_loss: Option<f64>,
    take_profit: Option<f64>,
) -> Result<(), String> {
    if !entry.is_finite() || entry <= 0.0 {
        return Err(format!("S11: входная цена некорректна ({})", entry));
    }
    let is_long = match side {
        "BUY" => true,
        "SELL" => false,
        other => return Err(format!("S11: неизвестная сторона '{}'", other)),
    };
    if let Some(sl) = stop_loss {
        if !sl.is_finite() || sl <= 0.0 {
            return Err(format!("S11: SL некорректен ({})", sl));
        }
        if is_long && sl >= entry {
            return Err(format!(
                "S11: SL {:.8} >= входа {:.8} у лонга — мгновенный стоп",
                sl, entry
            ));
        }
        if !is_long && sl <= entry {
            return Err(format!(
                "S11: SL {:.8} <= входа {:.8} у шорта — мгновенный стоп",
                sl, entry
            ));
        }
    }
    if let Some(tp) = take_profit {
        if !tp.is_finite() || tp <= 0.0 {
            return Err(format!("S11: TP некорректен ({})", tp));
        }
        if is_long && tp <= entry {
            return Err(format!(
                "S11: TP {:.8} <= входа {:.8} у лонга — мгновенный тейк",
                tp, entry
            ));
        }
        if !is_long && tp >= entry {
            return Err(format!(
                "S11: TP {:.8} >= входа {:.8} у шорта — мгновенный тейк",
                tp, entry
            ));
        }
    }
    Ok(())
}

// ============================================================
// ✅ P56/T-K6: дневной бюджет портфеля — чистая функция семантики K6/P15.
// WARN/STOP срабатывают только при СТРОГОМ превышении суммы лимита:
// sum == limit (40.00 из 40.00) — валидно (K6 молчит), sum > limit — стоп.
// Единственная точка правды для check_k6.ps1 и HARD HALT (P54).
// ============================================================
pub fn portfolio_daily_limit_ok(sum_daily_loss: f64, equity_base: f64, stop_pct: f64) -> bool {
    if !sum_daily_loss.is_finite() || !equity_base.is_finite() || !stop_pct.is_finite() {
        return false;
    }
    if equity_base <= 0.0 || stop_pct <= 0.0 {
        return true; // база/порог не заданы — гейт не вооружён (как в P15)
    }
    let limit = equity_base * stop_pct / 100.0;
    sum_daily_loss <= limit
}

#[cfg(test)]
mod p56_tests {
    use super::*;

    // ---------- S11: рельсы ----------

    #[test]
    fn s11_long_valid_rails_pass() {
        assert!(validate_rails("BUY", 100.0, Some(99.0), Some(103.0)).is_ok());
        // рельсы отсутствуют — тоже валидный сигнал (мягкие стратегии)
        assert!(validate_rails("BUY", 100.0, None, None).is_ok());
    }

    #[test]
    fn s11_short_valid_rails_pass() {
        assert!(validate_rails("SELL", 100.0, Some(101.0), Some(97.0)).is_ok());
    }

    #[test]
    fn s11_rejects_wrong_side_stop() {
        // Лонг со стопом выше входа = мгновенный StopLoss
        assert!(validate_rails("BUY", 100.0, Some(100.5), Some(103.0)).is_err());
        // Шорт со стопом ниже входа = мгновенный StopLoss
        assert!(validate_rails("SELL", 100.0, Some(99.5), Some(97.0)).is_err());
        // SL == вход — тоже отказ (равенство — не «за стопом»)
        assert!(validate_rails("BUY", 100.0, Some(100.0), None).is_err());
    }

    #[test]
    fn s11_rejects_wrong_side_tp() {
        assert!(validate_rails("BUY", 100.0, Some(99.0), Some(99.5)).is_err());
        assert!(validate_rails("SELL", 100.0, Some(101.0), Some(100.5)).is_err());
    }

    #[test]
    fn s11_rejects_non_finite_and_nonpositive() {
        assert!(validate_rails("BUY", f64::NAN, Some(99.0), None).is_err());
        assert!(validate_rails("BUY", f64::INFINITY, Some(99.0), None).is_err());
        assert!(validate_rails("BUY", -1.0, Some(99.0), None).is_err());
        assert!(validate_rails("BUY", 100.0, Some(f64::NAN), None).is_err());
        assert!(validate_rails("BUY", 100.0, Some(0.0), None).is_err());
        assert!(validate_rails("BUY", 100.0, None, Some(f64::INFINITY)).is_err());
    }

    #[test]
    fn s11_rejects_unknown_side() {
        assert!(validate_rails("buy", 100.0, Some(99.0), None).is_err());
        assert!(validate_rails("", 100.0, None, None).is_err());
    }

    // ---------- K6/P54: дневной бюджет ----------

    #[test]
    fn k6_budget_40_of_800_is_silent() {
        // Конфиг P50: 4 бота 8+8+8+16 = 40, база 800, 5% = 40 → молчание
        assert!(portfolio_daily_limit_ok(40.0, 800.0, 5.0));
        // 40.01 — уже стоп
        assert!(!portfolio_daily_limit_ok(40.01, 800.0, 5.0));
    }

    #[test]
    fn k6_budget_edge_cases() {
        assert!(portfolio_daily_limit_ok(0.0, 800.0, 5.0));
        // база/порог не заданы — гейт не вооружён (fail-open осознанно:
        // это WARN-семантика K6, HARD HALT базируется на P15-стейте)
        assert!(portfolio_daily_limit_ok(1e9, 0.0, 5.0));
        assert!(portfolio_daily_limit_ok(1e9, 800.0, 0.0));
        // NaN/inf — отказ в сторону безопасности
        assert!(!portfolio_daily_limit_ok(f64::NAN, 800.0, 5.0));
        assert!(!portfolio_daily_limit_ok(40.0, f64::INFINITY, 5.0));
    }

    #[test]
    fn sizing_risk_amount_scales_linearly() {
        // Сайзинг: risk_amount = balance * pct/100, qty = risk/stop_dist
        let q1 = calc_position_size_with_fees(800.0, 100.0, 99.0, 0.5, 0.001);
        let q2 = calc_position_size_with_fees(400.0, 100.0, 99.0, 0.5, 0.001);
        assert!((q1 / q2 - 2.0).abs() < 1e-9, "balance x0.5 => qty x0.5");
        // Множитель выживания 0.5 (ЭТАП C) эквивалентен половине баланса
        let q3 = calc_position_size_with_fees(800.0 * 0.5, 100.0, 99.0, 0.5, 0.001);
        assert!((q3 - q2).abs() < 1e-12);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calc_position_size() {
        let qty = calc_position_size(200.0, 77000.0, 76700.0, 0.5);
        assert!((qty - 0.003333).abs() < 1e-5);
    }

    #[test]
    fn test_calc_position_size_with_fees() {
        // 1000 USDT, риск 0.5% = 5 USDT; вход 100, SL 99 → дистанция 1.0
        // round-trip fee 0.1% → ещё 0.1 на единицу → знаменатель 1.1
        let qty = calc_position_size_with_fees(1000.0, 100.0, 99.0, 0.5, 0.001);
        assert!((qty - 5.0 / 1.1).abs() < 1e-9);
        // с комиссиями объём всегда меньше
        assert!(qty < calc_position_size(1000.0, 100.0, 99.0, 0.5));
    }

    #[test]
    fn test_round_to_tick() {
        assert_eq!(round_to_tick(77207.987, 0.01), 77207.99);
    }

    #[test]
    fn test_floor_to_step() {
        assert!((floor_to_step(0.003339, 0.00001) - 0.00333).abs() < 1e-9);
    }

    // ✅ P24 (T-01): регресс-тест guard вырожденного стопа.
    // До фикса: 1000 U, риск 0.5% -> qty = 5 / (0.001×100) = 50 ед. (!)
    #[test]
    fn test_t01_guard_sl_equals_entry() {
        assert_eq!(calc_position_size_with_fees(1000.0, 100.0, 100.0, 0.5, 0.001), 0.0);
        // обратный случай — почти нулевой, но валидный стоп не блокируется
        let q = calc_position_size_with_fees(1000.0, 100.0, 99.999999999, 0.5, 0.001);
        assert!(q > 0.0);
    }

    #[test]
    fn test_sizing_zero_risk_and_tick() {
        assert_eq!(calc_position_size_with_fees(1000.0, 100.0, 99.0, 0.0, 0.001), 0.0);
        assert_eq!(calc_position_size_with_fees(0.0, 100.0, 99.0, 0.5, 0.001), 0.0);
        // tick/step <= 0 — функции не ломаются
        assert_eq!(round_to_tick(123.456, 0.0), 123.456);
        assert_eq!(floor_to_step(123.456, -1.0), 123.456);
    }

    // ✅ P24: канонические кейсы округлений (референс отчёта, точность 1e-9)
    #[test]
    fn test_round_to_tick_half_away() {
        // 12.375 / 0.25 = 49.5 -> half-away-from-zero -> 50 -> 12.5 (найдка T-02)
        assert!((round_to_tick(12.375, 0.25) - 12.5).abs() < 1e-9);
        // шаг больше цены
        assert_eq!(round_to_tick(3.0, 10.0), 0.0);
    }

    #[test]
    fn test_round_to_decimals_reference() {
        // Ожидания верифицированы Python-референсом (f64 IEEE-754, округление
        // точного бинарного значения): f64(0.12345) хранится ВЫШЕ середины
        // 0.12345000000000000417... -> 0.1235; f64(0.12344) ниже -> 0.1234.
        assert!((round_to_decimals(0.12345, 4) - 0.1235).abs() < 1e-12);
        assert!((round_to_decimals(0.12344, 4) - 0.1234).abs() < 1e-12);
        assert!((round_to_decimals(1.00005, 4) - 1.0001).abs() < 1e-12);
        assert!((floor_decimals(1.23456, 3) - 1.234).abs() < 1e-9);
        assert!((floor_decimals(-1.23456, 3) - (-1.235)).abs() < 1e-9); // floor вниз
    }

    // ✅ P41/F11: регресс-тест float-артефакта (audit N7).
    // До фикса: 0.3/0.1=2.999…96 → floor=2 → 0.2; аналогично 0.7/0.1=6.99…9.
    #[test]
    fn test_floor_to_step_float_artifact() {
        let q = floor_to_step(0.3, 0.1);
        assert!((q - 0.3).abs() < 1e-12, "got {q}");
        assert!(q <= 0.3, "инвариант результат<=quantity нарушен: {q}");
        let q = floor_to_step(0.7, 0.1);
        assert!((q - 0.7).abs() < 1e-12, "got {q}");
        assert!(q <= 0.7);
        // честно нецелые частные не должны подтягиваться вверх
        assert!((floor_to_step(0.29, 0.1) - 0.2).abs() < 1e-12);
        // крупный объём на границе шага
        assert!((floor_to_step(123.0, 1.0) - 123.0).abs() < 1e-9);
    }

    #[test]
    fn test_floor_to_step_reference() {
        assert!((floor_to_step(0.1, 0.03) - 0.09).abs() < 1e-9);
        assert!(floor_to_step(0.001, 0.01) >= 0.0);
    }
}