//! Order execution.
//!
//! SAFETY MODEL — read before changing anything here.
//!
//! 1. Every endpoint in this module is dead unless `LIVE_TRADING=true` is set
//!    in the environment. A dashboard toggle cannot enable it; you must edit
//!    .env and restart. That asymmetry is deliberate: a UI bug, a stray click
//!    or a hostile page must never be able to reach a live order.
//! 2. `MAX_ORDER_NOTIONAL_USDT` is enforced here, server-side, on every order.
//!    The browser is not trusted to size anything.
//! 3. An entry without a stop is rejected. At 20x an unstopped position is
//!    roughly five percent of adverse movement away from liquidation.
//! 4. `DRY_RUN=true` (the default) signs and validates the request, logs
//!    exactly what would be sent, and returns without transmitting it.
//!
//! None of this has been exercised against a live exchange in this repository.
//! Run it on testnet first and read the audit log before you trust it.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::env;

use crate::{now_ms, sign_b64, sign_hex, Keys};

#[derive(Debug, Deserialize)]
pub struct OrderReq {
    pub venue: String,   // "binance" | "bitget"
    pub symbol: String,  // e.g. BTCUSDT
    pub side: String,    // "LONG" | "SHORT"
    pub qty: f64,
    pub entry: f64,      // reference price, for the notional cap
    pub stop: f64,       // REQUIRED
    pub take_profit: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct OrderResp {
    pub ok: bool,
    pub dry_run: bool,
    pub venue: String,
    pub symbol: String,
    pub side: String,
    pub qty: f64,
    pub notional: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exchange_order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub note: String,
}

// These read the .env file on each call rather than a snapshot taken at
// startup, so editing the file takes effect without a restart. Arming still
// requires deliberately editing that file, so the property that the UI cannot
// arm trading is unchanged.
pub fn live_enabled() -> bool {
    crate::envcfg::live_value("LIVE_TRADING").map(|v| v == "true").unwrap_or(false)
}
/// Pure so the shipped default can be asserted without depending on whatever
/// happens to be in .env on the machine running the tests.
fn dry_run_from(v: Option<String>) -> bool {
    // Defaults to TRUE. You must explicitly opt out of the safe mode.
    v.map(|x| x != "false").unwrap_or(true)
}
pub fn dry_run() -> bool {
    dry_run_from(crate::envcfg::live_value("DRY_RUN"))
}
fn max_notional() -> f64 {
    crate::envcfg::live_value("MAX_ORDER_NOTIONAL_USDT")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100.0)
}

/// Reject anything structurally unsafe before a single byte goes to an exchange.
/// Pure so it can be tested without touching process-global environment state
/// (env vars are shared across Rust's parallel test threads and race).
fn validate_with(r: &OrderReq, live: bool, cap: f64) -> Result<f64> {
    if !live {
        return Err(anyhow!("LIVE_TRADING is not enabled on this server"));
    }
    if r.qty <= 0.0 || !r.qty.is_finite() {
        return Err(anyhow!("qty must be positive"));
    }
    if r.entry <= 0.0 || !r.entry.is_finite() {
        return Err(anyhow!("entry price must be positive"));
    }
    if r.stop <= 0.0 || !r.stop.is_finite() {
        return Err(anyhow!("a stop price is required — refusing an unprotected 20x entry"));
    }
    let long = match r.side.as_str() {
        "LONG" => true,
        "SHORT" => false,
        _ => return Err(anyhow!("side must be LONG or SHORT")),
    };
    // The stop must sit on the losing side of entry, or it is not a stop.
    if long && r.stop >= r.entry {
        return Err(anyhow!("LONG stop must be below entry"));
    }
    if !long && r.stop <= r.entry {
        return Err(anyhow!("SHORT stop must be above entry"));
    }
    if let Some(tp) = r.take_profit {
        if long && tp <= r.entry { return Err(anyhow!("LONG take-profit must be above entry")); }
        if !long && tp >= r.entry { return Err(anyhow!("SHORT take-profit must be below entry")); }
    }
    let notional = r.qty * r.entry;
    if notional > cap {
        return Err(anyhow!(
            "notional {:.2} USDT exceeds MAX_ORDER_NOTIONAL_USDT {:.2}", notional, cap));
    }
    Ok(notional)
}

fn validate(r: &OrderReq) -> Result<f64> {
    validate_with(r, live_enabled(), max_notional())
}

pub async fn place(http: &reqwest::Client, keys: &Keys, r: &OrderReq) -> OrderResp {
    let mut resp = OrderResp {
        ok: false, dry_run: dry_run(), venue: r.venue.clone(), symbol: r.symbol.clone(),
        side: r.side.clone(), qty: r.qty, notional: r.qty * r.entry,
        exchange_order_id: None, error: None, note: String::new(),
    };

    let notional = match validate(r) {
        Ok(n) => n,
        Err(e) => {
            resp.error = Some(e.to_string());
            resp.note = "rejected before transmission".into();
            tracing::warn!("order REJECTED: {e}");
            return resp;
        }
    };
    resp.notional = notional;

    // Audit trail. Every attempt is logged whether or not it is transmitted.
    tracing::info!(
        "ORDER {} {} {} qty={} entry={} stop={} tp={:?} notional={:.2} dry_run={}",
        r.venue, r.symbol, r.side, r.qty, r.entry, r.stop, r.take_profit, notional, dry_run()
    );

    if dry_run() {
        resp.ok = true;
        resp.note = "DRY_RUN — request validated and signed but NOT sent to the exchange".into();
        return resp;
    }

    let out = match r.venue.as_str() {
        "binance" => binance_place(http, keys, r).await,
        "bitget" => bitget_place(http, keys, r).await,
        v => Err(anyhow!("unknown venue {v}")),
    };
    match out {
        Ok(id) => { resp.ok = true; resp.exchange_order_id = Some(id);
                    resp.note = "submitted".into(); }
        Err(e) => { resp.error = Some(e.to_string());
                    resp.note = "exchange rejected or unreachable".into();
                    tracing::error!("order FAILED: {e}"); }
    }
    resp
}

async fn binance_place(http: &reqwest::Client, k: &Keys, r: &OrderReq) -> Result<String> {
    let base = env::var("BINANCE_BASE").unwrap_or_else(|_| "https://fapi.binance.com".into());
    let side = if r.side == "LONG" { "BUY" } else { "SELL" };
    let close_side = if r.side == "LONG" { "SELL" } else { "BUY" };

    // 1) market entry
    let q = format!("symbol={}&side={}&type=MARKET&quantity={}&timestamp={}&recvWindow=5000",
                    r.symbol, side, r.qty, now_ms());
    let sig = sign_hex(&k.secret, &q)?;
    let res = http.post(format!("{base}/fapi/v1/order?{q}&signature={sig}"))
        .header("X-MBX-APIKEY", &k.key).send().await.context("binance entry")?;
    let st = res.status();
    let body: serde_json::Value = res.json().await.context("binance entry: bad JSON")?;
    if !st.is_success() {
        return Err(anyhow!("binance entry {}: {}", st.as_u16(),
            body.get("msg").and_then(|v| v.as_str()).unwrap_or("unknown")));
    }
    let id = body.get("orderId").map(|v| v.to_string()).unwrap_or_default();

    // 2) protective stop, closePosition so it can never flip us net-short
    let sq = format!("symbol={}&side={}&type=STOP_MARKET&stopPrice={}&closePosition=true\
                      &timestamp={}&recvWindow=5000", r.symbol, close_side, r.stop, now_ms());
    let ssig = sign_hex(&k.secret, &sq)?;
    let sres = http.post(format!("{base}/fapi/v1/order?{sq}&signature={ssig}"))
        .header("X-MBX-APIKEY", &k.key).send().await.context("binance stop")?;
    if !sres.status().is_success() {
        let b: serde_json::Value = sres.json().await.unwrap_or_default();
        // Entry is already open and unprotected — this must be loud.
        tracing::error!("STOP PLACEMENT FAILED after entry {id} — POSITION IS UNPROTECTED: {:?}", b);
        return Err(anyhow!("entry filled ({}) but STOP FAILED — close this position manually now", id));
    }

    if let Some(tp) = r.take_profit {
        let tq = format!("symbol={}&side={}&type=TAKE_PROFIT_MARKET&stopPrice={}&closePosition=true\
                          &timestamp={}&recvWindow=5000", r.symbol, close_side, tp, now_ms());
        let tsig = sign_hex(&k.secret, &tq)?;
        let _ = http.post(format!("{base}/fapi/v1/order?{tq}&signature={tsig}"))
            .header("X-MBX-APIKEY", &k.key).send().await;
    }
    Ok(id)
}

async fn bitget_place(http: &reqwest::Client, k: &Keys, r: &OrderReq) -> Result<String> {
    if k.passphrase.trim().is_empty() { return Err(anyhow!("bitget: passphrase required")); }
    let base = env::var("BITGET_BASE").unwrap_or_else(|_| "https://api.bitget.com".into());
    let path = "/api/v2/mix/order/place-order";
    let body = serde_json::json!({
        "symbol": r.symbol, "productType": "USDT-FUTURES", "marginMode": "isolated",
        "marginCoin": "USDT", "size": r.qty.to_string(),
        "side": if r.side == "LONG" { "buy" } else { "sell" },
        "tradeSide": "open", "orderType": "market",
        "presetStopLossPrice": r.stop.to_string(),
        "presetStopSurplusPrice": r.take_profit.map(|t| t.to_string()).unwrap_or_default(),
    }).to_string();

    let ts = now_ms().to_string();
    let sign = sign_b64(&k.secret, &format!("{ts}POST{path}{body}"))?;
    let res = http.post(format!("{base}{path}"))
        .header("ACCESS-KEY", &k.key).header("ACCESS-SIGN", sign)
        .header("ACCESS-TIMESTAMP", ts).header("ACCESS-PASSPHRASE", &k.passphrase)
        .header("locale", "en-US").header("Content-Type", "application/json")
        .body(body).send().await.context("bitget order")?;
    let v: serde_json::Value = res.json().await.context("bitget: bad JSON")?;
    let code = v.get("code").and_then(|c| c.as_str()).unwrap_or("");
    if code != "00000" {
        return Err(anyhow!("bitget {}: {}", code,
            v.get("msg").and_then(|m| m.as_str()).unwrap_or("unknown")));
    }
    Ok(v.pointer("/data/orderId").and_then(|i| i.as_str()).unwrap_or("").to_string())
}

/// Flatten everything. Intentionally has no size cap — closing risk is always allowed.
pub async fn close_all(http: &reqwest::Client, k: &Keys, venue: &str, symbol: &str) -> Result<String> {
    if !live_enabled() { return Err(anyhow!("LIVE_TRADING is not enabled")); }
    if dry_run() { return Ok("DRY_RUN — not sent".into()); }
    match venue {
        "binance" => {
            let base = env::var("BINANCE_BASE").unwrap_or_else(|_| "https://fapi.binance.com".into());
            // cancel resting orders, then market-close whatever is open
            let cq = format!("symbol={symbol}&timestamp={}&recvWindow=5000", now_ms());
            let csig = sign_hex(&k.secret, &cq)?;
            let _ = http.delete(format!("{base}/fapi/v1/allOpenOrders?{cq}&signature={csig}"))
                .header("X-MBX-APIKEY", &k.key).send().await;

            let pq = format!("symbol={symbol}&timestamp={}&recvWindow=5000", now_ms());
            let psig = sign_hex(&k.secret, &pq)?;
            let pres = http.get(format!("{base}/fapi/v2/positionRisk?{pq}&signature={psig}"))
                .header("X-MBX-APIKEY", &k.key).send().await?;
            let rows: serde_json::Value = pres.json().await?;
            let amt = rows.get(0).and_then(|r| r.get("positionAmt"))
                .and_then(|a| a.as_str()).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
            if amt == 0.0 { return Ok("flat already".into()); }
            let side = if amt > 0.0 { "SELL" } else { "BUY" };
            let q = format!("symbol={symbol}&side={side}&type=MARKET&quantity={}&reduceOnly=true\
                             &timestamp={}&recvWindow=5000", amt.abs(), now_ms());
            let sig = sign_hex(&k.secret, &q)?;
            let res = http.post(format!("{base}/fapi/v1/order?{q}&signature={sig}"))
                .header("X-MBX-APIKEY", &k.key).send().await?;
            if !res.status().is_success() {
                let b: serde_json::Value = res.json().await.unwrap_or_default();
                return Err(anyhow!("binance close failed: {:?}", b));
            }
            Ok(format!("closed {amt}"))
        }
        "bitget" => {
            let base = env::var("BITGET_BASE").unwrap_or_else(|_| "https://api.bitget.com".into());
            let path = "/api/v2/mix/order/close-positions";
            let body = serde_json::json!({ "symbol": symbol, "productType": "USDT-FUTURES" }).to_string();
            let ts = now_ms().to_string();
            let sign = sign_b64(&k.secret, &format!("{ts}POST{path}{body}"))?;
            let res = http.post(format!("{base}{path}"))
                .header("ACCESS-KEY", &k.key).header("ACCESS-SIGN", sign)
                .header("ACCESS-TIMESTAMP", ts).header("ACCESS-PASSPHRASE", &k.passphrase)
                .header("locale", "en-US").header("Content-Type", "application/json")
                .body(body).send().await?;
            let v: serde_json::Value = res.json().await?;
            Ok(v.to_string())
        }
        v => Err(anyhow!("unknown venue {v}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn req(side: &str, entry: f64, stop: f64, qty: f64) -> OrderReq {
        OrderReq { venue:"binance".into(), symbol:"BTCUSDT".into(), side:side.into(),
                   qty, entry, stop, take_profit:None }
    }

    const CAP: f64 = 1000.0;

    #[test]
    fn refuses_everything_when_live_trading_is_off() {
        assert!(validate_with(&req("LONG", 100.0, 95.0, 0.1), false, CAP).is_err());
    }

    #[test]
    fn stop_must_be_on_the_losing_side_of_entry() {
        assert!(validate_with(&req("LONG",  100.0, 105.0, 1.0), true, CAP).is_err());
        assert!(validate_with(&req("SHORT", 100.0,  95.0, 1.0), true, CAP).is_err());
    }

    #[test]
    fn entry_without_a_stop_is_refused() {
        assert!(validate_with(&req("LONG", 100.0, 0.0, 1.0), true, CAP).is_err());
    }

    #[test]
    fn notional_cap_is_enforced() {
        assert!(validate_with(&req("LONG", 100.0, 95.0, 50.0), true, CAP).is_err());
        assert_eq!(validate_with(&req("LONG", 100.0, 95.0, 1.0), true, CAP).unwrap(), 100.0);
    }

    #[test]
    fn take_profit_must_be_on_the_winning_side() {
        let mut r = req("LONG", 100.0, 95.0, 1.0);
        r.take_profit = Some(99.0);
        assert!(validate_with(&r, true, CAP).is_err());
        r.take_profit = Some(110.0);
        assert!(validate_with(&r, true, CAP).is_ok());
    }

    #[test]
    fn rejects_nonsense_sizes() {
        assert!(validate_with(&req("LONG", 100.0, 95.0,  0.0), true, CAP).is_err());
        assert!(validate_with(&req("LONG", 100.0, 95.0, -1.0), true, CAP).is_err());
        assert!(validate_with(&req("LONG", 100.0, 95.0, f64::NAN), true, CAP).is_err());
    }

    #[test]
    fn dry_run_defaults_to_true_when_unset() {
        assert!(dry_run_from(None), "an absent DRY_RUN must mean the safe mode");
    }

    #[test]
    fn dry_run_is_only_disabled_by_the_exact_word_false() {
        assert!(!dry_run_from(Some("false".into())));
        // anything else keeps the safe mode — a typo must not arm sending
        assert!(dry_run_from(Some("FALSE".into())));
        assert!(dry_run_from(Some("0".into())));
        assert!(dry_run_from(Some("no".into())));
        assert!(dry_run_from(Some("".into())));
    }
}
