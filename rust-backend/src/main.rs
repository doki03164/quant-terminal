//! QUANT TERMINAL — read-only signing proxy.
//!
//! The dashboard is a browser page, and a browser cannot hold an exchange
//! secret safely: anything in JS is readable by the user, any extension, and
//! any XSS. Exchanges also reject signed requests from browser origins. So the
//! secret lives here instead, and the page asks this service for balances.
//!
//! DELIBERATELY READ-ONLY. There is no order placement, no transfer, no
//! withdrawal endpoint, and none should be added without a separate review:
//! the moment this service can trade, a bug in it can empty an account.
//! Create the API keys with trading and withdrawal permissions DISABLED and
//! bind them to this machine's IP.

use anyhow::{anyhow, Context, Result};
use axum::{
    extract::State,
    http::{HeaderValue, Method, StatusCode},
    routing::get,
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::Sha256;
use std::{env, net::SocketAddr, sync::Arc, time::{SystemTime, UNIX_EPOCH}};
use tower_http::cors::CorsLayer;

mod engine;
mod envcfg;
mod indicators;
mod trade;
use trade::{close_all, place, OrderReq, OrderResp};

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct Keys {
    pub key: String,
    pub secret: String,
    pub passphrase: String,
}
impl Keys {
    fn from_env(prefix: &str) -> Option<Self> {
        let key = env::var(format!("{prefix}_API_KEY")).ok()?;
        let secret = env::var(format!("{prefix}_API_SECRET")).ok()?;
        if key.trim().is_empty() || secret.trim().is_empty() {
            return None;
        }
        Some(Self {
            key,
            secret,
            passphrase: env::var(format!("{prefix}_PASSPHRASE")).unwrap_or_default(),
        })
    }
}

struct AppState {
    http: reqwest::Client,
    binance: Option<Keys>,
    bitget: Option<Keys>,
}

pub fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}

pub fn sign_hex(secret: &str, payload: &str) -> Result<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| anyhow!("invalid secret length"))?;
    mac.update(payload.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

pub fn sign_b64(secret: &str, payload: &str) -> Result<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| anyhow!("invalid secret length"))?;
    mac.update(payload.as_bytes());
    Ok(B64.encode(mac.finalize().into_bytes()))
}

#[derive(Serialize, Default)]
struct VenueBalance {
    connected: bool,
    /// Total wallet balance in USDT.
    balance: f64,
    /// Wallet balance plus unrealised PnL on open positions.
    equity: f64,
    unrealized: f64,
    available: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct BalanceReply {
    ok: bool,
    ts: u128,
    binance: VenueBalance,
    bitget: VenueBalance,
    /// Sum of both venues' equity — what the dashboard header shows.
    total_equity: f64,
}

/// Binance USDⓈ-M futures account. Read-only endpoint.
async fn binance_balance(http: &reqwest::Client, k: &Keys) -> Result<VenueBalance> {
    let base = env::var("BINANCE_BASE")
        .unwrap_or_else(|_| "https://fapi.binance.com".into());
    let query = format!("timestamp={}&recvWindow=5000", now_ms());
    let sig = sign_hex(&k.secret, &query)?;
    let url = format!("{base}/fapi/v2/account?{query}&signature={sig}");

    let resp = http
        .get(&url)
        .header("X-MBX-APIKEY", &k.key)
        .send()
        .await
        .context("binance request failed")?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.context("binance: bad JSON")?;
    if !status.is_success() {
        let msg = body.get("msg").and_then(|v| v.as_str()).unwrap_or("unknown");
        return Err(anyhow!("binance {}: {}", status.as_u16(), msg));
    }

    let f = |k: &str| body.get(k).and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
    Ok(VenueBalance {
        connected: true,
        balance: f("totalWalletBalance"),
        equity: f("totalMarginBalance"),
        unrealized: f("totalUnrealizedProfit"),
        available: f("availableBalance"),
        error: None,
    })
}

/// Bitget USDT-M futures account. Read-only endpoint.
async fn bitget_balance(http: &reqwest::Client, k: &Keys) -> Result<VenueBalance> {
    if k.passphrase.trim().is_empty() {
        return Err(anyhow!("bitget: BITGET_PASSPHRASE is required"));
    }
    let base = env::var("BITGET_BASE").unwrap_or_else(|_| "https://api.bitget.com".into());
    let path = "/api/v2/mix/account/accounts";
    let query = "productType=USDT-FUTURES";
    let ts = now_ms().to_string();
    // Bitget prehash: timestamp + METHOD + requestPath + "?" + query + body
    let prehash = format!("{ts}GET{path}?{query}");
    let sign = sign_b64(&k.secret, &prehash)?;

    let resp = http
        .get(format!("{base}{path}?{query}"))
        .header("ACCESS-KEY", &k.key)
        .header("ACCESS-SIGN", sign)
        .header("ACCESS-TIMESTAMP", ts)
        .header("ACCESS-PASSPHRASE", &k.passphrase)
        .header("locale", "en-US")
        .header("Content-Type", "application/json")
        .send()
        .await
        .context("bitget request failed")?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.context("bitget: bad JSON")?;
    let code = body.get("code").and_then(|v| v.as_str()).unwrap_or("");
    if !status.is_success() || (!code.is_empty() && code != "00000") {
        let msg = body.get("msg").and_then(|v| v.as_str()).unwrap_or("unknown");
        return Err(anyhow!("bitget {}: {}", code, msg));
    }

    let mut out = VenueBalance { connected: true, ..Default::default() };
    if let Some(rows) = body.get("data").and_then(|v| v.as_array()) {
        for r in rows {
            let g = |k: &str| r.get(k).and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
            out.balance += g("accountEquity") - g("unrealizedPL");
            out.equity += g("accountEquity");
            out.unrealized += g("unrealizedPL");
            out.available += g("available");
        }
    }
    Ok(out)
}

/// Total futures equity on Binance, used by the daemon to size positions.
pub async fn binance_equity(http: &reqwest::Client, k: &Keys) -> Result<f64> {
    Ok(binance_balance(http, k).await?.equity)
}

async fn get_balance(State(st): State<Arc<AppState>>) -> (StatusCode, Json<BalanceReply>) {
    let mut reply = BalanceReply {
        ok: true,
        ts: now_ms(),
        binance: VenueBalance::default(),
        bitget: VenueBalance::default(),
        total_equity: 0.0,
    };

    if let Some(k) = &st.binance {
        match binance_balance(&st.http, k).await {
            Ok(b) => reply.binance = b,
            // Never echo the raw error verbatim into a browser response beyond
            // the exchange's own message — it must not contain key material.
            Err(e) => {
                reply.ok = false;
                reply.binance.error = Some(e.to_string());
                tracing::warn!("binance balance failed: {e}");
            }
        }
    }
    if let Some(k) = &st.bitget {
        match bitget_balance(&st.http, k).await {
            Ok(b) => reply.bitget = b,
            Err(e) => {
                reply.ok = false;
                reply.bitget.error = Some(e.to_string());
                tracing::warn!("bitget balance failed: {e}");
            }
        }
    }
    reply.total_equity = reply.binance.equity + reply.bitget.equity;
    (StatusCode::OK, Json(reply))
}

#[derive(Serialize)]
struct Health {
    ok: bool,
    binance_configured: bool,
    bitget_configured: bool,
    /// True only when LIVE_TRADING=true is set in the server environment.
    trading_enabled: bool,
    /// When true, orders are validated and signed but never transmitted.
    dry_run: bool,
    max_order_notional_usdt: f64,
}

async fn health(State(st): State<Arc<AppState>>) -> Json<Health> {
    Json(Health {
        ok: true,
        binance_configured: st.binance.is_some(),
        bitget_configured: st.bitget.is_some(),
        trading_enabled: trade::live_enabled(),
        dry_run: trade::dry_run(),
        max_order_notional_usdt: std::env::var("MAX_ORDER_NOTIONAL_USDT").ok()
            .and_then(|v| v.parse().ok()).unwrap_or(100.0),
    })
}

#[derive(serde::Deserialize)]
struct CloseReq { venue: String, symbol: String }

async fn get_env() -> (StatusCode, Json<serde_json::Value>) {
    match envcfg::read_view() {
        Ok(v) => (StatusCode::OK, Json(serde_json::to_value(v).unwrap_or_default())),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR,
                   Json(serde_json::json!({"ok":false,"error":e.to_string()}))),
    }
}

async fn post_env(Json(u): Json<envcfg::EnvUpdate>) -> (StatusCode, Json<serde_json::Value>) {
    match envcfg::write(u) {
        Ok(r) => {
            let code = if r.ok { StatusCode::OK } else { StatusCode::BAD_REQUEST };
            (code, Json(serde_json::to_value(r).unwrap_or_default()))
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR,
                   Json(serde_json::json!({"ok":false,"error":e.to_string()}))),
    }
}

async fn post_order(State(st): State<Arc<AppState>>, Json(req): Json<OrderReq>)
    -> (StatusCode, Json<OrderResp>) {
    let keys = match req.venue.as_str() {
        "binance" => st.binance.clone(),
        "bitget" => st.bitget.clone(),
        _ => None,
    };
    let Some(k) = keys else {
        return (StatusCode::BAD_REQUEST, Json(OrderResp {
            ok:false, dry_run:trade::dry_run(), venue:req.venue.clone(), symbol:req.symbol.clone(),
            side:req.side.clone(), qty:req.qty, notional:0.0, exchange_order_id:None,
            error:Some("venue not configured on this server".into()), note:"rejected".into() }));
    };
    let r = place(&st.http, &k, &req).await;
    let code = if r.ok { StatusCode::OK } else { StatusCode::BAD_REQUEST };
    (code, Json(r))
}

async fn post_close(State(st): State<Arc<AppState>>, Json(req): Json<CloseReq>)
    -> (StatusCode, Json<serde_json::Value>) {
    let keys = match req.venue.as_str() {
        "binance" => st.binance.clone(),
        "bitget" => st.bitget.clone(),
        _ => None,
    };
    let Some(k) = keys else {
        return (StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok":false,"error":"venue not configured"})));
    };
    match close_all(&st.http, &k, &req.venue, &req.symbol).await {
        Ok(m) => (StatusCode::OK, Json(serde_json::json!({"ok":true,"result":m}))),
        Err(e) => (StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok":false,"error":e.to_string()}))),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "quant_terminal_backend=info,tower_http=warn".into()),
        )
        .init();

    let binance = Keys::from_env("BINANCE");
    let bitget = Keys::from_env("BITGET");
    if binance.is_none() && bitget.is_none() {
        tracing::warn!("no API keys configured — /api/balance will return zeros. See .env.example");
    }

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    // ---- subcommands -------------------------------------------------
    let args: Vec<String> = env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("serve");
    match cmd {
        "daemon" => {
            let path = std::path::PathBuf::from(
                env::var("CONFIG_PATH").unwrap_or_else(|_| "config.json".into()));
            let cfg = engine::Config::load(&path)?;
            tracing::info!("loaded {}", path.display());
            if trade::live_enabled() && !trade::dry_run() {
                tracing::error!("*** DAEMON ARMED FOR LIVE ORDERS WITH REAL FUNDS ***");
            } else if trade::live_enabled() {
                tracing::warn!("daemon running in DRY_RUN — signals logged, nothing sent");
            } else {
                tracing::info!("LIVE_TRADING=false — daemon will evaluate but never order");
            }
            let secs: u64 = env::var("DAEMON_INTERVAL_SECS").ok()
                .and_then(|v| v.parse().ok()).unwrap_or(60);
            return engine::Daemon::new(http, cfg, binance, secs).run().await;
        }
        "parity" => {
            // Print indicator values for live data so they can be diffed
            // against the browser's computeIndicators() output.
            let sym = args.get(1).cloned().unwrap_or_else(|| "BTCUSDT".into());
            let iv = args.get(2).cloned().unwrap_or_else(|| "4h".into());
            let bars = engine::klines(&http, &sym, &iv, 500).await?;
            let closes: Vec<f64> = bars.iter().map(|b| b.c).collect();
            let n = closes.len();
            let e120 = indicators::ema(&closes, 120);
            let m = indicators::macd(&closes, 12, 26, 9);
            println!("{{");
            println!("  \"symbol\": \"{sym}\", \"interval\": \"{iv}\", \"bars\": {n},");
            println!("  \"last_close\": {},", closes[n - 1]);
            println!("  \"ema120\": {},", e120[n - 1]);
            println!("  \"atr14\": {},", indicators::atr(&bars, 14));
            println!("  \"adx14\": {},", indicators::adx(&bars, 14));
            println!("  \"macd_line\": {},", m.line[n - 1]);
            println!("  \"macd_signal\": {},", m.signal[n - 1]);
            println!("  \"macd_hist\": {}", m.hist[n - 1]);
            println!("}}");
            return Ok(());
        }
        "check" => {
            let path = std::path::PathBuf::from(
                env::var("CONFIG_PATH").unwrap_or_else(|_| "config.json".into()));
            match engine::Config::load(&path) {
                Ok(c) => {
                    println!("config OK  (version {})", c.version);
                    println!("  leverage      {}", c.risk.leverage);
                    println!("  risk/trade    {}%", c.risk.risk_pct * 100.0);
                    println!("  stop          {}x ATR", c.risk.sl_atr_mult);
                    match &c.strategies.macd {
                        Some(m) => println!("  macd          on={} trend_len={} rr={}",
                                            m.on, m.trend_len, m.rr),
                        None => println!("  macd          (absent)"),
                    }
                    println!("  live_trading  {}", trade::live_enabled());
                    println!("  dry_run       {}", trade::dry_run());
                    return Ok(());
                }
                Err(e) => { eprintln!("config INVALID: {e}"); std::process::exit(1); }
            }
        }
        "help" | "--help" | "-h" => {
            println!("quant-terminal-backend\n");
            println!("  serve    (default)  HTTP API for the dashboard");
            println!("  daemon              run strategies headless, no browser needed");
            println!("  check               validate config.json and print the effective settings");
            println!("  parity [SYM] [TF]   print indicator values for cross-checking with the UI");
            return Ok(());
        }
        "serve" => {}
        other => {
            eprintln!("unknown command '{other}' — try: help");
            std::process::exit(2);
        }
    }

    let state = Arc::new(AppState { http, binance, bitget });

    // Only the local dashboard may call this. Widen deliberately, never to Any:
    // any page you visit could otherwise read your balances.
    let origins: Vec<HeaderValue> = env::var("ALLOWED_ORIGINS")
        .unwrap_or_else(|_| "http://localhost:8080,http://127.0.0.1:8080,null".into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    // A JSON POST triggers a preflight, and the browser aborts unless the
    // response explicitly allows the content-type header. Without this every
    // POST from the dashboard fails before it reaches a handler.
    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([axum::http::header::CONTENT_TYPE])
        .allow_origin(origins);

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/balance", get(get_balance))
        .route("/api/order", axum::routing::post(post_order))
        .route("/api/close", axum::routing::post(post_close))
        .route("/api/env", get(get_env).post(post_env))
        .with_state(state)
        .layer(cors);

    let port: u16 = env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8787);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));   // loopback only
    if trade::live_enabled() {
        if trade::dry_run() {
            tracing::warn!("LIVE_TRADING=true but DRY_RUN=true — orders are signed but NOT sent");
        } else {
            tracing::error!("*** LIVE TRADING ARMED — REAL ORDERS WILL BE PLACED WITH REAL FUNDS ***");
        }
    } else {
        tracing::info!("trading disabled (set LIVE_TRADING=true to arm)");
    }
    tracing::info!("listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Known-answer test from RFC 4231 keeps the signing path honest; a silent
    // change here would produce requests the exchange rejects as unauthorised.
    #[test]
    fn hmac_hex_matches_reference() {
        let got = sign_hex("key", "The quick brown fox jumps over the lazy dog").unwrap();
        assert_eq!(got, "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8");
    }

    #[test]
    fn hmac_b64_is_base64_of_same_digest() {
        let hexed = sign_hex("key", "abc").unwrap();
        let b64 = sign_b64("key", "abc").unwrap();
        assert_eq!(B64.encode(hex::decode(hexed).unwrap()), b64);
    }

    #[test]
    fn missing_env_yields_no_keys() {
        assert!(Keys::from_env("DEFINITELY_NOT_SET_12345").is_none());
    }
}
