# Conton multi-account pool (1 fat account)

## Goal

Conton looks like **one logged-in session**. Under the hood `$CODEX_HOME` holds **~2000 CodeBuddy JWTs**; when credits on the active account are almost gone (or 401/refresh fails), `AuthManager` promotes the next JWT into the **same** `auth.json` path Codex already uses.

## Files (under `CODEX_HOME`, e.g. `~/.conton`)

| File | Role |
|------|------|
| `auth.json` | Active session (`AuthDotJson`) — **unchanged Codex shape** |
| `conton_pool.jsonl` | All CodeBuddy accounts |
| `conton_pool_state.json` | `current_id`, `exhausted_ids`, `min_credits` (default 8) |

## Code (in-tree only)

| Module | Role |
|--------|------|
| `login/src/auth/codebuddy_pool.rs` | import / rotate / credit parse (RE live) |
| `login/src/auth/manager.rs` | `rotate_conton_pool`, low-credit probe, `UnauthorizedRecovery::PoolRotate` |
| `cli` login `--import-pool` | one-shot load JSONL → pool + activate first JWT |

## Import (Arch)

```bash
# 1) One-shot data export from your Sub2API DB (optional source only — not a runtime dep)
#    research/conton/pool/accounts.jsonl  (gitignored)

export CODEX_HOME=~/.conton
mkdir -p "$CODEX_HOME"

# 2) After building Conton/codex binary from research/codex-cli:
codex login --import-pool /home/nvdorman/captcha-solver/research/conton/pool/accounts.jsonl

# 3) Verify
codex login status
```

## Rotate triggers (AuthManager)

1. **Refresh fails** → pool rotate (`refresh_failed`)
2. **After refresh**, live `get-user-resource` remain &lt; `min_credits` (default **8**) → rotate
3. **HTTP credit death** (429 Credits exhausted / 11216) → `rotate_conton_pool_if_credits_exhausted` (call from core client when wired)

## RE credit endpoint (live)

```http
POST https://www.codebuddy.ai/billing/meter/get-user-resource
Authorization: Bearer <jwt>
X-User-Id: <email>
```

Prefer `CapacityRemain` when `TotalDosage` is 0 (false empty).

## Not used

- CodeBuddy npm trampoline / inject
- Sub2API as Conton runtime
- Standalone gateway process
