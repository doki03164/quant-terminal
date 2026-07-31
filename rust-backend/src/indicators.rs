//! Indicators, ported from the dashboard's JavaScript.
//!
//! These MUST stay numerically identical to the JS implementations in
//! crypto-bot-dashboard.html. If they drift, the strategy you backtested is
//! not the strategy the daemon trades — which is the most dangerous kind of
//! bug in this project because nothing visibly breaks.
//!
//! `cargo run -- parity` prints these values for live Binance data so they can
//! be diffed against the browser's `computeIndicators()` output.

#[derive(Debug, Clone, Copy)]
pub struct Bar {
    pub t: i64,
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
    pub v: f64,
    pub ct: i64,
}

/// Exponential moving average, seeded with the first value (matches the JS).
pub fn ema(vals: &[f64], p: usize) -> Vec<f64> {
    let mut out = Vec::with_capacity(vals.len());
    if vals.is_empty() {
        return out;
    }
    let k = 2.0 / (p as f64 + 1.0);
    let mut e = vals[0];
    for (i, v) in vals.iter().enumerate() {
        e = if i == 0 { *v } else { v * k + e * (1.0 - k) };
        out.push(e);
    }
    out
}

/// Wilder ATR.
pub fn atr(bars: &[Bar], p: usize) -> f64 {
    if bars.len() < p + 1 {
        return 0.0;
    }
    let mut tr = Vec::with_capacity(bars.len());
    for (i, b) in bars.iter().enumerate() {
        let pv = if i > 0 { bars[i - 1].c } else { b.o };
        tr.push((b.h - b.l).max((b.h - pv).abs()).max((b.l - pv).abs()));
    }
    let mut a: f64 = tr[..p].iter().sum::<f64>() / p as f64;
    for v in &tr[p..] {
        a = (a * (p as f64 - 1.0) + v) / p as f64;
    }
    a
}

/// Wilder ADX. Seeds the smoothers with a p-bar sum, as the JS does.
pub fn adx(bars: &[Bar], p: usize) -> f64 {
    if bars.len() < 18 {
        return 0.0;
    }
    let p = p.min(bars.len() / 3).max(2);
    let take = (p * 10).min(bars.len());
    let src = &bars[bars.len() - take..];

    let (mut tra, mut pdma, mut ndma) = (Vec::new(), Vec::new(), Vec::new());
    for i in 1..src.len() {
        let (h, l) = (src[i].h, src[i].l);
        let (ph, pl, pc) = (src[i - 1].h, src[i - 1].l, src[i - 1].c);
        let (up, dn) = (h - ph, pl - l);
        pdma.push(if up > dn && up > 0.0 { up } else { 0.0 });
        ndma.push(if dn > up && dn > 0.0 { dn } else { 0.0 });
        tra.push((h - l).max((h - pc).abs()).max((l - pc).abs()));
    }
    if tra.len() < p * 2 {
        return 0.0;
    }
    let sum = |v: &[f64], n: usize| -> f64 { v[..n].iter().sum() };
    let (mut tr, mut pdm, mut ndm) = (sum(&tra, p), sum(&pdma, p), sum(&ndma, p));

    let dx_of = |tr: f64, pdm: f64, ndm: f64| -> f64 {
        if tr <= 0.0 {
            return 0.0;
        }
        let (pdi, ndi) = (100.0 * pdm / tr, 100.0 * ndm / tr);
        if pdi + ndi > 0.0 {
            100.0 * (pdi - ndi).abs() / (pdi + ndi)
        } else {
            0.0
        }
    };

    let mut dxs = vec![dx_of(tr, pdm, ndm)];
    for i in p..tra.len() {
        tr = tr - tr / p as f64 + tra[i];
        pdm = pdm - pdm / p as f64 + pdma[i];
        ndm = ndm - ndm / p as f64 + ndma[i];
        dxs.push(dx_of(tr, pdm, ndm));
    }
    if dxs.len() < p {
        return *dxs.last().unwrap_or(&0.0);
    }
    let mut a = sum(&dxs, p) / p as f64;
    for v in &dxs[p..] {
        a = (a * (p as f64 - 1.0) + v) / p as f64;
    }
    a
}

pub struct Macd {
    pub line: Vec<f64>,
    pub signal: Vec<f64>,
    pub hist: Vec<f64>,
}

pub fn macd(closes: &[f64], fast: usize, slow: usize, sig: usize) -> Macd {
    let ef = ema(closes, fast);
    let es = ema(closes, slow);
    let line: Vec<f64> = ef.iter().zip(&es).map(|(a, b)| a - b).collect();
    let signal = ema(&line, sig);
    let hist = line.iter().zip(&signal).map(|(a, b)| a - b).collect();
    Macd { line, signal, hist }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(o: f64, h: f64, l: f64, c: f64) -> Bar {
        Bar { t: 0, o, h, l, c, v: 1.0, ct: 0 }
    }

    #[test]
    fn ema_seeds_on_first_value_and_smooths() {
        let v = vec![10.0, 11.0, 12.0];
        let e = ema(&v, 2);
        // k = 2/3. e0 = 10. e1 = 11*2/3 + 10*1/3 = 10.6667. e2 = 12*2/3 + e1/3
        assert!((e[0] - 10.0).abs() < 1e-12);
        assert!((e[1] - 10.666666666666666).abs() < 1e-9);
        assert!((e[2] - 11.555555555555555).abs() < 1e-9);
    }

    #[test]
    fn ema_of_a_constant_series_is_that_constant() {
        let v = vec![7.0; 50];
        for x in ema(&v, 12) {
            assert!((x - 7.0).abs() < 1e-12);
        }
    }

    #[test]
    fn atr_of_constant_range_bars_equals_that_range() {
        // every bar spans exactly 2.0 and closes where it opened
        let bars: Vec<Bar> = (0..40).map(|_| bar(100.0, 101.0, 99.0, 100.0)).collect();
        assert!((atr(&bars, 14) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn atr_returns_zero_without_enough_bars() {
        let bars: Vec<Bar> = (0..5).map(|_| bar(1.0, 2.0, 0.5, 1.5)).collect();
        assert_eq!(atr(&bars, 14), 0.0);
    }

    #[test]
    fn adx_is_high_for_a_clean_one_way_trend() {
        // strictly rising bars: +DI dominates, DX saturates near 100
        let bars: Vec<Bar> = (0..60)
            .map(|i| {
                let base = 100.0 + i as f64;
                bar(base, base + 1.0, base - 0.2, base + 0.8)
            })
            .collect();
        let a = adx(&bars, 14);
        assert!(a > 60.0, "expected a strong trend reading, got {a}");
        assert!(a <= 100.0);
    }

    #[test]
    fn adx_is_low_for_a_flat_market() {
        let bars: Vec<Bar> = (0..60).map(|_| bar(100.0, 100.5, 99.5, 100.0)).collect();
        let a = adx(&bars, 14);
        assert!(a < 30.0, "expected a weak reading on flat bars, got {a}");
    }

    #[test]
    fn adx_returns_zero_on_short_input() {
        let bars: Vec<Bar> = (0..10).map(|_| bar(1.0, 2.0, 0.5, 1.5)).collect();
        assert_eq!(adx(&bars, 14), 0.0);
    }

    #[test]
    fn macd_line_is_fast_minus_slow_and_hist_is_line_minus_signal() {
        let closes: Vec<f64> = (0..80).map(|i| 100.0 + (i as f64) * 0.5).collect();
        let m = macd(&closes, 12, 26, 9);
        let ef = ema(&closes, 12);
        let es = ema(&closes, 26);
        let n = closes.len() - 1;
        assert!((m.line[n] - (ef[n] - es[n])).abs() < 1e-12);
        assert!((m.hist[n] - (m.line[n] - m.signal[n])).abs() < 1e-12);
        // a steadily rising series must give a positive MACD line
        assert!(m.line[n] > 0.0);
    }
}
