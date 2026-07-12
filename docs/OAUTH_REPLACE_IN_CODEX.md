# Conton: OAuth login diganti di arsitektur Codex (bukan gateway)

## Kesalahan desain yang dihindari

| Ditolak | Alasan |
|---------|--------|
| Standalone HTTP gateway di `research/conton/gateway` | Memecah dari AuthManager / login crate |
| Patch Sub2API untuk Conton | Bukan fokus; bukan login Codex |
| Inject `codebuddy/inject/*` / trampoline npm | Bukan arsitektur Codex; user larang reuse |

## Target arsitektur (sama dengan upstream)

```
codex login / codex login --device-auth
        │
        ▼
cli/src/login.rs          ← login_with_chatgpt / device_code entry
        │
        ▼
login/src/device_code_auth.rs   ← CodeBuddy plugin auth (RE live)
login/src/server.rs             ← DEFAULT_ISSUER = www.codebuddy.ai
login/src/auth/manager.rs       ← refresh → /v2/plugin/auth/token/refresh
login/src/auth/storage.rs       ← auth.json (AuthDotJson) TIDAK diubah shape
        │
        ▼
AuthManager + CodexAuth::Chatgpt  (mode storage tetap)
```

Simbol crate (`AuthManager`, `CodexAuth`, `TokenData`, `run_device_code_login`) **tetap**. Yang diganti = wire protocol issuer/refresh/login.

## Mapping RE live → file Codex

| CodeBuddy (RE 2026-07-12) | Codex file / fungsi |
|---------------------------|---------------------|
| `POST /v2/plugin/auth/state?platform=CLI` | `device_code_auth::request_device_code` |
| Browser `authUrl` | `webbrowser::open` + prompt Conton |
| `GET /v2/plugin/auth/token?state=` (11217 pending) | `complete_device_code_login` poll |
| Persist access/refresh JWT | `server::persist_tokens_async` → `$CODEX_HOME/auth.json` |
| `POST /v2/plugin/auth/token/refresh` + `X-Refresh-Token` | `manager::request_chatgpt_token_refresh` |
| Issuer OpenAI dihapus default | `DEFAULT_ISSUER`, `CLIENT_ID=CLI` |

## Perubahan file (vendor tree)

- `codex-rs/login/src/device_code_auth.rs` — full CodeBuddy CLI login
- `codex-rs/login/src/device_code_auth_tests.rs`
- `codex-rs/login/src/auth/manager.rs` — refresh URL + body/headers CodeBuddy
- `codex-rs/login/src/server.rs` — `DEFAULT_ISSUER`
- `codex-rs/cli/src/login.rs` — browser login → `run_device_code_login` CodeBuddy

## Pool multi-akun (sudah)

1. `login/src/auth/codebuddy_pool.rs` + `AuthManager::rotate_conton_pool` / low-credit probe
2. CLI: `codex login --import-pool path.jsonl`
3. `UnauthorizedRecovery` step `PoolRotate` setelah refresh gagal

## Masih next (in-tree)

1. **Model wire**: Responses API Codex vs CodeBuddy `/v2/chat/completions` — `core`/`model-provider`
2. Panggil `rotate_conton_pool_if_credits_exhausted` dari model client saat HTTP 429 body Credits exhausted
3. Compile full: butuh crates.io (offline sering gagal di lab ini)

## Merge upstream

Konflik utama akan di `device_code_auth.rs` / refresh constants. Strategi: keep Conton protocol functions; re-apply after `git merge origin/main`.
