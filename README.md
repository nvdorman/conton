# Conton

**Conton** = Codex architecture + **CodeBuddy Global** JWT auth + multi-account pool rotate.

- Sessions/auth always under **`~/.conton`** (never `~/.codex`)
- Model traffic: `www.codebuddy.ai` via in-tree Responses→chat bridge
- Build **on GitHub Actions** (recommended) — local release LTO is heavy on small machines

## Quick start (use a CI binary)

```bash
# clone via SSH
git clone git@github.com:nvdorman/conton.git
cd conton

# after Actions finishes: download artifact "conton-linux-x86_64"
mkdir -p dist
# place the downloaded binary as:
#   dist/conton
chmod +x dist/conton

export PATH="$PWD/bin:$PATH"

# import pool once (keep accounts.jsonl local / gitignored)
# format: one JSON object per line — see pool/accounts.example.jsonl
cp /path/to/your/accounts.jsonl pool/accounts.jsonl   # optional copy into repo (gitignored)
conton login --import-pool pool/accounts.jsonl
# or absolute path:
# conton login --import-pool ~/secrets/conton_accounts.jsonl

conton login status
cd /tmp && conton exec --skip-git-repo-check "Reply with exactly: pong" </dev/null
```

## Repo layout

| Path | Purpose |
|------|---------|
| `bin/conton` | Launcher (`CODEX_HOME=~/.conton`, vendor/release binary only) |
| `vendor/codex/` | Codex fork with Conton auth + CodeBuddy bridge |
| `config/config.example.toml` | Seeded into `~/.conton/config.toml` |
| `pool/` | Local JWT pool only — **never commit real tokens** |
| `.github/workflows/build.yml` | Release binary build on GitHub |

## Build locally (optional)

```bash
cd vendor/codex/codex-rs
# prefer debug if RAM < 16GB
cargo build -p codex-cli
# or:
CARGO_BUILD_JOBS=1 cargo build -p codex-cli --release
```

Launcher looks for:

1. `$CONTON_BIN`
2. `dist/conton` (CI artifact)
3. `vendor/codex/codex-rs/target/{release,debug}/codex`

## Home isolation

| Env | Effect |
|-----|--------|
| `CONTON_HOME` / `CODEX_HOME` | Conton data dir (default `~/.conton`) |
| `CONTON_VENDOR` | Override path to Codex fork |
| `CONTON_BIN` | Pin exact binary |

Launcher **refuses** stock Codex under `~/.codex/packages` and refuses `CODEX_HOME=~/.codex` unless `CONTON_ALLOW_CODEX_HOME=1`.

## Secrets

Do **not** commit:

- `pool/accounts.jsonl` (real JWTs)
- `~/.conton/auth.json`
- `.env` / API keys

Use Actions artifacts + local import only.

## Docs

- `docs/OAUTH_REPLACE_IN_CODEX.md`
- `docs/POOL_ROTATE.md`
- `docs/AUTH_RE_LIVE.md`

## Speed tips (vs CodeBuddy CLI)

```bash
# page-cache warm (reduces cold start of large debug binary)
conton-prewarm start
conton-prewarm status

# daily use
export PATH="$PWD/bin:$PATH"
conton exec --skip-git-repo-check -c 'approval_policy="never"' "hello" </dev/null
```

Prefer GitHub Actions **release** artifact in `dist/conton` over `target/debug/codex` (1.4G).
Edge floor for gpt-5.5 is ~4–5s TTFT (measured from CBC logs); Conton cannot beat that on the same network.
