//! Conton / CodeBuddy Global login (replaces OpenAI device-code OAuth).
//!
//! Live RE (2026-07-12) from `@tencent-ai/codebuddy-code@2.119.3` + www.codebuddy.ai:
//!
//!   POST /v2/plugin/auth/state?platform=CLI&nonce=…  → { state, authUrl }
//!   User completes browser login at authUrl
//!   GET  /v2/plugin/auth/token?state=…               → accessToken | code 11217 pending
//!   POST /v2/plugin/auth/token/refresh               → rotate access JWT
//!
//! Public entry points stay the same as upstream Codex so CLI / TUI wiring is unchanged.

use codex_http_client::HttpClient;
use http::StatusCode;
use rand::RngCore;
use serde::Deserialize;
use serde::Serialize;
use std::time::Duration;
use std::time::Instant;

use crate::default_client::create_raw_auth_client;
use crate::server::ServerOptions;
use std::io;

const ANSI_BLUE: &str = "\x1b[94m";
const ANSI_GRAY: &str = "\x1b[90m";
const ANSI_RESET: &str = "\x1b[0m";

/// Pending browser login (live RE: CodeBuddy returns this while user has not finished).
const PENDING_AUTH_CODE: i64 = 11217;

/// Default product base when ServerOptions.issuer still points at legacy OpenAI host.
pub const CODEBUDDY_DEFAULT_BASE: &str = "https://www.codebuddy.ai";

#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub verification_url: String,
    /// Display token for the user (CodeBuddy has no separate user_code; we show a short state id).
    pub user_code: String,
    /// Plugin auth `state` used for polling.
    device_auth_id: String,
    interval: u64,
}

#[derive(Deserialize)]
struct PluginEnvelope<T> {
    code: i64,
    #[serde(default)]
    msg: String,
    // No #[serde(default)] — Option already null-defaults without requiring T: Default.
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthStateData {
    state: String,
    auth_url: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthTokenData {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    domain: Option<String>,
}

#[derive(Serialize)]
struct AuthStateBody {
    nonce: String,
}

fn resolve_codebuddy_base(issuer: &str) -> String {
    let base = issuer.trim_end_matches('/');
    if base.contains("openai.com") || base.is_empty() {
        CODEBUDDY_DEFAULT_BASE.to_string()
    } else {
        base.to_string()
    }
}

fn random_nonce() -> String {
    let mut bytes = [0u8; 8];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn cli_auth_headers() -> Vec<(&'static str, String)> {
    // Live RE product headers (cli-external-link / SaaS / CLI IDE).
    vec![
        ("Accept", "application/json, text/plain, */*".to_string()),
        ("Content-Type", "application/json".to_string()),
        ("Cache-Control", "no-cache".to_string()),
        ("Pragma", "no-cache".to_string()),
        ("X-Requested-With", "XMLHttpRequest".to_string()),
        ("X-Domain", "www.codebuddy.ai".to_string()),
        ("X-No-Authorization", "true".to_string()),
        ("X-No-User-Id", "true".to_string()),
        ("X-No-Enterprise-Id", "true".to_string()),
        ("X-No-Department-Info", "true".to_string()),
        ("X-Product", "SaaS".to_string()),
        ("X-Product-Version", env!("CARGO_PKG_VERSION").to_string()),
        ("X-IDE-Type", "CLI".to_string()),
        ("X-IDE-Name", "CLI".to_string()),
        ("X-IDE-Version", env!("CARGO_PKG_VERSION").to_string()),
        (
            "User-Agent",
            format!("CodeBuddyCode/{}", env!("CARGO_PKG_VERSION")),
        ),
    ]
}

async fn start_plugin_auth(
    client: &HttpClient,
    base: &str,
) -> std::io::Result<(String, String)> {
    let nonce = random_nonce();
    let url = format!("{base}/v2/plugin/auth/state?platform=CLI&nonce={nonce}");
    let body = serde_json::to_string(&AuthStateBody {
        nonce: nonce.clone(),
    })
    .map_err(std::io::Error::other)?;

    let mut req = client.post(url).body(body);
    for (k, v) in cli_auth_headers() {
        req = req.header(k, v);
    }
    let resp = req.send().await.map_err(std::io::Error::other)?;
    let status = resp.status();
    let text = resp.text().await.map_err(std::io::Error::other)?;
    if !status.is_success() {
        return Err(std::io::Error::other(format!(
            "codebuddy auth/state failed HTTP {status}: {text}"
        )));
    }
    let env: PluginEnvelope<AuthStateData> =
        serde_json::from_str(&text).map_err(std::io::Error::other)?;
    if env.code != 0 {
        return Err(std::io::Error::other(format!(
            "codebuddy auth/state code={}: {}",
            env.code, env.msg
        )));
    }
    let data = env
        .data
        .ok_or_else(|| std::io::Error::other("codebuddy auth/state missing data"))?;
    Ok((data.state, data.auth_url))
}

async fn poll_plugin_token(
    client: &HttpClient,
    base: &str,
    state: &str,
    interval: u64,
) -> std::io::Result<AuthTokenData> {
    let url = format!("{base}/v2/plugin/auth/token?state={state}");
    let max_wait = Duration::from_secs(15 * 60);
    let start = Instant::now();

    loop {
        let mut req = client.get(&url);
        for (k, v) in cli_auth_headers() {
            // GET: drop Content-Type
            if k == "Content-Type" {
                continue;
            }
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(std::io::Error::other)?;
        let status = resp.status();
        let text = resp.text().await.map_err(std::io::Error::other)?;

        if status == StatusCode::NOT_FOUND {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "codebuddy plugin auth token endpoint not found",
            ));
        }
        if !status.is_success() {
            return Err(std::io::Error::other(format!(
                "codebuddy auth/token HTTP {status}: {text}"
            )));
        }

        let env: PluginEnvelope<AuthTokenData> =
            serde_json::from_str(&text).map_err(std::io::Error::other)?;
        if env.code == PENDING_AUTH_CODE {
            if start.elapsed() >= max_wait {
                return Err(std::io::Error::other(
                    "codebuddy login timed out after 15 minutes",
                ));
            }
            let sleep_for = Duration::from_secs(interval.max(1)).min(max_wait - start.elapsed());
            tokio::time::sleep(sleep_for).await;
            continue;
        }
        if env.code != 0 {
            return Err(std::io::Error::other(format!(
                "codebuddy auth/token code={}: {}",
                env.code, env.msg
            )));
        }
        let data = env
            .data
            .ok_or_else(|| std::io::Error::other("codebuddy auth/token missing data"))?;
        if data.access_token.trim().is_empty() {
            return Err(std::io::Error::other(
                "codebuddy auth/token returned empty accessToken",
            ));
        }
        let _ = data.token_type.as_ref();
        let _ = data.expires_in;
        let _ = data.domain.as_ref();
        return Ok(data);
    }
}

fn device_code_prompt(verification_url: &str, code: &str) -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        "\nWelcome to Conton [v{ANSI_GRAY}{version}{ANSI_RESET}]\n{ANSI_GRAY}CodeBuddy-backed coding agent (Codex architecture){ANSI_RESET}\n\
\nSign in with CodeBuddy (replaces OpenAI OAuth):\n\
\n1. Open this link in your browser and complete login\n   {ANSI_BLUE}{verification_url}{ANSI_RESET}\n\
\n2. Session ref {ANSI_GRAY}(expires in 15 minutes){ANSI_RESET}\n   {ANSI_BLUE}{code}{ANSI_RESET}\n\
\n{ANSI_GRAY}Continue only if you started this login in Conton/Codex.{ANSI_RESET}\n",
    )
}

fn print_device_code_prompt(verification_url: &str, code: &str) {
    let prompt = device_code_prompt(verification_url, code);
    println!("{prompt}");
}

pub async fn request_device_code(opts: &ServerOptions) -> std::io::Result<DeviceCode> {
    let base = resolve_codebuddy_base(&opts.issuer);
    let client = create_raw_auth_client(&base, opts.auth_route_config.as_ref())?;
    let (state, auth_url) = start_plugin_auth(&client, &base).await?;
    let short = if state.len() > 8 {
        state[..8].to_string()
    } else {
        state.clone()
    };
    Ok(DeviceCode {
        verification_url: auth_url,
        user_code: short,
        device_auth_id: state,
        interval: 2,
    })
}

pub async fn complete_device_code_login(
    opts: ServerOptions,
    device_code: DeviceCode,
) -> std::io::Result<()> {
    let base = resolve_codebuddy_base(&opts.issuer);
    let client = create_raw_auth_client(&base, opts.auth_route_config.as_ref())?;

    let token = poll_plugin_token(
        &client,
        &base,
        &device_code.device_auth_id,
        device_code.interval,
    )
    .await?;

    // Persist into the same AuthDotJson path as ChatGPT OAuth so AuthManager is unchanged.
    // Keycloak access JWT carries email; parse_chatgpt_jwt_claims tolerates missing OpenAI claims.
    let id_token = token.access_token.clone();
    let access_token = token.access_token;
    let refresh_token = token.refresh_token;

    crate::server::persist_tokens_async(
        &opts.codex_home,
        /*api_key*/ None,
        id_token,
        access_token,
        refresh_token,
        opts.cli_auth_credentials_store_mode,
        opts.auth_keyring_backend_kind,
    )
    .await
}

pub async fn run_device_code_login(opts: ServerOptions) -> std::io::Result<()> {
    let device_code = request_device_code(&opts).await?;
    // Open browser to CodeBuddy authUrl (same UX as former ChatGPT browser login).
    if opts.open_browser {
        let _ = webbrowser::open(&device_code.verification_url);
    }
    print_device_code_prompt(&device_code.verification_url, &device_code.user_code);
    complete_device_code_login(opts, device_code).await
}

#[cfg(test)]
#[path = "device_code_auth_tests.rs"]
mod tests;
