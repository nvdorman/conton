//! Conton multi-account pool for CodeBuddy Global JWTs.
//!
//! Lives inside the Codex `login` crate so AuthManager / auth.json stay the
//! single source of truth. Pool data is plain JSONL under `$CODEX_HOME`
//! (typically `~/.conton`).
//!
//! Live RE (2026-07-12) — www.codebuddy.ai / CLI 2.119.3:
//! - Credits: POST /billing/meter/get-user-resource → CapacityRemain / TotalDosage
//! - Free trial package ≈ 250 credits
//! - Exhaust: HTTP 429 "Credits exhausted" or JSON code 11216 TrialExpired
//! - Refresh: POST /v2/plugin/auth/token/refresh + X-Refresh-Token
//!
//! "One fat account" illusion: Conton always exposes the active account via
//! the existing AuthDotJson path; when credits are almost gone, we mark that
//! pool row exhausted and promote the next JWT into auth.json.

use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;

use super::storage::AuthDotJson;
use super::storage::AuthKeyringBackendKind;
use super::storage::create_auth_storage;
use crate::token_data::TokenData;
use crate::token_data::parse_chatgpt_jwt_claims;
use codex_config::types::AuthCredentialsStoreMode;
use codex_protocol::auth::AuthMode;

/// Pool rows file (one JSON object per line).
pub const POOL_FILE_NAME: &str = "conton_pool.jsonl";
/// Rotation / exhausted state.
pub const POOL_STATE_FILE_NAME: &str = "conton_pool_state.json";

/// Rotate when remain is strictly below this (live free-trial headroom).
pub const DEFAULT_MIN_CREDITS: f64 = 8.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolAccount {
    /// Stable id (Sub2API account id as string, or email).
    pub id: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub user_id: String,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub credits_remain: Option<f64>,
    #[serde(default)]
    pub exhausted: bool,
    #[serde(default)]
    pub exhausted_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolState {
    #[serde(default)]
    pub current_id: Option<String>,
    #[serde(default)]
    pub exhausted_ids: Vec<String>,
    #[serde(default)]
    pub rotate_count: u64,
    #[serde(default = "default_min_credits")]
    pub min_credits: f64,
    #[serde(default)]
    pub last_rotate_at: Option<String>,
    #[serde(default)]
    pub last_rotate_reason: Option<String>,
}

fn default_min_credits() -> f64 {
    DEFAULT_MIN_CREDITS
}

impl Default for PoolState {
    fn default() -> Self {
        Self {
            current_id: None,
            exhausted_ids: Vec::new(),
            rotate_count: 0,
            min_credits: DEFAULT_MIN_CREDITS,
            last_rotate_at: None,
            last_rotate_reason: None,
        }
    }
}

pub fn pool_path(codex_home: &Path) -> PathBuf {
    codex_home.join(POOL_FILE_NAME)
}

pub fn pool_state_path(codex_home: &Path) -> PathBuf {
    codex_home.join(POOL_STATE_FILE_NAME)
}

pub fn load_pool(codex_home: &Path) -> std::io::Result<Vec<PoolAccount>> {
    let path = pool_path(codex_home);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(&path)?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let acc: PoolAccount = serde_json::from_str(line).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("pool line {}: {e}", i + 1),
            )
        })?;
        if acc.access_token.trim().is_empty() {
            continue;
        }
        out.push(acc);
    }
    Ok(out)
}

pub fn save_pool(codex_home: &Path, accounts: &[PoolAccount]) -> std::io::Result<()> {
    std::fs::create_dir_all(codex_home)?;
    let path = pool_path(codex_home);
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut f = opts.open(&path)?;
    for acc in accounts {
        let line = serde_json::to_string(acc).map_err(std::io::Error::other)?;
        writeln!(f, "{line}")?;
    }
    Ok(())
}

pub fn load_pool_state(codex_home: &Path) -> std::io::Result<PoolState> {
    let path = pool_state_path(codex_home);
    if !path.exists() {
        return Ok(PoolState::default());
    }
    let raw = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

pub fn save_pool_state(codex_home: &Path, state: &PoolState) -> std::io::Result<()> {
    std::fs::create_dir_all(codex_home)?;
    let path = pool_state_path(codex_home);
    let raw = serde_json::to_string_pretty(state).map_err(std::io::Error::other)?;
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut f = opts.open(path)?;
    f.write_all(raw.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

/// Import accounts from an external JSONL (e.g. one-shot Sub2API postgres dump).
/// Does **not** call Sub2API at runtime — pure file copy/normalize into CODEX_HOME.
pub fn import_pool_from_jsonl(codex_home: &Path, source: &Path) -> std::io::Result<usize> {
    let file = File::open(source)?;
    let reader = BufReader::new(file);
    let mut accounts = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("import line {}: {e}", i + 1),
            )
        })?;
        let access = v
            .get("access_token")
            .or_else(|| v.get("bearer_token"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if access.trim().is_empty() {
            continue;
        }
        let email = v
            .get("email")
            .or_else(|| v.get("user_id"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let id = v
            .get("id")
            .map(|x| match x {
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.clone(),
                _ => String::new(),
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| email.clone());
        let refresh = v
            .get("refresh_token")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let user_id = v
            .get("user_id")
            .and_then(|x| x.as_str())
            .unwrap_or(email.as_str())
            .to_string();
        let credits_remain = v
            .get("credits_remain")
            .and_then(|x| x.as_f64())
            .or_else(|| {
                v.get("extra")
                    .and_then(|e| e.get("codebuddy_credits_remain"))
                    .and_then(|x| x.as_f64())
            });
        let base_url = v
            .get("base_url")
            .and_then(|x| x.as_str())
            .map(str::to_string);
        accounts.push(PoolAccount {
            id,
            email,
            user_id,
            access_token: access,
            refresh_token: refresh,
            base_url,
            credits_remain,
            exhausted: false,
            exhausted_reason: None,
        });
    }
    let n = accounts.len();
    save_pool(codex_home, &accounts)?;
    let mut state = load_pool_state(codex_home)?;
    // Prefer first non-exhausted as current if unset.
    if state.current_id.is_none() {
        state.current_id = accounts.first().map(|a| a.id.clone());
    }
    save_pool_state(codex_home, &state)?;
    // Apply first usable account into auth.json so Conton is logged in immediately.
    if let Some(acc) = accounts.iter().find(|a| !a.exhausted) {
        apply_pool_account_to_auth(
            codex_home,
            acc,
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
        )?;
        let mut state = load_pool_state(codex_home)?;
        state.current_id = Some(acc.id.clone());
        save_pool_state(codex_home, &state)?;
    }
    Ok(n)
}

/// Write a pool account into the standard Codex AuthDotJson store (active session).
pub fn apply_pool_account_to_auth(
    codex_home: &Path,
    account: &PoolAccount,
    store_mode: AuthCredentialsStoreMode,
    keyring_backend: AuthKeyringBackendKind,
) -> std::io::Result<()> {
    let id_info = parse_chatgpt_jwt_claims(&account.access_token).map_err(std::io::Error::other)?;
    // Prefer email from JWT; fall back to pool row.
    let mut id_info = id_info;
    if id_info.email.is_none() && !account.email.is_empty() {
        id_info.email = Some(account.email.clone());
    }
    let account_id = if account.user_id.is_empty() {
        id_info.email.clone().or_else(|| Some(account.id.clone()))
    } else {
        Some(account.user_id.clone())
    };
    let tokens = TokenData {
        id_token: id_info,
        access_token: account.access_token.clone(),
        refresh_token: account.refresh_token.clone(),
        account_id,
    };
    let auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(tokens),
        last_refresh: Some(Utc::now()),
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
    };
    let storage = create_auth_storage(codex_home.to_path_buf(), store_mode, keyring_backend);
    storage.save(&auth)
}

/// Mark current pool row exhausted and promote the next usable JWT into auth.json.
/// Returns the new account id when rotation succeeded.
pub fn rotate_pool(
    codex_home: &Path,
    reason: &str,
    store_mode: AuthCredentialsStoreMode,
    keyring_backend: AuthKeyringBackendKind,
) -> std::io::Result<Option<String>> {
    let mut accounts = load_pool(codex_home)?;
    if accounts.is_empty() {
        return Err(std::io::Error::other(
            "conton pool empty — import JSONL first (codex login --import-pool path)",
        ));
    }
    let mut state = load_pool_state(codex_home)?;
    let min = if state.min_credits > 0.0 {
        state.min_credits
    } else {
        DEFAULT_MIN_CREDITS
    };

    // Exhaust current if set.
    if let Some(cur) = state.current_id.clone() {
        if let Some(acc) = accounts.iter_mut().find(|a| a.id == cur) {
            acc.exhausted = true;
            acc.exhausted_reason = Some(reason.to_string());
            if !state.exhausted_ids.iter().any(|x| x == &cur) {
                state.exhausted_ids.push(cur);
            }
        }
    }

    // Pick next non-exhausted with enough cached credits (if known).
    let next = accounts.iter().find(|a| {
        if a.exhausted || state.exhausted_ids.iter().any(|x| x == &a.id) {
            return false;
        }
        match a.credits_remain {
            Some(r) if r < min => false,
            _ => true,
        }
    });

    let Some(next) = next.cloned() else {
        save_pool(codex_home, &accounts)?;
        save_pool_state(codex_home, &state)?;
        return Err(std::io::Error::other(
            "conton pool exhausted — no schedulable CodeBuddy accounts left",
        ));
    };

    apply_pool_account_to_auth(codex_home, &next, store_mode, keyring_backend)?;
    state.current_id = Some(next.id.clone());
    state.rotate_count = state.rotate_count.saturating_add(1);
    state.last_rotate_at = Some(Utc::now().to_rfc3339());
    state.last_rotate_reason = Some(reason.to_string());
    save_pool(codex_home, &accounts)?;
    save_pool_state(codex_home, &state)?;
    tracing::info!(
        account_id = %next.id,
        reason = %reason,
        rotate_count = state.rotate_count,
        "conton pool rotated active CodeBuddy account"
    );
    Ok(Some(next.id))
}

/// Live credit snapshot from www.codebuddy.ai (RE).
#[derive(Debug, Clone)]
pub struct CreditSnapshot {
    pub capacity_remain: f64,
    pub capacity_size: f64,
    pub raw_ok: bool,
}

/// POST /billing/meter/get-user-resource with CLI-like headers.
pub async fn probe_user_resource(
    client: &codex_http_client::HttpClient,
    base_url: &str,
    access_token: &str,
    user_id: &str,
) -> std::io::Result<CreditSnapshot> {
    let base = base_url.trim_end_matches('/');
    let url = format!("{base}/billing/meter/get-user-resource");
    let resp = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/plain, */*")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("X-User-Id", user_id)
        .header("X-Domain", "www.codebuddy.ai")
        .header("X-Product", "SaaS")
        .header("X-IDE-Type", "CLI")
        .header("X-IDE-Name", "CLI")
        .header(
            "User-Agent",
            format!("CodeBuddyCode/{}", env!("CARGO_PKG_VERSION")),
        )
        .body("{}")
        .send()
        .await
        .map_err(std::io::Error::other)?;
    let status = resp.status();
    let body = resp.text().await.map_err(std::io::Error::other)?;
    if !status.is_success() {
        return Err(std::io::Error::other(format!(
            "get-user-resource HTTP {status}: {body}"
        )));
    }
    parse_credit_snapshot(&body)
}

pub fn parse_credit_snapshot(body: &str) -> std::io::Result<CreditSnapshot> {
    let v: Value = serde_json::from_str(body).map_err(std::io::Error::other)?;
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        return Err(std::io::Error::other(format!(
            "get-user-resource code={code}"
        )));
    }
    // data.Response.Data.TotalDosage + Accounts[].CapacityRemain
    let data = v
        .pointer("/data/Response/Data")
        .or_else(|| v.pointer("/data/response/data"))
        .cloned()
        .unwrap_or(Value::Null);
    let mut remain = data
        .get("TotalDosage")
        .or_else(|| data.get("total_dosage"))
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0);
    let mut size = 0.0;
    if let Some(arr) = data
        .get("Accounts")
        .or_else(|| data.get("accounts"))
        .and_then(|a| a.as_array())
    {
        let mut max_remain = 0.0_f64;
        for a in arr {
            let r = a
                .get("CapacityRemain")
                .or_else(|| a.get("capacity_remain"))
                .and_then(|x| x.as_f64())
                .or_else(|| {
                    a.get("CapacityRemainPrecise")
                        .and_then(|x| x.as_str())
                        .and_then(|s| s.parse().ok())
                })
                .unwrap_or(0.0);
            let s = a
                .get("CapacitySize")
                .or_else(|| a.get("capacity_size"))
                .and_then(|x| x.as_f64())
                .unwrap_or(0.0);
            if r > max_remain {
                max_remain = r;
            }
            if s > size {
                size = s;
            }
        }
        // Prefer package remain when TotalDosage is 0/null but CapacityRemain is healthy (RE bugfix).
        if remain <= 0.0 && max_remain > 0.0 {
            remain = max_remain;
        } else if max_remain > remain {
            remain = max_remain;
        }
    }
    Ok(CreditSnapshot {
        capacity_remain: remain,
        capacity_size: size,
        raw_ok: true,
    })
}

/// Detect live credit-death response bodies / status (RE).
pub fn is_credits_exhausted_http(status: u16, body: &str) -> bool {
    if status == 402 {
        return true;
    }
    let s = body.to_ascii_lowercase();
    if s.contains("credits exhausted") {
        return true;
    }
    if s.contains("trialexpired") || s.contains("trial expired") {
        return true;
    }
    if s.contains("\"code\":11216") || s.contains("\"code\": 11216") {
        return true;
    }
    if status == 429 && (s.contains("credit") || s.contains("quota") || s.is_empty()) {
        return true;
    }
    false
}

pub fn should_rotate_for_credits(remain: f64, min_credits: f64) -> bool {
    let min = if min_credits > 0.0 {
        min_credits
    } else {
        DEFAULT_MIN_CREDITS
    };
    remain >= 0.0 && remain < min
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_live_shaped_credits() {
        let raw = r#"{"code":0,"msg":"OK","data":{"Response":{"Data":{"TotalCount":1,"TotalDosage":249,"Accounts":[{"CapacityRemain":249,"CapacitySize":250,"CapacityRemainPrecise":"249.5"}]}}}}"#;
        let snap = parse_credit_snapshot(raw).unwrap();
        assert!(snap.capacity_remain >= 249.0);
        assert_eq!(snap.capacity_size, 250.0);
    }

    #[test]
    fn parse_prefers_capacity_when_total_zero() {
        let raw = r#"{"code":0,"data":{"Response":{"Data":{"TotalDosage":0,"Accounts":[{"CapacityRemain":200,"CapacitySize":250}]}}}}"#;
        let snap = parse_credit_snapshot(raw).unwrap();
        assert_eq!(snap.capacity_remain, 200.0);
    }

    #[test]
    fn exhaust_detector() {
        assert!(is_credits_exhausted_http(
            429,
            "Credits exhausted. Please visit https://www.codebuddy.ai/profile/usage"
        ));
        assert!(is_credits_exhausted_http(400, r#"{"code":11216,"msg":"TrialExpired"}"#));
        assert!(!is_credits_exhausted_http(
            400,
            r#"{"code":11101,"msg":"invalid request"}"#
        ));
    }

    #[test]
    fn rotate_threshold() {
        assert!(should_rotate_for_credits(0.0, 8.0));
        assert!(should_rotate_for_credits(7.9, 8.0));
        assert!(!should_rotate_for_credits(8.0, 8.0));
        assert!(!should_rotate_for_credits(249.0, 8.0));
    }
}
