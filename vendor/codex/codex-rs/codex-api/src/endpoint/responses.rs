use crate::auth::SharedAuthProvider;
use crate::codebuddy_bridge;
use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::common::ResponsesApiRequest;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::requests::Compression;
use crate::requests::headers::build_session_headers;
use crate::requests::headers::insert_header;
use crate::requests::headers::subagent_header;
use crate::sse::spawn_response_stream;
use crate::telemetry::SseTelemetry;
use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::RequestCompression;
use codex_client::RequestTelemetry;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use futures::StreamExt;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use serde_json::Value;
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::mpsc;
use tracing::instrument;

pub struct ResponsesClient<T: HttpTransport> {
    session: EndpointSession<T>,
    sse_telemetry: Option<Arc<dyn SseTelemetry>>,
}

#[derive(Default)]
pub struct ResponsesOptions {
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub session_source: Option<SessionSource>,
    pub extra_headers: HeaderMap,
    pub compression: Compression,
    pub turn_state: Option<Arc<OnceLock<String>>>,
}

impl<T: HttpTransport> ResponsesClient<T> {
    pub fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            session: EndpointSession::new(transport, provider, auth),
            sse_telemetry: None,
        }
    }

    pub fn with_telemetry(
        self,
        request: Option<Arc<dyn RequestTelemetry>>,
        sse: Option<Arc<dyn SseTelemetry>>,
    ) -> Self {
        Self {
            session: self.session.with_request_telemetry(request),
            sse_telemetry: sse,
        }
    }

    #[instrument(
        name = "responses.stream_request",
        level = "info",
        skip_all,
        fields(
            transport = "responses_http",
            http.method = "POST",
            api.path = "responses"
        )
    )]
    pub async fn stream_request(
        &self,
        request: ResponsesApiRequest,
        options: ResponsesOptions,
    ) -> Result<ResponseStream, ApiError> {
        let ResponsesOptions {
            session_id,
            thread_id,
            session_source,
            extra_headers,
            compression,
            turn_state,
        } = options;

        let codebuddy = codebuddy_bridge::is_codebuddy_base_url(&self.session.provider().base_url);

        let body = if codebuddy {
            let chat = codebuddy_bridge::responses_request_to_codebuddy_chat(&request);
            EncodedJsonBody::encode(&chat).map_err(|e| {
                ApiError::Stream(format!("failed to encode codebuddy chat request: {e}"))
            })?
        } else {
            EncodedJsonBody::encode(&request).map_err(|e| {
                ApiError::Stream(format!("failed to encode responses request: {e}"))
            })?
        };

        let mut headers = extra_headers;
        if let Some(ref thread_id) = thread_id {
            insert_header(&mut headers, "x-client-request-id", thread_id);
        }
        headers.extend(build_session_headers(session_id, thread_id.clone()));
        if let Some(subagent) = subagent_header(&session_source) {
            insert_header(&mut headers, "x-openai-subagent", &subagent);
        }
        if codebuddy {
            // Live RE conversation ids help sticky multi-turn on CodeBuddy edge.
            if let Some(ref thread_id) = thread_id {
                insert_header(&mut headers, "X-Conversation-ID", thread_id);
                insert_header(&mut headers, "X-Conversation-Request-ID", thread_id);
            }
            insert_header(&mut headers, "X-Model-ID", &request.model);
            insert_header(
                &mut headers,
                "User-Agent",
                &format!("CodeBuddyCode/{}", env!("CARGO_PKG_VERSION")),
            );
        }

        if codebuddy {
            // CodeBuddy rejects zstd request bodies (magic 0x28 → parse error "invalid character '('").
            self.stream_codebuddy_chat(body, headers, Compression::None, turn_state)
                .await
        } else {
            self.stream_encoded(body, headers, compression, turn_state)
                .await
        }
    }

    fn path() -> &'static str {
        "responses"
    }

    /// Conton: stream CodeBuddy chat.completion.chunk SSE and map to ResponseEvent.
    async fn stream_codebuddy_chat(
        &self,
        body: EncodedJsonBody,
        extra_headers: HeaderMap,
        compression: Compression,
        _turn_state: Option<Arc<OnceLock<String>>>,
    ) -> Result<ResponseStream, ApiError> {
        // Always plain JSON for CodeBuddy — no zstd Content-Encoding.
        let _ = compression;
        let request_compression = RequestCompression::None;

        // Conton debug: dump full chat body (no secrets; JWT is in headers only).
        if std::env::var_os("CONTON_STREAM_DEBUG").is_some() {
            let _ = std::fs::write("/tmp/conton_stream_req.json", body.as_bytes());
        }

        let stream_response = self
            .session
            .stream_encoded_json_with(
                Method::POST,
                codebuddy_bridge::codebuddy_chat_path(),
                extra_headers,
                Some(body),
                |req| {
                    req.headers.insert(
                        http::header::ACCEPT,
                        HeaderValue::from_static("text/event-stream, application/json"),
                    );
                    // Explicit Content-Type for CodeBuddy chat JSON.
                    req.headers.insert(
                        http::header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    );
                    req.compression = request_compression;
                },
            )
            .await?;

        let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(128);
        let idle = self.session.provider().stream_idle_timeout;
        let upstream_request_id = stream_response
            .headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let mut byte_stream = stream_response.bytes;
        let debug_stream = std::env::var_os("CONTON_STREAM_DEBUG").is_some();

        tokio::spawn(async move {
            let _ = tx_event.send(Ok(ResponseEvent::Created)).await;
            // Core turn loop requires OutputItemAdded before OutputTextDelta
            // (else "OutputTextDelta without active item").
            let assistant_id = ResponseItemId::new("msg");
            let seed_item = ResponseItem::Message {
                id: Some(assistant_id.clone()),
                role: "assistant".into(),
                content: vec![ContentItem::OutputText {
                    text: String::new(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            };
            if tx_event
                .send(Ok(ResponseEvent::OutputItemAdded(seed_item)))
                .await
                .is_err()
            {
                return;
            }

            let mut full = String::new();
            let mut buffer = String::new();
            let mut done = false;
            let mut raw_debug = String::new();

            while let Some(item) = tokio::time::timeout(idle, byte_stream.next())
                .await
                .ok()
                .flatten()
            {
                let chunk = match item {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx_event
                            .send(Err(ApiError::Stream(format!("codebuddy stream: {e}"))))
                            .await;
                        return;
                    }
                };
                let chunk_str = String::from_utf8_lossy(&chunk);
                if debug_stream && raw_debug.len() < 8000 {
                    raw_debug.push_str(&chunk_str);
                }
                buffer.push_str(&chunk_str);
                while let Some(idx) = buffer.find('\n') {
                    let mut line = buffer[..idx].to_string();
                    buffer = buffer[idx + 1..].to_string();
                    if line.ends_with('\r') {
                        line.pop();
                    }
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let data = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
                    if data == "[DONE]" || codebuddy_bridge::chat_chunk_finished(data) {
                        // Still try to take final content on the finish chunk.
                        if let Some(delta) = codebuddy_bridge::chat_chunk_text_delta(data) {
                            full.push_str(&delta);
                        }
                        done = true;
                        break;
                    }
                    if let Some(delta) = codebuddy_bridge::chat_chunk_text_delta(data) {
                        // Accumulate only — emit once via OutputItemDone to avoid
                        // double-printing the same text in exec/TUI.
                        full.push_str(&delta);
                    }
                    // Surface structured API errors (credit/auth/params).
                    if full.is_empty()
                        && (data.contains("Credits exhausted")
                            || data.contains("11216")
                            || data.contains("TrialExpired")
                            || data.contains("error_msg")
                            || (data.contains("\"code\"") && data.contains("\"msg\"")))
                    {
                        let _ = tx_event
                            .send(Err(ApiError::Stream(format!(
                                "codebuddy api error: {data}"
                            ))))
                            .await;
                        return;
                    }
                }
                if done {
                    break;
                }
            }

            // Flush trailing buffer without final newline.
            if !done && !buffer.trim().is_empty() {
                let data = buffer
                    .trim()
                    .strip_prefix("data:")
                    .map(str::trim)
                    .unwrap_or(buffer.trim());
                if let Some(delta) = codebuddy_bridge::chat_chunk_text_delta(data) {
                    full.push_str(&delta);
                } else if full.is_empty()
                    && (data.contains("\"code\"")
                        || data.contains("error_msg")
                        || data.contains("\"msg\""))
                {
                    let _ = tx_event
                        .send(Err(ApiError::Stream(format!(
                            "codebuddy api error: {data}"
                        ))))
                        .await;
                    return;
                }
            }

            if debug_stream {
                let _ = std::fs::write(
                    "/tmp/conton_stream_resp.txt",
                    format!(
                        "full_len={} full={:?}\nraw_head={}\n",
                        full.len(),
                        full.chars().take(200).collect::<String>(),
                        raw_debug.chars().take(4000).collect::<String>()
                    ),
                );
            }

            // Always put full text on Done so last_agent_message / session
            // rollout stay correct (deltas alone are not enough for some sinks).
            let item = ResponseItem::Message {
                id: Some(assistant_id),
                role: "assistant".into(),
                content: vec![ContentItem::OutputText { text: full }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            };
            let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await;
            let _ = tx_event
                .send(Ok(ResponseEvent::Completed {
                    response_id: format!("conton_{}", conton_uuid_like()),
                    token_usage: None,
                    end_turn: Some(true),
                }))
                .await;
        });

        Ok(ResponseStream {
            rx_event,
            upstream_request_id,
        })
    }

    #[instrument(
        name = "responses.stream",
        level = "info",
        skip_all,
        fields(
            transport = "responses_http",
            http.method = "POST",
            api.path = "responses",
            turn.has_state = turn_state.is_some()
        )
    )]
    pub async fn stream(
        &self,
        body: Value,
        extra_headers: HeaderMap,
        compression: Compression,
        turn_state: Option<Arc<OnceLock<String>>>,
    ) -> Result<ResponseStream, ApiError> {
        let body = EncodedJsonBody::encode(&body)
            .map_err(|e| ApiError::Stream(format!("failed to encode responses request: {e}")))?;
        self.stream_encoded(body, extra_headers, compression, turn_state)
            .await
    }

    async fn stream_encoded(
        &self,
        body: EncodedJsonBody,
        extra_headers: HeaderMap,
        compression: Compression,
        turn_state: Option<Arc<OnceLock<String>>>,
    ) -> Result<ResponseStream, ApiError> {
        let request_compression = match compression {
            Compression::None => RequestCompression::None,
            Compression::Zstd => RequestCompression::Zstd,
        };

        let stream_response = self
            .session
            .stream_encoded_json_with(
                Method::POST,
                Self::path(),
                extra_headers,
                Some(body),
                |req| {
                    req.headers.insert(
                        http::header::ACCEPT,
                        HeaderValue::from_static("text/event-stream"),
                    );
                    req.compression = request_compression;
                },
            )
            .await?;

        Ok(spawn_response_stream(
            stream_response,
            self.session.provider().stream_idle_timeout,
            self.sse_telemetry.clone(),
            turn_state,
        ))
    }
}

fn conton_uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{t:x}")
}
