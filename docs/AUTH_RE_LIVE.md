# Conton auth — live reverse engineering (2026-07-12)

**Constraint:** evidence from **live** `@tencent-ai/codebuddy-code` install + `https://www.codebuddy.ai` + local Sub2API.  
**Not used as source:** `captcha-solver/codebuddy/inject/*`, trampoline, or pool.js (user request: fresh RE, not reuse).

Host: Arch Linux. Sub2API: `sub2api-codebuddy:local` on `127.0.0.1:8080`.  
DB: `accounts` platform=`codebuddy` count **2312** all `schedulable=true` at probe time.

---

## 1. CodeBuddy CLI package (live install)

| Item | Live value |
|------|------------|
| Package | `@tencent-ai/codebuddy-code@2.119.3` |
| Entry | `bin/codebuddy` → `dist/codebuddy.js` or `dist/codebuddy-headless.js` |
| Trampoline inject | **Absent** on this install (`has_trampoline=false`) |
| Auth product id | `Tencent-Cloud.coding-copilot` |
| Auth type | `cli-external-link` |
| Domains | `www.codebuddy.ai` (external / Global) |
| Token header | `Authorization` (`bearerToken`) |
| User header | `X-User-Id` (URLEncode) |
| prefixPath | `/plugin` |

### Session file (live on disk)

Path:

`~/.local/share/CodeBuddyExtension/Data/Public/auth/Tencent-Cloud.coding-copilot.info`

Shape (tokens redacted):

```json
{
  "auth": {
    "accessToken": "<jwt RS256>",
    "refreshToken": "<jwt offline>",
    "expiresAt": "ISO-8601",
    "expiresIn": 86400,
    "domain": "www.codebuddy.ai",
    "tokenType": "Bearer"
  },
  "account": { "uid": "<email>", "email": "<email>", "nickname": "…", "type": "external" }
}
```

Bundle symbols (string RE on `dist/codebuddy.js`): `FileAuthenticationStorage`, `apiKeyHelper`, `CUSTOM_TOKEN`, `handleExternalChange`, `switchBySession`, `TrialExpired` / `11216`, `axiosToFetchAdapter`, `https.request`.

### Auth endpoints (live HTTP)

| Call | Result |
|------|--------|
| `POST /v2/plugin/auth/state?platform=CLI&nonce=…` | **200** `{state, authUrl}` → `https://www.codebuddy.ai/login?platform=CLI&state=…` |
| `POST /v2/plugin/auth/token/refresh` + `X-Refresh-Token` + `X-Auth-Refresh-Source: plugin` | **200** new `accessToken` |
| `POST /v2/auth/token/refresh` | **404** Route Not Found (dead path on edge) |

JWT access token (live decode, account id=128):

- `iss`: `https://www.codebuddy.ai/auth/realms/copilot`
- `azp`: `console`
- `typ`: `Bearer`
- `email` / `preferred_username` present
- alg **RS256**

---

## 2. Credits (live site)

```http
POST https://www.codebuddy.ai/billing/meter/get-user-resource
Authorization: Bearer <accessToken>
X-User-Id: <email>
Content-Type: application/json

{}
```

Live response (account 128, 2026-07-12):

- `code=0`
- `TotalDosage=249`, `CapacityRemain=249`, `CapacitySize=250`
- Package: **CodeBuddy One-time Free 2-Week Pro Plan Trial**
- Unit: `credit`

CLI error enum (bundle): `TrialExpired = 11216`, `LicenseExpired = 11212`.

Chat credit death on production often surfaces as **HTTP 429** body text about credits (not only 11216). Gateway must treat both as “rotate account”.

---

## 3. Chat wire (live site — critical)

```http
POST https://www.codebuddy.ai/v2/chat/completions
```

### Headers required for parity (live success)

Observed from product + bundle X-* strings + successful live call:

| Header | Example |
|--------|---------|
| `Authorization` | `Bearer <jwt>` |
| `X-User-Id` | account email |
| `X-Domain` | `www.codebuddy.ai` |
| `X-Product` | `SaaS` |
| `X-Product-Version` | `2.119.3` |
| `X-IDE-Type` / `X-IDE-Name` | `CLI` |
| `X-IDE-Version` | `2.119.3` |
| `X-Agent-Intent` | `craft` |
| `X-Private-Data` | `false` |
| `X-Conversation-ID` | uuid |
| `X-Conversation-Request-ID` | hex |
| `X-Conversation-Message-ID` | hex |
| `X-Request-ID` | hex |
| `X-Model-ID` | model id |
| `Accept` | `text/event-stream, application/json` |
| `User-Agent` | `CodeBuddyCode/2.119.3` |

### Body rules (live)

| Rule | Evidence |
|------|----------|
| **Must include `system` + `user`** | User-only → **`code 11101` invalid request** (reproduced live + prior probe notes) |
| **`stream: true`** | Native path is SSE; live 200 returned `text/event-stream` |
| Model ids | e.g. `gpt-5.5`, `glm-4.6` live 200 with content |

Live 2026-07-12:

- `gpt-5.5` + system/user + full headers → **HTTP 200 SSE**
- `glm-4.6` → **HTTP 200** delta `content=pong`

---

## 4. Codex OAuth vs Conton (why we bypass OpenAI login)

Upstream Codex (`research/codex-cli/codex-rs/login`):

| Mechanism | Issuer / store |
|-----------|----------------|
| Browser PKCE | `https://auth.openai.com`, client `app_EMoamEEZ73f0CkXaXp7hrann`, localhost `:1455` |
| Device code | `/deviceauth/usercode` + poll |
| Refresh | `POST …/oauth/token` grant_type refresh_token |
| Persist | `$CODEX_HOME/auth.json` + optional keyring |
| API key path | `OPENAI_API_KEY` / `login --with-api-key` |

CodeBuddy is **Keycloak realm `copilot`** + plugin auth, **not** OpenAI Hydra.  
Conton model traffic uses **custom `model_provider` + API key** only:

```toml
model_provider = "conton"
[model_providers.conton]
name = "Conton"
base_url = "http://127.0.0.1:8080/v1"
wire_api = "responses"
# key from env OPENAI_API_KEY = Sub2API group key
```

Codex **removed** `wire_api = "chat"`; only **Responses**. Sub2API must accept `/v1/responses` and forward to CodeBuddy chat (already live).

---

## 5. Sub2API “one key / many JWT” (live)

| Check | Result |
|-------|--------|
| `GET /health` | `{"status":"ok"}` |
| `GET /v1/models` + group key | **200**, 33 models including `gpt-5.5` |
| `POST /v1/responses` model `gpt-5.5` | **200** completed, text `pong1` / `pong2` |
| Latency | ~4.1s then ~2.3s (warm) |
| Pool size | **2312** CodeBuddy rows, all schedulable |

Illusion:

1. Conton holds **one** Sub2API API key (group → platform `codebuddy`).
2. Each Responses call: Sub2API picks a schedulable JWT, injects CLI headers, calls Global.
3. On credit death / auth fail: account becomes unschedulable / error policy → **next account**.
4. Sticky headers (`session-id` / thread id) keep multi-turn on same JWT until escape.

Conton UI never sees 2000 emails — only gateway identity.

---

## 6. Conton auth implementation plan (merge-safe)

### Phase A — zero vendor patch (now)

1. `CODEX_HOME=~/.conton`
2. `config.toml` → `model_providers.conton` → Sub2API
3. `OPENAI_API_KEY=<sub2api key>`
4. Launcher `bin/conton` sets home + exec real `codex` binary
5. Do **not** run `codex login` ChatGPT OAuth for this path

### Phase B — Sub2API harden (gateway, not Codex rename)

1. Proactive probe: `POST /billing/meter/get-user-resource` when remain &lt; threshold (e.g. 8)
2. On 429 / 11216 / “Credits exhausted”: mark account, **retry same request** on next JWT (hide error from Conton)
3. Keep sticky until credit escape
4. Refresh JWT via `POST /v2/plugin/auth/token/refresh` before expiry

### Phase C — optional tiny vendor patch (only if needed)

Prefer **not** forking OAuth. If Conton must avoid `OPENAI_*` naming confusion:

- config template comments only, or
- one-line help string brand “Conton”
- still keep `AuthManager` / `CLIENT_ID` symbols for upstream merge

### Explicitly out of scope for Conton auth

- Copying `codebuddy/inject/trampoline.js` into Conton
- Patching npm `bin/codebuddy` for Conton
- Storing 2000 tokens in `~/.conton/auth.json`

---

## 7. Live command log (redacted)

```text
# credits
POST /billing/meter/get-user-resource → 200 code=0 remain=249/250 trial

# refresh
POST /v2/plugin/auth/token/refresh → 200 accessToken issued
POST /v2/auth/token/refresh → 404

# CLI device start
POST /v2/plugin/auth/state?platform=CLI → 200 authUrl

# chat fail/success
user-only body → 11101
system+user + CLI headers + stream → 200 SSE gpt-5.5 / glm-4.6

# conton path
POST http://127.0.0.1:8080/v1/responses Authorization: Bearer sk-… → 200 pong
```

Artifacts for deeper protocol (historical probes under `research/codebuddy-id/`, gateway under `research/sub2api/backend/internal/pkg/codebuddy/`) may be consulted as **secondary**; primary evidence for Conton auth decisions is this live session.
