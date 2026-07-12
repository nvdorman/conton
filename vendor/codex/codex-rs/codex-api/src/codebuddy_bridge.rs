//! Conton: bridge Codex Responses API traffic to CodeBuddy Global chat completions.
//!
//! Live RE (2026-07-12+):
//! - POST https://www.codebuddy.ai/v2/chat/completions (stream=true, system+user required)
//! - Tools: OpenAI chat `tools` + streaming `delta.tool_calls` (name + arguments deltas)
//! - SSE objects are chat.completion.chunk (not Responses events)
//!
//! Without tools in the chat body, Conton is text-only and the model invents
//! "filesystem tools unavailable". Tools MUST be forwarded.

use crate::common::ResponsesApiRequest;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

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

    // RE: user-only messages → 11101; always include a system message.
    // Keep system compact: full Codex base instructions often trip content_filter.
    let system = compact_system_prompt(&req.instructions);
    messages.push(json!({"role": "system", "content": system}));

    // Pending assistant tool_calls assembled across consecutive FunctionCall items.
    let mut pending_tool_calls: Vec<Value> = Vec::new();

    let flush_tool_calls = |msgs: &mut Vec<Value>, pending: &mut Vec<Value>| {
        if pending.is_empty() {
            return;
        }
        msgs.push(json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": pending.clone(),
        }));
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
                pending_tool_calls.push(json!({
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments,
                    }
                }));
            }
            ResponseItem::CustomToolCall {
                name,
                input,
                call_id,
                ..
            } => {
                pending_tool_calls.push(json!({
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": input,
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
                // Best-effort: surface prior shell intent as assistant tool call if possible.
                if let Some(cid) = call_id {
                    if let Ok(args) = serde_json::to_string(action) {
                        pending_tool_calls.push(json!({
                            "id": cid,
                            "type": "function",
                            "function": {
                                "name": "shell",
                                "arguments": args,
                            }
                        }));
                    }
                }
            }
            _ => {
                // Ignore reasoning / web search / other proprietary items for chat wire.
            }
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
            // Codex uses string tool_choice ("auto") or richer objects; pass through when simple.
            if !req.tool_choice.is_empty() {
                body["tool_choice"] = json!(req.tool_choice);
            } else {
                body["tool_choice"] = json!("auto");
            }
            if req.parallel_tool_calls {
                body["parallel_tool_calls"] = json!(true);
            }
        }
    }

    if let Some(reasoning) = &req.reasoning {
        if let Some(effort) = reasoning_effort_str(reasoning) {
            body["reasoning_effort"] = json!(effort);
        }
    }

    body
}

/// Responses tools use top-level `name`; chat completions nest under `function`.
fn responses_tools_to_chat_tools(tools: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for t in tools {
        let typ = t.get("type").and_then(|x| x.as_str()).unwrap_or("function");
        // Only map function-like tools for chat; skip web_search / local_shell proprietary types
        // unless they already look like chat tools.
        if t.get("function").is_some() {
            out.push(t.clone());
            continue;
        }
        if typ == "function" {
            let name = t
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                continue;
            }
            let description = t
                .get("description")
                .cloned()
                .unwrap_or_else(|| json!(""));
            let parameters = t
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type":"object","properties":{}}));
            let mut function = json!({
                "name": name,
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
            continue;
        }
        // local_shell / custom — map shell-like names if present
        if typ == "local_shell" || name_hint(t).as_deref() == Some("shell") {
            out.push(json!({
                "type": "function",
                "function": {
                    "name": "shell",
                    "description": "Run a shell command in the workspace",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "command": { "type": "string" },
                            "workdir": { "type": "string" }
                        },
                        "required": ["command"]
                    }
                }
            }));
        }
    }
    // Dedupe by function name
    let mut seen = BTreeMap::new();
    for t in out {
        let name = t
            .pointer("/function/name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        seen.entry(name).or_insert(t);
    }
    seen.into_values().collect()
}

fn name_hint(t: &Value) -> Option<String> {
    t.get("name")
        .and_then(|x| x.as_str())
        .map(str::to_string)
}

fn compact_system_prompt(instructions: &str) -> String {
    let trimmed = instructions.trim();
    if trimmed.is_empty() {
        return "You are Conton, a coding agent with tools (shell, file read/write, search). Use tools to inspect the workspace; never claim tools are unavailable when tools are provided.".into();
    }
    // Cap length — full Codex agent prompt is huge and often content-filtered.
    const MAX: usize = 6000;
    let mut base = if trimmed.len() <= MAX {
        trimmed.to_string()
    } else {
        let mut out = trimmed.chars().take(MAX).collect::<String>();
        out.push_str("\n\n[system truncated for CodeBuddy]");
        out
    };
    // Conton: explicit tools reminder so the model does not invent "no FS tools".
    if !base.to_ascii_lowercase().contains("use tools") {
        base.push_str(
            "\n\nYou have callable tools (shell, filesystem, etc.). Prefer tools over asking the user to run commands. Never claim filesystem tools are unavailable when a tools array is present.",
        );
    }
    base
}

fn is_host_policy_blob(text: &str) -> bool {
    text.contains("<permissions instructions>")
        || text.contains("<skills_instructions>")
        || text.contains("<collaboration_mode>")
        || text.contains("# AGENTS.md instructions")
        || (text.starts_with("<") && text.contains("instructions>") && text.len() > 500)
}

fn reasoning_effort_str(reasoning: &crate::common::Reasoning) -> Option<&'static str> {
    let v = serde_json::to_value(reasoning).ok()?;
    let effort = v.get("effort").and_then(|e| e.as_str())?;
    Some(match effort {
        "none" | "minimal" | "low" => "low",
        "medium" => "medium",
        "high" | "xhigh" | "max" => "high",
        other if !other.is_empty() => "high",
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

/// Extract assistant text delta from a CodeBuddy/OpenAI chat.completion.chunk JSON line.
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

/// Incremental tool call assembly (OpenAI/CodeBuddy chat stream).
#[derive(Debug, Default, Clone)]
pub struct ToolCallBuilder {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Apply one SSE data line's tool_calls deltas into builders keyed by index.
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

pub fn tool_builders_to_response_items(builders: &BTreeMap<u64, ToolCallBuilder>) -> Vec<ResponseItem> {
    builders
        .values()
        .filter(|b| !b.name.is_empty())
        .map(|b| {
            let call_id = if b.id.is_empty() {
                format!("call_{}", uuid_like())
            } else {
                b.id.clone()
            };
            ResponseItem::FunctionCall {
                id: Some(ResponseItemId::new("fc")),
                name: b.name.clone(),
                namespace: None,
                arguments: b.arguments.clone(),
                call_id,
                internal_chat_message_metadata_passthrough: None,
            }
        })
        .collect()
}

fn uuid_like() -> String {
    format!("{:x}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0))
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
    fn conversion_adds_system_user_and_tools() {
        let req = ResponsesApiRequest {
            model: "gpt-5.5".into(),
            instructions: "Be brief".into(),
            input: vec![ResponseItem::Message {
                id: None,
                role: "user".into(),
                content: vec![ContentItem::InputText {
                    text: "ping".into(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }],
            tools: Some(vec![json!({
                "type": "function",
                "name": "shell",
                "description": "Run shell",
                "parameters": {"type":"object","properties":{"command":{"type":"string"}}}
            })]),
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
        assert_eq!(body["model"], "gpt-5.5");
        assert!(body["tools"].as_array().unwrap().len() >= 1);
        assert_eq!(body["tools"][0]["function"]["name"], "shell");
        let msgs = body["messages"].as_array().unwrap();
        assert!(msgs.iter().any(|m| m["role"] == "system"));
        assert!(msgs.iter().any(|m| m["role"] == "user" && m["content"] == "ping"));
    }

    #[test]
    fn assembles_tool_call_deltas() {
        let mut builders = BTreeMap::new();
        apply_tool_call_deltas(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":""}}]}}]}"#,
            &mut builders,
        );
        apply_tool_call_deltas(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"","arguments":"{\"command\":\"ls\"}"}}]}}]}"#,
            &mut builders,
        );
        let items = tool_builders_to_response_items(&builders);
        assert_eq!(items.len(), 1);
        match &items[0] {
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                assert_eq!(name, "shell");
                assert!(arguments.contains("ls"));
                assert_eq!(call_id, "call_1");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
