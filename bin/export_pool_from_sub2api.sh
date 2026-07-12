#!/usr/bin/env bash
# One-shot DATA export of CodeBuddy JWTs from local Sub2API postgres.
# Conton does NOT call Sub2API at runtime — import the JSONL only.
set -euo pipefail
OUT="${1:-$(cd "$(dirname "$0")/.." && pwd)/pool/accounts.jsonl}"
mkdir -p "$(dirname "$OUT")"
docker exec sub2api-postgres psql -U sub2api -d sub2api -t -A -c "
COPY (
  SELECT json_build_object(
    'id', a.id,
    'email', coalesce(a.credentials->>'email', a.credentials->>'user_id', a.name),
    'user_id', coalesce(a.credentials->>'user_id', a.credentials->>'email', a.name),
    'access_token', a.credentials->>'access_token',
    'refresh_token', a.credentials->>'refresh_token',
    'base_url', coalesce(nullif(a.credentials->>'base_url',''), 'https://www.codebuddy.ai'),
    'credits_remain', (a.extra->>'codebuddy_credits_remain')::float8
  )
  FROM accounts a
  WHERE a.platform='codebuddy' AND a.deleted_at IS NULL
    AND length(coalesce(a.credentials->>'access_token','')) > 50
  ORDER BY a.id
) TO STDOUT
" > "$OUT"
echo "wrote $(wc -l < "$OUT") accounts → $OUT"
