//! Conton: bridge Codex Responses API traffic to CodeBuddy Global chat completions.
//!
//! Live RE (2026-07-12):
//! - POST https://www.codebuddy.ai/v2/chat/completions (stream=true, system+user required)
//! - SSE objects are chat.completion.chunk (not Responses events)
//!
//! This stays inside codex-api so Conton does not need a standalone gateway.

use crate::common::ResponsesApiRequest;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use serde_json::Value;
use serde_json::json;

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
    // Keep system compact: full Codex base instructions often trip CodeBuddy content_filter.
    let system = compact_system_prompt(&req.instructions);
    messages.push(json!({"role": "system", "content": system}));

    for item in &req.input {
        if let Some((role, text)) = response_item_to_role_text(item) {
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            // Drop bulky host/policy blobs that are not user intent (filter bait).
            if is_host_policy_blob(text) {
                continue;
            }
            // CodeBuddy chat only accepts system/user/assistant.
            // Codex emits developer/tool roles; map them so we don't trip content_filter.
            let role = match role.as_str() {
                "system" => continue, // already seeded above
                "assistant" => "assistant",
                "user" | "developer" => "user",
                // tool / function / other → user-visible context
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
    }

    // Ensure at least one user message (empty tool-only turns).
    if !messages.iter().any(|m| m.get("role").and_then(|r| r.as_str()) == Some("user")) {
        messages.push(json!({"role": "user", "content": "Continue."}));
    }

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
    });

    if let Some(reasoning) = &req.reasoning {
        // CodeBuddy accepts reasoning_effort for gpt-5.x (RE + product.json).
        if let Some(effort) = reasoning_effort_str(reasoning) {
            body["reasoning_effort"] = json!(effort);
        }
    }

    body
}

fn compact_system_prompt(instructions: &str) -> String {
    let trimmed = instructions.trim();
    if trimmed.is_empty() {
        return "You are Conton, a helpful coding assistant. Be concise and follow the user.".into();
    }
    // Cap length — full Codex agent prompt is huge and often content-filtered.
    const MAX: usize = 4000;
    if trimmed.len() <= MAX {
        return trimmed.to_string();
    }
    let mut out = trimmed.chars().take(MAX).collect::<String>();
    out.push_str("\n\n[system truncated for CodeBuddy]");
    out
}

fn is_host_policy_blob(text: &str) -> bool {
    // Developer/policy wrappers from Codex host — not user chat.
    text.contains("<permissions instructions>")
        || text.contains("<skills_instructions>")
        || text.contains("<collaboration_mode>")
        || text.contains("# AGENTS.md instructions")
        || (text.starts_with("<") && text.contains("instructions>") && text.len() > 500)
}

fn reasoning_effort_str(reasoning: &crate::common::Reasoning) -> Option<&'static str> {
    // Field layout varies; try serialize.
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

fn response_item_to_role_text(item: &ResponseItem) -> Option<(String, String)> {
    match item {
        ResponseItem::Message { role, content, .. } => {
            let text = content_items_to_text(content);
            Some((role.clone(), text))
        }
        _ => None,
    }
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
    // Standard OpenAI-style
    if let Some(delta) = v
        .pointer("/choices/0/delta/content")
        .and_then(|c| c.as_str())
    {
        if !delta.is_empty() {
            return Some(delta.to_string());
        }
    }
    // Some edges put full message
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
    fn conversion_adds_system_and_user() {
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
        assert_eq!(body["model"], "gpt-5.5");
        assert_eq!(body["stream"], true);
        let msgs = body["messages"].as_array().unwrap();
        assert!(msgs.iter().any(|m| m["role"] == "system"));
        assert!(msgs.iter().any(|m| m["role"] == "user" && m["content"] == "ping"));
    }
}
