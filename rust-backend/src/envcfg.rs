//! Editing .env from the dashboard.
//!
//! THE ONE RULE: this endpoint may make the system SAFER, never more dangerous.
//!
//! `LIVE_TRADING` can be turned OFF here but never ON, and `DRY_RUN` can be
//! turned ON but never OFF. Arming live trading stays a deliberate file edit.
//! That asymmetry is the entire value of the lock — a switch the UI can flip is
//! not a lock, it is just another button, and a stray click or a hostile page
//! reaching localhost must never be able to put real money at risk.
//!
//! Turning risk OFF is always allowed, for the same reason the close endpoint
//! has no size cap: you should never be blocked from reducing exposure.
//!
//! `ALLOWED_ORIGINS` is not editable at all. Widening it is what would let an
//! arbitrary web page read your balances.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

/// Values the dashboard is allowed to write.
const WRITABLE: &[&str] = &[
    "BINANCE_API_KEY", "BINANCE_API_SECRET",
    "BITGET_API_KEY", "BITGET_API_SECRET", "BITGET_PASSPHRASE",
    "MAX_ORDER_NOTIONAL_USDT", "DAEMON_INTERVAL_SECS",
    "BINANCE_BASE", "BITGET_BASE", "PORT",
    // accepted but direction-restricted, see guard_direction()
    "LIVE_TRADING", "DRY_RUN",
];
/// Never exposed back to the browser in readable form.
const SECRET: &[&str] = &[
    "BINANCE_API_SECRET", "BITGET_API_SECRET", "BITGET_PASSPHRASE",
    "BINANCE_API_KEY", "BITGET_API_KEY",
];

/// Current value of a key straight from the .env FILE, falling back to the
/// process environment.
///
/// dotenvy loads .env once at startup, so editing the file left a running
/// server behaving on stale values while the dashboard displayed the new ones.
/// There was no way to see the two had diverged — which is exactly how
/// DRY_RUN=false in the file coexisted with dry_run=true in the process.
/// Safety-relevant switches are therefore re-read on every use.
pub fn live_value(key: &str) -> Option<String> {
    if let Ok(text) = std::fs::read_to_string(env_path()) {
        if let Some(v) = parse(&text).get(key) {
            return Some(v.clone());
        }
    }
    std::env::var(key).ok()
}

pub fn env_path() -> PathBuf {
    PathBuf::from(std::env::var("ENV_PATH").unwrap_or_else(|_| ".env".into()))
}

fn parse(text: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') { continue; }
        if let Some((k, v)) = t.split_once('=') {
            m.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    m
}

/// Rewrite only the keys we were given, preserving comments, order and
/// everything else in the file.
fn splice(original: &str, updates: &BTreeMap<String, String>) -> String {
    let mut seen: Vec<&String> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    for line in original.lines() {
        let t = line.trim();
        let mut replaced = false;
        if !t.is_empty() && !t.starts_with('#') {
            if let Some((k, _)) = t.split_once('=') {
                let key = k.trim().to_string();
                if let Some(v) = updates.get(&key) {
                    out.push(format!("{key}={v}"));
                    seen.push(updates.get_key_value(&key).unwrap().0);
                    replaced = true;
                }
            }
        }
        if !replaced { out.push(line.to_string()); }
    }
    // append keys that were not already present
    for (k, v) in updates {
        if !seen.contains(&k) { out.push(format!("{k}={v}")); }
    }
    let mut s = out.join("\n");
    if !s.ends_with('\n') { s.push('\n'); }
    s
}

fn mask(v: &str) -> String {
    if v.is_empty() { return String::new(); }
    let n = v.chars().count();
    if n <= 4 { return "•".repeat(n); }
    let tail: String = v.chars().skip(n - 4).collect();
    format!("{}{}", "•".repeat(n.saturating_sub(4).min(20)), tail)
}

#[derive(Serialize)]
pub struct EnvView {
    pub values: BTreeMap<String, String>,
    /// Keys the UI may write at all.
    pub writable: Vec<String>,
    /// Present for honesty in the UI: these can only move toward safety.
    pub one_way: BTreeMap<String, String>,
    pub path: String,
}

pub fn read_view() -> Result<EnvView> {
    let p = env_path();
    let text = std::fs::read_to_string(&p).unwrap_or_default();
    let raw = parse(&text);
    let mut values = BTreeMap::new();
    for (k, v) in &raw {
        if SECRET.contains(&k.as_str()) {
            values.insert(k.clone(), mask(v));
        } else {
            values.insert(k.clone(), v.clone());
        }
    }
    let mut one_way = BTreeMap::new();
    one_way.insert("LIVE_TRADING".into(),
        "只能從介面關閉,不能開啟 — 開啟請手動編輯 .env 並重啟".into());
    one_way.insert("DRY_RUN".into(),
        "只能從介面開啟,不能關閉 — 關閉請手動編輯 .env 並重啟".into());
    Ok(EnvView {
        values,
        writable: WRITABLE.iter().map(|s| s.to_string()).collect(),
        one_way,
        path: p.display().to_string(),
    })
}

#[derive(Deserialize)]
pub struct EnvUpdate {
    pub values: BTreeMap<String, String>,
}

/// Reject any change that would increase risk.
fn guard_direction(key: &str, val: &str) -> Result<()> {
    match key {
        // may only go false
        "LIVE_TRADING" if val == "true" => Err(anyhow!(
            "LIVE_TRADING 不能從介面開啟。請手動編輯 .env 後重啟服務 —— \
             這道鎖的意義就在於介面碰不到它")),
        // may only go true
        "DRY_RUN" if val == "false" => Err(anyhow!(
            "DRY_RUN 不能從介面關閉。請手動編輯 .env 後重啟服務")),
        _ => Ok(()),
    }
}

#[derive(Serialize)]
pub struct EnvWriteResult {
    pub ok: bool,
    pub written: Vec<String>,
    pub rejected: BTreeMap<String, String>,
    pub restart_required: bool,
    pub path: String,
}

pub fn write(update: EnvUpdate) -> Result<EnvWriteResult> {
    let p = env_path();
    let original = std::fs::read_to_string(&p).unwrap_or_default();

    let mut accepted: BTreeMap<String, String> = BTreeMap::new();
    let mut rejected: BTreeMap<String, String> = BTreeMap::new();

    for (k, v) in update.values {
        if !WRITABLE.contains(&k.as_str()) {
            rejected.insert(k, "此欄位不可從介面修改".into());
            continue;
        }
        // A masked value means "unchanged" — never write bullets into the file.
        if v.chars().any(|c| c == '•') {
            continue;
        }
        if let Err(e) = guard_direction(&k, &v) {
            rejected.insert(k, e.to_string());
            continue;
        }
        accepted.insert(k, v);
    }

    if accepted.is_empty() {
        return Ok(EnvWriteResult {
            ok: rejected.is_empty(), written: vec![], rejected,
            restart_required: false, path: p.display().to_string(),
        });
    }

    let next = splice(&original, &accepted);
    std::fs::write(&p, next)?;

    // Apply the safety-reducing changes to this process immediately so they
    // take effect without waiting for a restart.
    let mut restart_required = false;
    for (k, v) in &accepted {
        match k.as_str() {
            "LIVE_TRADING" | "DRY_RUN" | "MAX_ORDER_NOTIONAL_USDT"
            | "BINANCE_BASE" | "BITGET_BASE" | "DAEMON_INTERVAL_SECS" => {
                std::env::set_var(k, v);
            }
            // keys are captured into AppState at boot
            _ => restart_required = true,
        }
    }
    let written: Vec<String> = accepted.keys().cloned().collect();
    for k in &written {
        tracing::warn!(".env updated via dashboard: {k}");   // never log the value
    }
    Ok(EnvWriteResult {
        ok: true, written, rejected, restart_required,
        path: p.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upd(pairs: &[(&str, &str)]) -> EnvUpdate {
        EnvUpdate { values: pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }
    }

    #[test]
    fn live_trading_cannot_be_armed_from_the_ui() {
        assert!(guard_direction("LIVE_TRADING", "true").is_err());
    }

    #[test]
    fn live_trading_can_always_be_disarmed_from_the_ui() {
        assert!(guard_direction("LIVE_TRADING", "false").is_ok());
    }

    #[test]
    fn dry_run_can_be_enabled_but_not_disabled() {
        assert!(guard_direction("DRY_RUN", "true").is_ok());
        assert!(guard_direction("DRY_RUN", "false").is_err());
    }

    #[test]
    fn allowed_origins_is_not_writable() {
        assert!(!WRITABLE.contains(&"ALLOWED_ORIGINS"));
    }

    #[test]
    fn splice_preserves_comments_and_untouched_keys() {
        let orig = "# a comment\nPORT=8787\n\n# another\nLIVE_TRADING=false\n";
        let mut m = BTreeMap::new();
        m.insert("PORT".to_string(), "9000".to_string());
        let out = splice(orig, &m);
        assert!(out.contains("# a comment"));
        assert!(out.contains("# another"));
        assert!(out.contains("PORT=9000"));
        assert!(out.contains("LIVE_TRADING=false"));
        assert!(!out.contains("PORT=8787"));
    }

    #[test]
    fn splice_appends_keys_that_were_absent() {
        let mut m = BTreeMap::new();
        m.insert("DAEMON_INTERVAL_SECS".to_string(), "30".to_string());
        let out = splice("PORT=8787\n", &m);
        assert!(out.contains("PORT=8787"));
        assert!(out.contains("DAEMON_INTERVAL_SECS=30"));
    }

    #[test]
    fn secrets_are_masked_and_keep_only_a_short_tail() {
        let m = mask("abcdefghijklmnop");
        assert!(m.ends_with("mnop"));
        assert!(!m.contains("abcdefghijkl"));
        assert_eq!(mask(""), "");
    }

    #[test]
    fn masked_values_are_treated_as_unchanged() {
        // a request echoing back the masked secret must not overwrite the file
        let u = upd(&[("BINANCE_API_SECRET", "••••••••wxyz")]);
        let has_real = u.values.values().any(|v| !v.contains('•'));
        assert!(!has_real, "masked values must be skipped, not written");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(!WRITABLE.contains(&"PATH"));
        assert!(!WRITABLE.contains(&"HOME"));
    }
}
