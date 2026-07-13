# Conton tooling vs CodeBuddy CLI — live RE (2026-07-13)

**Source (not hypothesis):** local install  
`@tencent-ai/codebuddy-code@2.119.3`  
- Entry: `~/.npm-global/lib/node_modules/@tencent-ai/codebuddy-code/bin/codebuddy`  
- Bundles: `dist/codebuddy.js` (~22MB), `dist/codebuddy-headless.js` (~16MB)  
- Config: `product.json`  
- No inject/trampoline used for this RE.

**Constraint:** evidence from string/static analysis of headless bundle + `product.json` + live Conton smoke request dump (`/tmp/conton_stream_req.json`).

---

## 1. How CodeBuddy CLI actually does tools

### 1.1 Transport

| Item | Live CBC value |
|------|----------------|
| Chat wire | **OpenAI chat completions** `POST …/chat/completions` (`stream: true`) |
| Normalizer | `normalizeChatCompletionsUrl()` strips trailing `/` then ensures single `/chat/completions` suffix |
| Non-stream | Edge returns **11101** “Non-stream chat request is currently not supported” (live probe) |
| Auth | `Authorization: Bearer <JWT>` |
| User | `X-User-Id` (product: URLEncode) |
| Product headers | `X-Product`, `X-IDE-Type`, `X-IDE-Name`, `X-IDE-Version`, `X-Model-ID`, `X-Private-Data` |
| Conversation stickiness | `X-Conversation-ID`, `X-Conversation-Request-ID`, **`X-Conversation-Message-ID`** (new UUID per model request), `X-Request-ID` |
| Agent mode | `X-Agent-Intent` = session meta `codebuddy.ai/mode` **default `"craft"`** |
| Optional | `X-Agent-Purpose` (`person_agent` if `PERSONAL_AGENT_ROLE`) |
| Proxy path | `/v2/service-proxy` with `X-Service-Id` (hooks/services, not primary chat) |

### 1.2 Tool registry (product.json)

`product.json.tools` lists **48** named tools (i18n description keys only; full JSON Schema lives in tool classes in the bundle):

Core agent tools:

`Bash`, `PowerShell`, `Read`, `Write`, `Edit`, `Glob`, `Grep`, `NotebookEdit`,  
`WebFetch`, `WebSearch`, `Agent`, `ToolSearch`, `DeferExecuteTool`,  
`TaskCreate/Get/Update/List/Stop/Output`, `KillShell`, MCP helpers, plan mode, skills, media, teams, cron, …

Flags:

| Feature | Value |
|---------|-------|
| `productFeatures.DeferToolLoading` | **true** (default) |
| `productFeatures.SkipToolCallSupportCheck` | **true** |
| `fillToolCallContentModelWhitelist` | `["glm","claude"]` |
| Models `supportsToolCall` | all listed models including `gpt-5.5` |
| `requestMaxStepLimit` | **100** |

### 1.3 Deferred tool loading (speed-critical)

Bundle symbols (headless):

- `defer_loading` on tool specs  
- `ToolSearchService` / deferred tool index  
- `DeferExecuteTool` tool  
- `normalizeAgentToolSpecs` force-clear `defer_loading` when needed  
- Builder log: `[DEFER-DBG] builder.build: summaries.count=…`

**Behavior (from code paths):**  
CBC does **not** always ship all 48 full tool schemas on every request. Many tools are deferred; the model uses **`ToolSearch` / `DeferExecuteTool`** to pull schemas on demand. That keeps first-turn tokens smaller and tool routing closer to what the model was trained on.

Conton currently forwards **Codex** tool list wholesale (different names + full schemas every turn).

### 1.4 Streaming tool_calls handling (correctness-critical)

CBC `normalizeStreamingToolCallIds(chunk, seenIds)`:

1. Split SSE by lines.  
2. For each `data:` JSON with `delta.tool_calls`:  
   - If `tool_calls[].id` already in `seenIds` → **delete id from that delta** (later chunks only carry index + argument fragments).  
   - Else add id to `seenIds`.  
3. Re-stringify modified SSE line if any id stripped.

Then `ingestChatToolCalls`:

- `resolveToolCallId(id, index)`: use explicit id, else map `index → id` from first chunk  
- Append `function.name` / `function.arguments` fragments into draft  
- Internal item type: `function_call` with `callId`, `name`, `arguments`

**Why Conton feels broken/slow if wrong:**  
CodeBuddy edge often sends **id only on first tool_calls delta**; subsequent deltas omit or repeat id. CBC **strips duplicate ids**. Conton’s assembler must keep `index → id` (we do) but must not treat repeated empty names as new tools, and should strip duplicate ids when replaying/logging.

### 1.5 Finish reasons

Mapped to stop:

`stop` | `end_turn` | `stop_sequence` | **`tool_calls`** | `tool_use` | `function_call` → `"end_turn"` (internal)

Live edge uses OpenAI-style **`finish_reason: "tool_calls"`** (observed earlier).

### 1.6 Parallel tools

Request body includes **`parallel_tool_calls`** when enabled in model settings.  
Runtime uses `Promise.all` heavily for tool prep / deferred index (not only model parallel calls).

### 1.7 Startup prewarm (latency vs Conton)

`bin/cbc-prewarm` is a **tiny pure-Node** helper (no 16MB bundle load):

- IPC sockets: `codebuddy-prewarm-<id>` under `/tmp` (Unix)  
- Discovers warm daemons under `~/.codebuddy/sessions/`  
- Purpose: **millisecond** activate of already-running agent process  

So CBC interactive “tooling feels fast” is not only API: **process is already warm**, tool manager + session already in memory.

Conton `exec` currently **cold-starts** a 1.4G debug Codex binary each run → multi-second fixed cost before any tool.

### 1.8 Sandbox

Optional sandbox-cli (`@tencent-ai/sandbox-cli-*`). Bash can be projected through sandbox with escalation metadata (`codebuddy.ai/sandboxIntercept`, etc.). Host tools still run in-process when sandbox off.

---

## 2. What Conton does today (live smoke dump)

From `/tmp/conton_stream_req.json` after tool-bridge fix:

| Field | Conton value | CBC native |
|-------|--------------|------------|
| tools count | **11** | up to **48** names; many deferred |
| tool names | **Codex**: `exec_command`, `write_stdin`, `update_plan`, `create_goal`, MCP helpers, … | **CBC**: `Bash`, `Read`, `Write`, `Edit`, `Glob`, `Grep`, `ToolSearch`, … |
| body size | ~20KB (one dump) | typically smaller first turn if defer loading |
| system prompt | truncated Codex instructions (~6KB) | CBC agent prompts + tool policy |
| headers | partial (Bearer, X-User-Id, X-Domain, X-Product, X-IDE-*, X-Conversation-ID) | full set + **Message-ID** + **Agent-Intent=craft** |
| process | cold `codex` binary per `exec` | prewarmed Node agent |

Smoke proved Conton **can** call tools (loop worked: `assistant` + `tool` roles in next request). Speed gap is **expected** from architecture mismatch + cold start, not from “tools missing”.

---

## 3. Why Conton tooling is slower (evidence-based)

1. **Wrong tool dialect** — model on CodeBuddy is wired for `Bash`/`Read`/`Write`… Conton exposes Codex `exec_command`/goals. Extra reasoning + worse tool choice.  
2. **No DeferToolLoading** — Conton dumps full Codex tool schemas every turn; CBC defers many tools behind `ToolSearch`/`DeferExecuteTool`.  
3. **Cold start** — no `cbc-prewarm` equivalent; debug Conton binary ~1.4G.  
4. **Missing CBC request hygiene** — Message-ID per step, Agent-Intent `craft`, streaming id normalization (parity incomplete).  
5. **Possible extra round-trips** — Codex agent loop + permissions/skills policy (even when partially stripped) vs CBC’s native tool runner.  
6. **No connection prewarm / keep-alive reuse** across CLI invocations (CBC long-lived process keeps HTTP agents).

---

## 4. What to implement next (parity plan)

Priority order:

1. **Header parity** on every chat POST:  
   `X-Conversation-ID`, `X-Conversation-Request-ID`, `X-Conversation-Message-ID` (new per request), `X-Request-ID`, `X-Agent-Intent: craft`, `X-Model-ID`, `X-IDE-Type/Name/Version`, `X-Private-Data`.  
2. **Streaming parity**: implement CBC-equivalent `normalizeStreamingToolCallIds` before parse.  
3. **Tool dialect bridge** (big win): map Codex tools ↔ CBC names where possible:  
   - `exec_command` / shell → `Bash` `{command}`  
   - file read → `Read`  
   - apply_patch/write → `Write`/`Edit`  
   Or expose CBC tool schemas and translate results back into Codex FunctionCall names.  
4. **DeferToolLoading**: send core tools only + ToolSearch schema; expand on demand.  
5. **Long-lived Conton daemon** (like prewarm) so `conton exec` attaches instead of cold cargo binary.  
6. Release binary (not debug) from GitHub Actions.

---

## 5. Artifact pointers

| Artifact | Path |
|----------|------|
| Package | `~/.npm-global/lib/node_modules/@tencent-ai/codebuddy-code@2.119.3` |
| Headless bundle | `dist/codebuddy-headless.js` |
| Product config | `product.json` |
| Prewarm CLI | `bin/cbc-prewarm` |
| Conton bridge (ours) | `research/codex-cli/codex-rs/codex-api/src/codebuddy_bridge.rs` |
| Conton stream | `…/endpoint/responses.rs` `stream_codebuddy_chat` |

---

## 6. Bottom line

CodeBuddy CLI **does** handle the full agent tool loop itself (chat completions + streaming tool_calls + local tool executor + deferred tools + prewarm). Conton now has a **minimal** chat→tool_calls bridge into Codex’s loop, but still speaks **Codex tool language** and pays **cold-start + fat tool schema** costs. Matching CBC speed requires RE-driven parity on **headers, tool names/schemas, defer loading, and process model** — not more guessing.

---

## 7. Conton fixes applied (2026-07-13, no Codex architecture change)

All changes confined to `codex-api` Conton bridge:

| Fix | Evidence match |
|-----|----------------|
| Wire tool name `exec_command`→`Bash` | CBC product tools + Bash handler |
| Dual `cmd`/`command` args | CBC uses `command`; Codex needs `cmd` |
| Inbound `Bash`→`exec_command` + `command`→`cmd` | Codex ExecCommandHandler |
| Hot-path tool filter (drop goals/MCP list) | CBC DeferToolLoading spirit |
| `parallel_tool_calls: true` | CBC model request body |
| Assistant+tool_calls omits `content` | CBC `sanitizeEmptyContent` |
| SSE `normalizeStreamingToolCallIds` | CBC headless same name |
| Headers: Message-ID, Agent-Intent=craft, IDE/*, Model-ID, … | CBC ModelProvider |
| Shorter system + Bash tool hint | CBC compact prompts |

Live smoke after fix (`/tmp/conton-smoke`): tools=`[Bash, write_stdin, update_plan, view_image]`, assistant tool message keys=`[role, tool_calls]`, Bash exec 0ms, total wall ~20s including cold binary start + 2 model round-trips.

Env escape hatch: `CONTON_FULL_TOOLS=1` restores full Codex tool advertisement.

## 8. Latency RE (2026-07-13 live numbers)

### Pure edge (curl, same JWT/headers, gpt-5.5)

| Body | effort | wall |
|------|--------|------|
| minimal system+user | low | ~3.5s |
| minimal system+user | high | ~3.5s |
| minimal system+user | xhigh | ~4.1s |
| bloated system+tools | xhigh | ~3.1s (variance) |
| user-only (no system) | xhigh | **11101** invalid |

So for short pong, **effort is not the main driver**; API floor ≈ **3–4s**.

### Conton `exec` wall (debug binary cold start)

| | before slim | after slim (this fix) |
|--|-------------|------------------------|
| wire `reasoning_effort` | **high** (bug, user set xhigh) | **xhigh** |
| tools advertised | 4–11 | **Bash only** |
| system len | 3500–6200 | **~250** |
| body bytes | ~8.5KB | **~2.2KB** |
| `/models` 404 double | yes | **skipped** |
| pong wall | ~13s | ~12s |
| tool (Bash ls) wall | ~20s | ~18s |

### Remaining gap vs CBC CLI (honest)

CBC long-lived prewarmed Node process → first token after API floor only.  
Conton `exec` **cold-starts** 1.4G debug Codex binary every time (~5–8s fixed).  
That is **process model**, not model effort. Fix path: release binary + optional Conton daemon/prewarm (no Codex architecture change required for slim wire; daemon is launcher-level).

### CBC primary agent path note

Bundle builds OpenAI Responses-style bodies (`input`/`instructions`/`previous_response_id`) via `OpenAIResponsesModel` with `baseURL=${endpoint}/v2`.  
Live `POST /v2/responses` returns **404** on public edge; CBC `axiosToFetchAdapter` rewrites/normalizes traffic and often lands on **`/v2/chat/completions`** (observed working). Conton correctly uses chat completions as the live edge path.
