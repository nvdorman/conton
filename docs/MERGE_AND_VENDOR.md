# Vendor merge policy

## Trees

```
captcha-solver/
  research/
    codex-cli/     # git clone of openai/codex (NESTED .git)
    conton/        # product docs + launcher + config (this folder)
    sub2api/       # gateway
```

`research/codex-cli` is a **nested git repository**. Parent `captcha-solver` may show it as untracked or submodule-like; keep vendor history independent.

## Rules

1. **Never** bulk-rename Codex crates/functions to Conton.
2. Conton branding = `CODEX_HOME`, launcher name, docs, optional help string.
3. Auth for CodeBuddy = **custom model provider + Sub2API key**, not patches to `auth.openai.com` client id for production path.
4. After `git pull` upstream inside `research/codex-cli`, re-run Conton smoke:

```bash
export CODEX_HOME=~/.conton
export OPENAI_API_KEY=…   # Sub2API
curl -sS http://127.0.0.1:8080/health
./research/conton/bin/conton exec "Reply with exactly: pong"
```

## Optional patches

If a vendor change is required, add:

```
research/conton/patches/0001-short-description.patch
```

Apply with `git -C research/codex-cli apply …` after merges. Prefer zero patches.
