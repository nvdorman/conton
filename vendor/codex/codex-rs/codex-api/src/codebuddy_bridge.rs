//! Conton: bridge Codex Responses API ↔ CodeBuddy Global chat completions.
//!
//! Live RE from `@tencent-ai/codebuddy-code@2.119.3` (2026-07-13):
//! - POST `/chat/completions` stream=true only
//! - Native tools: Bash/Read/Write/Edit/Glob/Grep (+ deferred ToolSearch…)
//! - `sanitizeEmptyContent`: assistant+tool_calls → **delete** content field
//! - `normalizeStreamingToolCallIds`: id only on first delta; strip duplicates
//! - Headers: X-Conversation-Message-ID (per request), X-Agent-Intent=craft, …
//! - DeferToolLoading: don't dump every tool schema every turn
//!
//! Conton keeps Codex architecture: FunctionCall names stay Codex-side
//! (`exec_command`, …). Wire dialect is remapped for CodeBuddy edge/model.

use crate::common::ResponsesApiRequest;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::HashSet;

pub fn is_codebuddy_base_url(base_url: &str) -> bool {
    let b = base_url.to_ascii_lowercase();
    b.contains("codebuddy.ai") || b.contains("codebuddy.cn")
}

pub fn codebuddy_chat_path() -> &'static str {
    "v2/chat/completions"
}

/// Convert a Responses request into a CodeBuddy-accepted chat completions body.
pub fn responses_request_to_codebuddy_chat(req: &ResponsesApiRequest) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    let system = compact_system_prompt(&req.instructions);
    messages.push(json!({"role": "system", "content": system}));

    let mut pending_tool_calls: Vec<Value> = Vec::new();

    let flush_tool_calls = |msgs: &mut Vec<Value>, pending: &mut Vec<Value>| {
        if pending.is_empty() {
            return;
        }
        // RE CBC sanitizeEmptyContent: omit content when tool_calls present
        // (do NOT send null — CBC deletes the field).
        let mut msg = Map::new();
        msg.insert("role".into(), json!("assistant"));
        msg.insert("tool_calls".into(), Value::Array(pending.clone()));
        msgs.push(Value::Object(msg));
        pending.clear();
    };

    for item in &req.input {
        match item {
            ResponseItem::Message { role, content, .. } => {
                flush_tool_calls(&mut messages, &mut pending_tool_calls);
                let text = content_items_to_text(content);
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                // Collapse env XML → one line; drop other host policy blobs.
                if let Some(compressed) = compress_environment_context(text) {
                    messages.push(json!({"role": "user", "content": compressed}));
                    continue;
                }
                if is_host_policy_blob(text) {
                    continue;
                }
                let role = match role.as_str() {
                    "system" => continue,
                    "assistant" => "assistant",
                    "user" | "developer" => "user",
                    other => {
                        messages.push(json!({
                            "role": "user",
                            "content": format!("[{other}] {text}"),
                        }));
                        continue;
                    }
                };
                messages.push(json!({"role": role, "content": text}));
            }
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                let wire_name = codex_tool_to_wire_name(name);
                let wire_args = codex_args_to_wire_args(name, arguments);
                pending_tool_calls.push(json!({
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": wire_name,
                        "arguments": wire_args,
                    }
                }));
            }
            ResponseItem::CustomToolCall {
                name,
                input,
                call_id,
                ..
            } => {
                let wire_name = codex_tool_to_wire_name(name);
                let wire_args = codex_args_to_wire_args(name, input);
                pending_tool_calls.push(json!({
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": wire_name,
                        "arguments": wire_args,
                    }
                }));
            }
            ResponseItem::FunctionCallOutput { call_id, output, .. }
            | ResponseItem::CustomToolCallOutput { call_id, output, .. } => {
                flush_tool_calls(&mut messages, &mut pending_tool_calls);
                let content = output
                    .body
                    .to_text()
                    .unwrap_or_else(|| String::from("(empty tool output)"));
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": content,
                }));
            }
            ResponseItem::LocalShellCall {
                call_id, action, ..
            } => {
                if let Some(cid) = call_id {
                    if let Ok(args) = serde_json::to_value(action) {
                        let command = args
                            .get("command")
                            .cloned()
                            .or_else(|| args.get("cmd").cloned())
                            .unwrap_or(args);
                        pending_tool_calls.push(json!({
                            "id": cid,
                            "type": "function",
                            "function": {
                                "name": "Bash",
                                "arguments": json!({"command": command, "cmd": command}).to_string(),
                            }
                        }));
                    }
                }
            }
            _ => {}
        }
    }
    flush_tool_calls(&mut messages, &mut pending_tool_calls);

    if !messages
        .iter()
        .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
    {
        messages.push(json!({"role": "user", "content": "Continue."}));
    }

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
    });

    if let Some(tools) = &req.tools {
        let chat_tools = responses_tools_to_chat_tools(tools);
        if !chat_tools.is_empty() {
            body["tools"] = Value::Array(chat_tools);
            body["tool_choice"] = if req.tool_choice.is_empty() {
                json!("auto")
            } else {
                json!(req.tool_choice)
            };
            // RE CBC: parallel_tool_calls on model requests when tools present.
            body["parallel_tool_calls"] = json!(true);
        }
    }

    if let Some(reasoning) = &req.reasoning {
        if let Some(effort) = reasoning_effort_str(reasoning) {
            body["reasoning_effort"] = json!(effort);
        }
    }

    body
}

/// Codex → CodeBuddy wire tool name (model dialect).
fn codex_tool_to_wire_name(name: &str) -> &str {
    match name {
        "exec_command" | "shell_command" | "shell" => "Bash",
        "write_stdin" => "write_stdin", // keep; no CBC 1:1 for PTY write
        other => other,
    }
}

/// CodeBuddy wire → Codex tool name (handler registry).
fn wire_tool_to_codex_name(name: &str) -> String {
    match name {
        "Bash" | "bash" | "PowerShell" | "powershell" | "shell" => "exec_command".into(),
        other => other.to_string(),
    }
}

/// Outbound: ensure Bash-style `command` is present for CBC-trained models.
fn codex_args_to_wire_args(codex_name: &str, arguments: &str) -> String {
    if codex_name != "exec_command" && codex_name != "shell_command" && codex_name != "shell" {
        return arguments.to_string();
    }
    let Ok(mut v) = serde_json::from_str::<Value>(arguments) else {
        return arguments.to_string();
    };
    if let Some(obj) = v.as_object_mut() {
        if !obj.contains_key("command") {
            if let Some(cmd) = obj.get("cmd").cloned() {
                obj.insert("command".into(), cmd);
            }
        }
        if !obj.contains_key("cmd") {
            if let Some(command) = obj.get("command").cloned() {
                obj.insert("cmd".into(), command);
            }
        }
    }
    v.to_string()
}

/// Inbound: map CBC Bash args to Codex exec_command (`cmd` required).
fn wire_args_to_codex_args(wire_name: &str, arguments: &str) -> String {
    let codex_name = wire_tool_to_codex_name(wire_name);
    if codex_name != "exec_command" {
        return arguments.to_string();
    }
    let Ok(mut v) = serde_json::from_str::<Value>(arguments) else {
        // Plain string command from model
        if !arguments.trim().is_empty() && !arguments.trim_start().starts_with('{') {
            return json!({"cmd": arguments, "command": arguments}).to_string();
        }
        return arguments.to_string();
    };
    if let Some(obj) = v.as_object_mut() {
        if !obj.contains_key("cmd") {
            if let Some(command) = obj.get("command").cloned() {
                obj.insert("cmd".into(), command);
            }
        }
        // Prefer quick one-shot; CBC often runs short commands without long PTY yield.
        if !obj.contains_key("yield_time_ms") {
            obj.insert("yield_time_ms".into(), json!(2500));
        }
    }
    v.to_string()
}

/// Hot-path tools only (DeferToolLoading-style). Codex still registers full set;
/// we only *advertise* these to the model for faster tool selection.
///
/// Default = Bash-only (+ apply_patch when present). Measured: advertising goals/MCP
/// tools on every turn adds schema tokens and slows tool choice vs CBC defer loading.
/// Override with CONTON_FULL_TOOLS=1.
fn is_hot_path_tool(name: &str) -> bool {
    if std::env::var("CONTON_FULL_TOOLS").ok().as_deref() == Some("1") {
        return true;
    }
    matches!(
        name,
        "exec_command"
            | "shell_command"
            | "shell"
            | "apply_patch"
            | "Bash"
            | "Read"
            | "Write"
            | "Edit"
            | "Glob"
            | "Grep"
    )
}

/// Convert Responses tools → chat tools with CBC dialect + defer filter.
fn responses_tools_to_chat_tools(tools: &[Value]) -> Vec<Value> {
    let full = std::env::var("CONTON_FULL_TOOLS").ok().as_deref() == Some("1");
    let mut out = Vec::new();
    for t in tools {
        let typ = t.get("type").and_then(|x| x.as_str()).unwrap_or("function");
        // Already chat-shaped
        if t.get("function").is_some() {
            let name = t
                .pointer("/function/name")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if !full && !is_hot_path_tool(name) {
                continue;
            }
            out.push(remap_chat_tool_entry(t.clone()));
            continue;
        }
        if typ != "function" && typ != "local_shell" {
            // Skip proprietary Responses-only types that bloat the request
            continue;
        }
        let name = t
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        if !full && !is_hot_path_tool(&name) {
            continue;
        }
        let wire_name = codex_tool_to_wire_name(&name);
        let description = t
            .get("description")
            .cloned()
            .unwrap_or_else(|| json!(""));
        let mut parameters = t
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type":"object","properties":{}}));

        // Dual cmd/command for shell tools (CBC models prefer `command`).
        if matches!(name.as_str(), "exec_command" | "shell_command" | "shell") {
            if let Some(props) = parameters
                .get_mut("properties")
                .and_then(|p| p.as_object_mut())
            {
                if !props.contains_key("command") {
                    if let Some(cmd_schema) = props.get("cmd").cloned() {
                        props.insert("command".into(), cmd_schema);
                    } else {
                        props.insert(
                            "command".into(),
                            json!({"type":"string","description":"Shell command to execute"}),
                        );
                    }
                }
            }
        }

        let mut function = json!({
            "name": wire_name,
            "description": description,
            "parameters": parameters,
        });
        if let Some(strict) = t.get("strict") {
            function["strict"] = strict.clone();
        }
        out.push(json!({
            "type": "function",
            "function": function,
        }));
    }

    // Dedupe by wire name; prefer first (hot path order preserved).
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for t in out {
        let name = t
            .pointer("/function/name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() || !seen.insert(name) {
            continue;
        }
        deduped.push(t);
    }
    deduped
}

fn remap_chat_tool_entry(mut t: Value) -> Value {
    if let Some(name) = t
        .pointer("/function/name")
        .and_then(|x| x.as_str())
        .map(str::to_string)
    {
        let wire = codex_tool_to_wire_name(&name);
        if let Some(obj) = t.get_mut("function").and_then(|f| f.as_object_mut()) {
            obj.insert("name".into(), json!(wire));
        }
    }
    t
}

fn compact_system_prompt(instructions: &str) -> String {
    // RE: CBC agent system is product-side, not a 20k Codex personality dump.
    // Live Conton dumps of truncated Codex were ~3.5–6k tokens of dead weight and
    // dominate TTFT vs CBC (API pong ~3.5s pure; Conton wall ~13s).
    // Keep a short fixed Conton system; ignore long Codex base instructions.
    let _ = instructions;
    "You are Conton, a coding agent (CodeBuddy-backed). Be concise. \
Use Bash for shell commands with JSON args {\"cmd\":\"...\"} or {\"command\":\"...\"}. \
Prefer tools over asking the user to run commands. Never claim tools are unavailable when tools are provided."
        .to_string()
}

fn is_host_policy_blob(text: &str) -> bool {
    text.contains("<permissions instructions>")
        || text.contains("<skills_instructions>")
        || text.contains("<collaboration_mode>")
        || text.contains("# AGENTS.md instructions")
        || text.contains("<environment_context>")
        || text.contains("<filesystem>")
        || text.contains("<permission_profile")
        || (text.starts_with("<") && text.contains("instructions>") && text.len() > 200)
}

/// Collapse Codex environment dumps to a single cwd line (CBC does not ship FS policy XML).
fn compress_environment_context(text: &str) -> Option<String> {
    if !text.contains("<environment_context>") && !text.contains("<cwd>") {
        return None;
    }
    let cwd = text
        .split("<cwd>")
        .nth(1)
        .and_then(|s| s.split("</cwd>").next())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let shell = text
        .split("<shell>")
        .nth(1)
        .and_then(|s| s.split("</shell>").next())
        .map(str::trim)
        .unwrap_or("bash");
    Some(match cwd {
        Some(c) => format!("Working directory: {c} (shell={shell})"),
        None => format!("Shell: {shell}"),
    })
}

fn reasoning_effort_str(reasoning: &crate::common::Reasoning) -> Option<&'static str> {
    // RE CBC product.json + UI: efforts are minimal|low|medium|high|xhigh|max.
    // Do NOT collapse xhigh→high — Conton previously sent high while user set xhigh.
    let v = serde_json::to_value(reasoning).ok()?;
    let effort = v.get("effort").and_then(|e| e.as_str())?;
    Some(match effort {
        "none" | "minimal" => "minimal",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" | "x-high" | "extra_high" => "xhigh",
        "max" => "max",
        _ if !effort.is_empty() => "high",
        _ => return None,
    })
}

fn content_items_to_text(content: &[ContentItem]) -> String {
    let mut parts = Vec::new();
    for c in content {
        match c {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                if !text.is_empty() {
                    parts.push(text.as_str());
                }
            }
            _ => {}
        }
    }
    parts.join("\n")
}

pub fn chat_chunk_text_delta(data: &str) -> Option<String> {
    let v: Value = serde_json::from_str(data).ok()?;
    if let Some(delta) = v
        .pointer("/choices/0/delta/content")
        .and_then(|c| c.as_str())
    {
        if !delta.is_empty() {
            return Some(delta.to_string());
        }
    }
    if let Some(content) = v
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
    {
        if !content.is_empty() {
            return Some(content.to_string());
        }
    }
    None
}

pub fn chat_chunk_finished(data: &str) -> bool {
    if data.trim() == "[DONE]" {
        return true;
    }
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return false;
    };
    v.pointer("/choices/0/finish_reason")
        .and_then(|f| f.as_str())
        .is_some_and(|f| !f.is_empty() && f != "null")
}

pub fn chat_chunk_finish_reason(data: &str) -> Option<String> {
    let v: Value = serde_json::from_str(data).ok()?;
    v.pointer("/choices/0/finish_reason")
        .and_then(|f| f.as_str())
        .filter(|f| !f.is_empty() && *f != "null")
        .map(str::to_string)
}

/// RE CBC `normalizeStreamingToolCallIds`: strip duplicate tool_call ids from
/// later SSE chunks (id appears only on first delta).
pub fn normalize_streaming_tool_call_ids(sse_chunk: &str, seen_ids: &mut HashSet<String>) -> String {
    if !sse_chunk.contains("tool_calls") {
        return sse_chunk.to_string();
    }
    let mut out_lines = Vec::new();
    let mut changed = false;
    for line in sse_chunk.split('\n') {
        let trimmed = line.trim_end_matches('\r');
        let Some(data) = trimmed.strip_prefix("data:") else {
            out_lines.push(line.to_string());
            continue;
        };
        let data = data.trim_start();
        if data.is_empty() || data == "[DONE]" {
            out_lines.push(line.to_string());
            continue;
        }
        let Ok(mut v) = serde_json::from_str::<Value>(data) else {
            out_lines.push(line.to_string());
            continue;
        };
        let mut line_changed = false;
        if let Some(choices) = v.get_mut("choices").and_then(|c| c.as_array_mut()) {
            if let Some(choice0) = choices.first_mut() {
                if let Some(delta) = choice0.get_mut("delta") {
                    if let Some(tcs) = delta.get_mut("tool_calls").and_then(|t| t.as_array_mut()) {
                        for tc in tcs {
                            if let Some(id) = tc.get("id").and_then(|x| x.as_str()) {
                                if !id.is_empty() {
                                    if seen_ids.contains(id) {
                                        if let Some(obj) = tc.as_object_mut() {
                                            obj.remove("id");
                                            line_changed = true;
                                        }
                                    } else {
                                        seen_ids.insert(id.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if line_changed {
            let prefix = if line.starts_with("data: ") {
                "data: "
            } else {
                "data:"
            };
            out_lines.push(format!("{prefix}{}", v));
            changed = true;
        } else {
            out_lines.push(line.to_string());
        }
    }
    if changed {
        out_lines.join("\n")
    } else {
        sse_chunk.to_string()
    }
}

#[derive(Debug, Default, Clone)]
pub struct ToolCallBuilder {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

pub fn apply_tool_call_deltas(data: &str, builders: &mut BTreeMap<u64, ToolCallBuilder>) {
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return;
    };
    let Some(arr) = v
        .pointer("/choices/0/delta/tool_calls")
        .and_then(|x| x.as_array())
    else {
        return;
    };
    for tc in arr {
        let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
        let b = builders.entry(idx).or_default();
        if let Some(id) = tc.get("id").and_then(|x| x.as_str()) {
            if !id.is_empty() {
                b.id = id.to_string();
            }
        }
        if let Some(func) = tc.get("function") {
            if let Some(name) = func.get("name").and_then(|x| x.as_str()) {
                if !name.is_empty() {
                    b.name.push_str(name);
                }
            }
            if let Some(args) = func.get("arguments").and_then(|x| x.as_str()) {
                b.arguments.push_str(args);
            }
        }
    }
}

pub fn tool_builders_to_response_items(
    builders: &BTreeMap<u64, ToolCallBuilder>,
) -> Vec<ResponseItem> {
    builders
        .values()
        .filter(|b| !b.name.is_empty())
        .map(|b| {
            let call_id = if b.id.is_empty() {
                format!("call_{}", uuid_like())
            } else {
                b.id.clone()
            };
            let codex_name = wire_tool_to_codex_name(&b.name);
            let arguments = wire_args_to_codex_args(&b.name, &b.arguments);
            ResponseItem::FunctionCall {
                id: Some(ResponseItemId::new("fc")),
                name: codex_name,
                namespace: None,
                arguments,
                call_id,
                internal_chat_message_metadata_passthrough: None,
            }
        })
        .collect()
}

fn uuid_like() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

/// CBC-style headers for each model request (RE product.json + ModelProvider).
pub fn codebuddy_request_headers(
    conversation_id: Option<&str>,
    model: &str,
) -> Vec<(&'static str, String)> {
    let message_id = uuid_like().replace('-', "");
    let mut headers = vec![
        ("X-Agent-Intent", "craft".into()),
        ("X-Product", "SaaS".into()),
        ("X-IDE-Type", "CLI".into()),
        ("X-IDE-Name", "CLI".into()),
        ("X-IDE-Version", env!("CARGO_PKG_VERSION").into()),
        ("X-Private-Data", "false".into()),
        ("X-Model-ID", model.into()),
        ("X-Conversation-Message-ID", message_id.clone()),
        ("X-Request-ID", message_id),
        (
            "User-Agent",
            format!("CodeBuddyCode/{}", env!("CARGO_PKG_VERSION")),
        ),
    ];
    if let Some(cid) = conversation_id {
        if !cid.is_empty() {
            headers.push(("X-Conversation-ID", cid.into()));
            headers.push(("X-Conversation-Request-ID", cid.into()));
            headers.push(("X-Session-ID", cid.into()));
        }
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::ResponsesApiRequest;

    #[test]
    fn detects_codebuddy_host() {
        assert!(is_codebuddy_base_url("https://www.codebuddy.ai"));
        assert!(!is_codebuddy_base_url("https://api.openai.com/v1"));
    }

    #[test]
    fn maps_exec_command_to_bash_and_filters_cold_tools() {
        let req = ResponsesApiRequest {
            model: "gpt-5.5".into(),
            instructions: "Be brief".into(),
            input: vec![ResponseItem::Message {
                id: None,
                role: "user".into(),
                content: vec![ContentItem::InputText {
                    text: "ls".into(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }],
            tools: Some(vec![
                json!({
                    "type": "function",
                    "name": "exec_command",
                    "description": "Run shell",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "cmd": {"type": "string"}
                        }
                    }
                }),
                json!({
                    "type": "function",
                    "name": "create_goal",
                    "description": "goal",
                    "parameters": {"type": "object", "properties": {}}
                }),
                json!({
                    "type": "function",
                    "name": "write_stdin",
                    "description": "pty",
                    "parameters": {"type": "object", "properties": {}}
                }),
            ]),
            tool_choice: "auto".into(),
            parallel_tool_calls: false,
            reasoning: None,
            store: false,
            stream: true,
            stream_options: None,
            include: vec![],
            service_tier: None,
            prompt_cache_key: None,
            text: None,
            client_metadata: None,
        };
        let body = responses_request_to_codebuddy_chat(&req);
        let tools = body["tools"].as_array().unwrap();
        // Bash only by default (write_stdin/create_goal filtered)
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "Bash");
        assert!(tools[0]["function"]["parameters"]["properties"]
            .get("command")
            .is_some());
        assert_eq!(body["parallel_tool_calls"], true);
        // short system, not full Codex dump
        let sys = body["messages"][0]["content"].as_str().unwrap();
        assert!(sys.len() < 800, "system too long: {}", sys.len());
        assert!(sys.contains("Bash"));
    }

    #[test]
    fn compresses_environment_context() {
        let req = ResponsesApiRequest {
            model: "gpt-5.5".into(),
            instructions: "".into(),
            input: vec![
                ResponseItem::Message {
                    id: None,
                    role: "user".into(),
                    content: vec![ContentItem::InputText {
                        text: "<environment_context><cwd>/tmp/x</cwd><shell>bash</shell></environment_context>".into(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::Message {
                    id: None,
                    role: "user".into(),
                    content: vec![ContentItem::InputText {
                        text: "hi".into(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
            ],
            tools: None,
            tool_choice: "auto".into(),
            parallel_tool_calls: false,
            reasoning: None,
            store: false,
            stream: true,
            stream_options: None,
            include: vec![],
            service_tier: None,
            prompt_cache_key: None,
            text: None,
            client_metadata: None,
        };
        let body = responses_request_to_codebuddy_chat(&req);
        let msgs = body["messages"].as_array().unwrap();
        let env = msgs.iter().find(|m| m["content"].as_str().unwrap_or("").contains("Working directory")).unwrap();
        assert!(env["content"].as_str().unwrap().contains("/tmp/x"));
    }

    #[test]
    fn inbound_bash_maps_to_exec_command_cmd() {
        let mut builders = BTreeMap::new();
        apply_tool_call_deltas(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Bash","arguments":""}}]}}]}"#,
            &mut builders,
        );
        apply_tool_call_deltas(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"","arguments":"{\"command\":\"ls -la\"}"}}]}}]}"#,
            &mut builders,
        );
        let items = tool_builders_to_response_items(&builders);
        match &items[0] {
            ResponseItem::FunctionCall {
                name, arguments, ..
            } => {
                assert_eq!(name, "exec_command");
                let v: Value = serde_json::from_str(arguments).unwrap();
                assert_eq!(v["cmd"], "ls -la");
                assert_eq!(v["command"], "ls -la");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn normalize_strips_duplicate_tool_ids() {
        let mut seen = HashSet::new();
        let chunk1 = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"name":"Bash","arguments":""}}]}}]}"#;
        let chunk2 = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"arguments":"{}"}}]}}]}"#;
        let n1 = normalize_streaming_tool_call_ids(chunk1, &mut seen);
        assert!(n1.contains("call_abc"));
        let n2 = normalize_streaming_tool_call_ids(chunk2, &mut seen);
        assert!(!n2.contains("\"id\":\"call_abc\"") || n2.contains(r#""id":null"#) || {
            // id field removed
            !n2.contains("call_abc") || n2.matches("call_abc").count() == 0
        });
        // stronger: parsed second chunk has no id
        let data = n2.strip_prefix("data:").unwrap().trim();
        let v: Value = serde_json::from_str(data).unwrap();
        assert!(v
            .pointer("/choices/0/delta/tool_calls/0/id")
            .is_none());
    }

    #[test]
    fn assistant_tool_calls_omit_content_field() {
        let req = ResponsesApiRequest {
            model: "gpt-5.5".into(),
            instructions: "".into(),
            input: vec![
                ResponseItem::Message {
                    id: None,
                    role: "user".into(),
                    content: vec![ContentItem::InputText {
                        text: "run ls".into(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::FunctionCall {
                    id: None,
                    name: "exec_command".into(),
                    namespace: None,
                    arguments: r#"{"cmd":"ls"}"#.into(),
                    call_id: "c1".into(),
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: "c1".into(),
                    output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                        "file.txt".into(),
                    ),
                    internal_chat_message_metadata_passthrough: None,
                },
            ],
            tools: None,
            tool_choice: "auto".into(),
            parallel_tool_calls: false,
            reasoning: None,
            store: false,
            stream: true,
            stream_options: None,
            include: vec![],
            service_tier: None,
            prompt_cache_key: None,
            text: None,
            client_metadata: None,
        };
        let body = responses_request_to_codebuddy_chat(&req);
        let msgs = body["messages"].as_array().unwrap();
        let assistant = msgs
            .iter()
            .find(|m| m["role"] == "assistant" && m.get("tool_calls").is_some())
            .unwrap();
        assert!(assistant.get("content").is_none());
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "Bash");
    }
}
