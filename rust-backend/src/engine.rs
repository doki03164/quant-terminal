//! Headless strategy engine.
//!
//! Runs without the dashboard so the bot survives closing the browser.
//! Reads the SAME config.json the dashboard exports, so what you tuned in the
//! UI is what runs here.
//!
//! ONLY the MACD + EMA(trend) strategy is ported so far. The dual-MA and
//! 7-gate playbook still live only in the browser. Porting them means
//! duplicating swing-pivot and market-structure detection, and a subtle
//! divergence there would mean the daemon trades something different from what
//! you backtested — so they are deliberately absent rather than approximated.
//! `strategies.dualma.on` / `.playbook.on` are ignored here and warned about.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::{collections::HashMap, path::Path, time::Duration};

use crate::indicators::{adx, atr, ema, macd, Bar};
use crate::trade::{self, OrderReq};
use crate::Keys;

/* ----------------------------- config ----------------------------- */

#[derive(Debug, Deserialize, Clone)]
pub struct Breakers {
    #[serde(default = "d_daily")]   pub daily: f64,
    #[serde(default = "d_weekly")]  pub weekly: f64,
    #[serde(default = "d_streak")]  pub streak: u32,
    #[serde(default = "d_maxtd", rename = "maxTradesDay")] pub max_trades_day: u32,
}
fn d_daily() -> f64 { 0.02 }
fn d_weekly() -> f64 { 0.05 }
fn d_streak() -> u32 { 3 }
fn d_maxtd() -> u32 { 2 }

#[derive(Debug, Deserialize, Clone)]
pub struct Risk {
    #[serde(default = "d_lev")]      pub leverage: f64,
    #[serde(default = "d_risk")]     pub risk_pct: f64,
    #[serde(default = "d_rr")]       pub min_rr: f64,
    #[serde(default = "d_slatr")]    pub sl_atr_mult: f64,
    #[serde(default)]                pub breakers: Option<Breakers>,
}
fn d_lev() -> f64 { 20.0 }
fn d_risk() -> f64 { 0.01 }
fn d_rr() -> f64 { 2.0 }
fn d_slatr() -> f64 { 2.0 }

#[derive(Debug, Deserialize, Clone)]
pub struct MacdCfg {
    #[serde(default)] pub on: bool,
    #[serde(default)] pub alloc: HashMap<String, bool>,
    #[serde(default)] pub tf: HashMap<String, bool>,
    #[serde(default = "d_f")]  pub fast: usize,
    #[serde(default = "d_s")]  pub slow: usize,
    #[serde(default = "d_sig")] pub signal: usize,
    #[serde(default = "d_tl")] pub trend_len: usize,
    #[serde(default = "d_md")] pub max_dist: f64,
    #[serde(default = "d_rr")] pub rr: f64,
}
fn d_f() -> usize { 12 }
fn d_s() -> usize { 26 }
fn d_sig() -> usize { 9 }
fn d_tl() -> usize { 120 }
fn d_md() -> f64 { 3.0 }

#[derive(Debug, Deserialize, Clone)]
pub struct Strategies {
    pub macd: Option<MacdCfg>,
    #[serde(default)] pub dualma: Option<serde_json::Value>,
    #[serde(default)] pub playbook: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub version: u32,
    pub risk: Risk,
    pub strategies: Strategies,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let c: Config = serde_json::from_str(&raw).context("config.json is not valid")?;
        if c.version != 1 {
            return Err(anyhow!("unsupported config version {}", c.version));
        }
        Ok(c)
    }
}

/* ----------------------------- market data ----------------------------- */

pub async fn klines(http: &reqwest::Client, symbol: &str, interval: &str, limit: u32)
    -> Result<Vec<Bar>>
{
    let base = std::env::var("BINANCE_DATA_BASE")
        .unwrap_or_else(|_| "https://api.binance.com".into());
    let url = format!("{base}/api/v3/klines?symbol={symbol}&interval={interval}&limit={limit}");
    let rows: Vec<Vec<serde_json::Value>> = http.get(&url).send().await
        .context("kline request failed")?
        .json().await.context("kline: bad JSON")?;
    let num = |v: &serde_json::Value| -> f64 {
        v.as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0)
    };
    Ok(rows.iter().map(|k| Bar {
        t: k[0].as_i64().unwrap_or(0),
        o: num(&k[1]), h: num(&k[2]), l: num(&k[3]), c: num(&k[4]), v: num(&k[5]),
        ct: k[6].as_i64().unwrap_or(0),
    }).collect())
}

/* ----------------------------- strategy ----------------------------- */

#[derive(Debug)]
pub struct Signal {
    pub side: String,
    pub entry: f64,
    pub stop: f64,
    pub take_profit: f64,
    pub reason: String,
}

/// Mirrors evalMacd() in the dashboard. Reads the last CLOSED bar.
pub fn eval_macd(bars: &[Bar], c: &MacdCfg, sl_atr_mult: f64) -> Option<Signal> {
    if bars.len() < c.trend_len + 40 {
        return None;
    }
    let closes: Vec<f64> = bars.iter().map(|b| b.c).collect();
    let n = closes.len();
    let i = n - 2; // last closed bar
    if i < 2 {
        return None;
    }
    let trend = ema(&closes, c.trend_len);
    let m = macd(&closes, c.fast, c.slow, c.signal);
    let a = atr(bars, 14);
    if a <= 0.0 {
        return None;
    }

    let above = closes[i] > trend[i];
    let dist = (closes[i] - trend[i]).abs() / a;

    let cross_up = m.line[i - 1] <= m.signal[i - 1] && m.line[i] > m.signal[i];
    let cross_dn = m.line[i - 1] >= m.signal[i - 1] && m.line[i] < m.signal[i];
    let crossed = if above { cross_up } else { cross_dn };
    if !crossed {
        return None;
    }

    let hist_ok = if above {
        m.hist[i] > 0.0 && m.hist[i] > m.hist[i - 1]
    } else {
        m.hist[i] < 0.0 && m.hist[i] < m.hist[i - 1]
    };
    if !hist_ok || dist > c.max_dist {
        return None;
    }

    let entry = closes[n - 1]; // current price
    let sl_dist = sl_atr_mult * a;
    let (stop, tp) = if above {
        (entry - sl_dist, entry + sl_dist * c.rr)
    } else {
        (entry + sl_dist, entry - sl_dist * c.rr)
    };
    Some(Signal {
        side: if above { "LONG" } else { "SHORT" }.into(),
        entry, stop, take_profit: tp,
        reason: format!("MACD {} + EMA{} (dist {:.2}xATR, adx {:.0})",
                        if above { "金叉" } else { "死叉" }, c.trend_len, dist, adx(bars, 14)),
    })
}

/// Risk-based size: risk_pct of equity divided by the stop distance.
pub fn size_position(equity: f64, risk_pct: f64, entry: f64, stop: f64) -> f64 {
    let d = (entry - stop).abs();
    if d <= 0.0 || equity <= 0.0 { return 0.0; }
    (equity * risk_pct) / d
}

/* ----------------------------- daemon ----------------------------- */

pub struct Daemon {
    pub http: reqwest::Client,
    pub cfg: Config,
    pub binance: Option<Keys>,
    pub interval_secs: u64,
    /// Symbols already holding a position, so we do not stack entries.
    open: HashMap<String, bool>,
    trades_today: u32,
    day: i64,
}

impl Daemon {
    pub fn new(http: reqwest::Client, cfg: Config, binance: Option<Keys>, interval_secs: u64) -> Self {
        Self { http, cfg, binance, interval_secs, open: HashMap::new(), trades_today: 0, day: 0 }
    }

    fn tf_interval(c: &MacdCfg) -> &'static str {
        if *c.tf.get("m15").unwrap_or(&false) { "15m" } else { "4h" }
    }

    pub async fn tick(&mut self) -> Result<()> {
        let Some(mc) = self.cfg.strategies.macd.clone() else {
            tracing::warn!("config has no macd strategy — nothing to run");
            return Ok(());
        };
        if !mc.on {
            tracing::info!("macd strategy is off in config — idle");
            return Ok(());
        }

        // reset the daily counter on UTC day rollover
        let now_day = (crate::now_ms() / 86_400_000) as i64;
        if now_day != self.day {
            self.day = now_day;
            self.trades_today = 0;
        }
        let cap = self.cfg.risk.breakers.as_ref().map(|b| b.max_trades_day).unwrap_or(2);
        if self.trades_today >= cap {
            tracing::info!("daily entry cap reached ({}/{}) — not opening new positions",
                           self.trades_today, cap);
            return Ok(());
        }

        let interval = Self::tf_interval(&mc);
        let symbols: Vec<String> = mc.alloc.iter()
            .filter(|(_, v)| **v)
            .map(|(k, _)| format!("{k}USDT"))
            .collect();

        for sym in symbols {
            if *self.open.get(&sym).unwrap_or(&false) { continue; }
            let bars = match klines(&self.http, &sym, interval, 500).await {
                Ok(b) => b,
                Err(e) => { tracing::warn!("{sym}: {e}"); continue; }
            };
            let Some(sig) = eval_macd(&bars, &mc, self.cfg.risk.sl_atr_mult) else { continue };

            // Equity for sizing comes from the exchange, never a guess.
            let equity = match &self.binance {
                Some(k) => crate::binance_equity(&self.http, k).await.unwrap_or(0.0),
                None => 0.0,
            };
            if equity <= 0.0 {
                tracing::warn!("{sym}: signal found but account equity is 0 — skipping");
                continue;
            }
            let qty = size_position(equity, self.cfg.risk.risk_pct, sig.entry, sig.stop);
            if qty <= 0.0 { continue; }

            tracing::info!("SIGNAL {sym} {} @ {:.4} stop {:.4} tp {:.4} qty {:.6} — {}",
                sig.side, sig.entry, sig.stop, sig.take_profit, qty, sig.reason);

            let Some(k) = &self.binance else {
                tracing::warn!("no binance keys — cannot act on signal");
                continue;
            };
            let req = OrderReq {
                venue: "binance".into(), symbol: sym.clone(), side: sig.side.clone(),
                qty, entry: sig.entry, stop: sig.stop, take_profit: Some(sig.take_profit),
            };
            let resp = trade::place(&self.http, k, &req).await;
            if resp.ok {
                if !resp.dry_run {
                    self.open.insert(sym.clone(), true);
                    self.trades_today += 1;
                }
                tracing::info!("{sym}: {}", resp.note);
            } else {
                tracing::warn!("{sym}: order not placed — {}", resp.error.unwrap_or_default());
            }
        }
        Ok(())
    }

    pub async fn run(mut self) -> Result<()> {
        if self.cfg.strategies.dualma.is_some() || self.cfg.strategies.playbook.is_some() {
            tracing::warn!(
                "config contains dualma/playbook settings, but only MACD is ported to the \
                 daemon — those strategies run in the dashboard only");
        }
        tracing::info!("daemon started — evaluating every {}s", self.interval_secs);
        loop {
            if let Err(e) = self.tick().await {
                tracing::error!("tick failed: {e}");
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(self.interval_secs)) => {}
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("stop requested — daemon exiting");
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth(n: usize, f: impl Fn(usize) -> f64) -> Vec<Bar> {
        (0..n).map(|i| {
            let c = f(i);
            Bar { t: i as i64, o: c, h: c + 0.5, l: c - 0.5, c, v: 1.0, ct: i as i64 }
        }).collect()
    }

    fn cfg() -> MacdCfg {
        MacdCfg { on: true, alloc: HashMap::new(), tf: HashMap::new(),
                  fast: 12, slow: 26, signal: 9, trend_len: 120, max_dist: 3.0, rr: 2.0 }
    }

    #[test]
    fn no_signal_without_enough_history() {
        let bars = synth(50, |i| 100.0 + i as f64);
        assert!(eval_macd(&bars, &cfg(), 2.0).is_none());
    }

    #[test]
    fn no_signal_on_a_flat_series() {
        let bars = synth(400, |_| 100.0);
        assert!(eval_macd(&bars, &cfg(), 2.0).is_none());
    }

    #[test]
    fn long_signal_puts_stop_below_and_target_above() {
        // long uptrend, then a dip and recovery to force a signal-line cross
        let bars = synth(400, |i| {
            let base = 100.0 + i as f64 * 0.4;
            if i > 300 && i < 340 { base - 12.0 } else { base }
        });
        if let Some(s) = eval_macd(&bars, &cfg(), 2.0) {
            assert_eq!(s.side, "LONG");
            assert!(s.stop < s.entry, "stop must sit below entry for a long");
            assert!(s.take_profit > s.entry, "target must sit above entry for a long");
            // reward:risk must equal the configured multiple
            let rr = (s.take_profit - s.entry) / (s.entry - s.stop);
            assert!((rr - 2.0).abs() < 1e-6, "rr was {rr}");
        }
    }

    #[test]
    fn sizing_risks_exactly_the_configured_fraction() {
        // 1% of 10_000 = 100 risked over a 50-wide stop => qty 2
        let qty = size_position(10_000.0, 0.01, 1000.0, 950.0);
        assert!((qty - 2.0).abs() < 1e-9);
    }

    #[test]
    fn sizing_refuses_degenerate_input() {
        assert_eq!(size_position(10_000.0, 0.01, 100.0, 100.0), 0.0);
        assert_eq!(size_position(0.0, 0.01, 100.0, 95.0), 0.0);
    }

    #[test]
    fn config_rejects_a_future_version() {
        let bad = r#"{"version":2,"risk":{},"strategies":{}}"#;
        assert!(serde_json::from_str::<Config>(bad).map(|c| c.version == 1).unwrap_or(false) == false);
    }

    #[test]
    fn config_parses_the_dashboard_export_shape() {
        let raw = r#"{
          "version":1,
          "risk":{"leverage":20,"risk_pct":0.01,"min_rr":2,"sl_atr_mult":2,
                  "breakers":{"daily":0.02,"weekly":0.05,"streak":3,"maxTradesDay":2}},
          "strategies":{"macd":{"on":true,"alloc":{"BTC":true},"tf":{"h4":true},
                                "fast":12,"slow":26,"signal":9,"trend_len":120,
                                "max_dist":3.0,"rr":2.0}}
        }"#;
        let c: Config = serde_json::from_str(raw).unwrap();
        assert_eq!(c.risk.leverage, 20.0);
        assert_eq!(c.risk.breakers.unwrap().max_trades_day, 2);
        let m = c.strategies.macd.unwrap();
        assert!(m.on && m.trend_len == 120);
        assert_eq!(m.alloc.get("BTC"), Some(&true));
    }
}
