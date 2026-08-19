//! An in-process OpenAI-shaped provider for integration tests: records
//! every request it receives and scripts its behavior off the request's
//! `model` field, so tests drive error paths without real network flakiness.
//!
//! Behavior models:
//! - `err-500`  -> 500 with a provider-style error body
//! - `err-429`  -> 429 with `retry-after: 7`
//! - `slow`     -> 2s delay before responding
//! - `slow-body` -> headers immediately, body 500ms later (Anthropic only)
//! - anything else -> a well-formed completion (or SSE stream when
//!   `"stream": true`), echoing the model name it was asked for

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub path: String,
    pub authorization: Option<String>,
    /// `x-api-key` (Anthropic) or `x-goog-api-key` (Gemini).
    pub api_key: Option<String>,
    pub body: Value,
    /// Every request header, lowercased. The subscription transports are
    /// defined largely by the headers they send, so asserting on them is
    /// asserting on the feature.
    pub headers: std::collections::BTreeMap<String, String>,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

#[derive(Clone, Default)]
struct Shared {
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    /// Per-model hit counts, for `recover-after-N` scripting.
    hits: Arc<Mutex<std::collections::HashMap<String, u32>>>,
}

#[derive(Clone)]
pub struct MockProvider {
    pub addr: SocketAddr,
    shared: Shared,
}

impl MockProvider {
    pub async fn spawn() -> Self {
        let shared = Shared::default();
        let app = Router::new()
            .route("/chat/completions", post(chat_completions))
            .route("/completions", post(completions))
            .route("/embeddings", post(embeddings))
            .route("/responses", post(openai_responses))
            .route("/openai/deployments/{deployment}/chat/completions", post(azure_chat))
            .route("/model/{model_action}/converse", post(bedrock_converse))
            .route("/model/{model_action}/converse-stream", post(bedrock_converse_stream))
            .route(
                "/v1/projects/{project}/locations/{location}/publishers/google/models/{model_action}",
                post(vertex_generate),
            )
            .route("/anything/{*rest}", axum::routing::any(record_anything))
            .route("/v1/messages", post(anthropic_messages))
            .route("/v1beta/models/{model_action}", post(gemini_generate))
            .route("/backend-api/codex/responses", post(codex_responses))
            .layer(axum::extract::DefaultBodyLimit::max(128 * 1024 * 1024))
            .with_state(shared.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { addr, shared }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.shared.requests.lock().unwrap().clone()
    }

    pub fn last_request(&self) -> RecordedRequest {
        self.shared
            .requests
            .lock()
            .unwrap()
            .last()
            .expect("no requests recorded")
            .clone()
    }

    pub fn request_count(&self) -> usize {
        self.shared.requests.lock().unwrap().len()
    }
}

/// Recording is capped so long-running rigs (soak) measure the gateway,
/// not the mock's memory of every request it ever saw.
const MAX_RECORDED: usize = 1000;

async fn record(shared: &Shared, path: &str, headers: &HeaderMap, body: &[u8]) -> Value {
    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    {
        let mut requests = shared.requests.lock().unwrap();
        if requests.len() >= MAX_RECORDED {
            requests.remove(0);
        }
    }
    shared.requests.lock().unwrap().push(RecordedRequest {
        path: path.to_owned(),
        authorization: headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        api_key: headers
            .get("x-api-key")
            .or_else(|| headers.get("x-goog-api-key"))
            .or_else(|| headers.get("api-key"))
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        body: parsed.clone(),
        headers: headers
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|v| (name.as_str().to_ascii_lowercase(), v.to_owned()))
            })
            .collect(),
    });
    parsed
}

async fn scripted_failure(shared: &Shared, model: &str) -> Option<Response> {
    // `recover-after-N`: fail with 500 for the first N hits, then succeed —
    // the shape breaker-recovery tests need.
    if let Some(n) = model
        .strip_prefix("recover-after-")
        .and_then(|n| n.parse::<u32>().ok())
    {
        let mut hits = shared.hits.lock().unwrap();
        let count = hits.entry(model.to_owned()).or_insert(0);
        *count += 1;
        if *count <= n {
            return Some(
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(
                        json!({"error": {"message": "still recovering", "type": "server_error"}}),
                    ),
                )
                    .into_response(),
            );
        }
        return None;
    }
    match model {
        "err-500" => Some(
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(
                    json!({"error": {"message": "upstream exploded", "type": "server_error"}}),
                ),
            )
                .into_response(),
        ),
        "err-429" => Some(
            (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, "7")],
                axum::Json(
                    json!({"error": {"message": "rate limited", "type": "rate_limit_error"}}),
                ),
            )
                .into_response(),
        ),
        // A subscription seat out of quota, with the header shapes the
        // real backends send. Note what is NOT here: Codex sends no
        // `retry-after` at all, so a gateway that reads only that header
        // learns nothing and re-probes an exhausted seat forever.
        "quota-codex" => Some(
            (
                StatusCode::TOO_MANY_REQUESTS,
                [
                    ("x-codex-primary-used-percent", "100"),
                    ("x-codex-primary-reset-after-seconds", "380613"),
                    ("x-codex-primary-window-minutes", "10080"),
                    ("x-codex-plan-type", "pro"),
                ],
                axum::Json(json!({"error": {
                    "type": "usage_limit_reached",
                    "message": "The usage limit has been reached",
                    "resets_in_seconds": 380612
                }})),
            )
                .into_response(),
        ),
        "quota-claude" => Some(
            (
                StatusCode::TOO_MANY_REQUESTS,
                [
                    ("retry-after", "3311"),
                    ("anthropic-ratelimit-unified-5h-utilization", "1.01"),
                    ("anthropic-ratelimit-unified-5h-status", "rejected"),
                    ("anthropic-ratelimit-unified-7d-utilization", "0.22"),
                    ("anthropic-ratelimit-unified-7d-status", "allowed"),
                ],
                axum::Json(json!({"type": "error", "error": {
                    "type": "rate_limit_error",
                    "message": "This request would exceed your account's rate limit."
                }})),
            )
                .into_response(),
        ),
        "slow" => {
            tokio::time::sleep(Duration::from_secs(2)).await;
            None
        }
        _ => None,
    }
}

async fn chat_completions(
    State(shared): State<Shared>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let parsed = record(&shared, "/chat/completions", &headers, &body).await;
    let model = parsed["model"].as_str().unwrap_or("unknown").to_owned();
    if let Some(failure) = scripted_failure(&shared, &model).await {
        return failure;
    }

    if parsed["stream"] == json!(true) {
        return sse_stream(model, parsed.get("tools").is_some());
    }

    axum::Json(json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "mock response"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}
    }))
    .into_response()
}

/// A realistic streamed completion: role chunk, three content deltas (or
/// tool-call argument deltas), a finish chunk, `[DONE]`. Chunks are spaced
/// out so buffering anywhere in the gateway is visible to tests as a
/// collapsed inter-chunk gap.
fn sse_stream(model: String, with_tools: bool) -> Response {
    let chunk = move |delta: Value, finish: Value| {
        json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "model": model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
        })
        .to_string()
    };

    let deltas: Vec<String> = if with_tools {
        vec![
            chunk(
                json!({"role": "assistant", "tool_calls": [{"index": 0, "id": "call_1",
                       "type": "function", "function": {"name": "get_weather", "arguments": ""}}]}),
                Value::Null,
            ),
            chunk(
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"city\":"}}]}),
                Value::Null,
            ),
            chunk(
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"Paris\"}"}}]}),
                Value::Null,
            ),
            chunk(json!({}), json!("tool_calls")),
        ]
    } else {
        vec![
            chunk(json!({"role": "assistant", "content": ""}), Value::Null),
            chunk(json!({"content": "mock "}), Value::Null),
            chunk(json!({"content": "stream"}), Value::Null),
            chunk(json!({}), json!("stop")),
        ]
    };

    let stream = futures_util::stream::unfold(0usize, move |i| {
        let deltas = deltas.clone();
        async move {
            if i < deltas.len() {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let frame = format!("data: {}\n\n", deltas[i]);
                Some((Ok::<_, std::convert::Infallible>(frame), i + 1))
            } else if i == deltas.len() {
                Some((Ok("data: [DONE]\n\n".to_owned()), i + 1))
            } else {
                None
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn completions(
    State(shared): State<Shared>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let parsed = record(&shared, "/completions", &headers, &body).await;
    axum::Json(json!({
        "id": "cmpl-mock",
        "object": "text_completion",
        "model": parsed["model"],
        "choices": [{"index": 0, "text": "mock", "finish_reason": "stop"}]
    }))
    .into_response()
}

async fn embeddings(
    State(shared): State<Shared>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let parsed = record(&shared, "/embeddings", &headers, &body).await;
    axum::Json(json!({
        "object": "list",
        "model": parsed["model"],
        "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]}],
        "usage": {"prompt_tokens": 4, "total_tokens": 4}
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Anthropic-dialect upstream
// ---------------------------------------------------------------------------

async fn anthropic_messages(
    State(shared): State<Shared>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let parsed = record(&shared, "/v1/messages", &headers, &body).await;
    let model = parsed["model"].as_str().unwrap_or("unknown").to_owned();
    if let Some(failure) = scripted_failure(&shared, &model).await {
        return failure;
    }
    let with_tools = parsed.get("tools").is_some();
    // A forced specific tool (workflow: json_schema emulation or named
    // tool_choice) answers with that tool.
    let forced_tool = parsed["tool_choice"]["name"].as_str().map(str::to_owned);

    if parsed["stream"] == json!(true) {
        return anthropic_stream(model, with_tools, forced_tool);
    }

    let content = if let Some(name) = forced_tool {
        json!([{"type": "tool_use", "id": "toolu_forced", "name": name,
                "input": {"answer": "structured", "n": 7}}])
    } else if with_tools {
        json!([
            {"type": "text", "text": "let me check"},
            {"type": "tool_use", "id": "toolu_01", "name": "get_weather",
             "input": {"city": "Paris"}},
            {"type": "tool_use", "id": "toolu_02", "name": "get_weather",
             "input": {"city": "Tokyo"}}
        ])
    } else {
        json!([{"type": "text", "text": "mock response"}])
    };
    let stop_reason = if with_tools || parsed["tool_choice"].is_object() {
        "tool_use"
    } else {
        "end_turn"
    };

    let payload = json!({
        "id": "msg_mock",
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {"input_tokens": 11, "output_tokens": 5}
    });
    if model == "slow-body" {
        return delayed_body(payload, Duration::from_millis(500));
    }
    axum::Json(payload).into_response()
}

/// Headers now, body later — a provider that has started answering but
/// has not finished generating. Every real upstream we translate for
/// behaves this way, and it is the only shape that tells a gateway which
/// stops its upstream clock at the headers from one that does not.
fn delayed_body(payload: Value, delay: Duration) -> Response {
    let stream = futures_util::stream::once(async move {
        tokio::time::sleep(delay).await;
        Ok::<_, std::convert::Infallible>(bytes::Bytes::from(payload.to_string()))
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from_stream(stream))
        .expect("static response parts")
}

/// Anthropic event stream: ping interleaved, tool arguments split across
/// three `input_json_delta` fragments (mid-token, to exercise
/// reassembly), realistic event ordering.
fn anthropic_stream(model: String, with_tools: bool, forced_tool: Option<String>) -> Response {
    let mut frames: Vec<String> = Vec::new();
    let ev = |name: &str, data: Value| format!("event: {name}\ndata: {data}\n\n");

    frames.push(ev(
        "message_start",
        json!({"type": "message_start", "message": {
            "id": "msg_mock", "type": "message", "role": "assistant", "model": model,
            "content": [], "stop_reason": null,
            "usage": {"input_tokens": 11, "output_tokens": 0}}}),
    ));
    frames.push(ev("ping", json!({"type": "ping"})));

    if let Some(name) = forced_tool {
        frames.push(ev(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "tool_use", "id": "toolu_forced", "name": name, "input": {}}}),
        ));
        for fragment in ["{\"answer\":", " \"structu", "red\", \"n\": 7}"] {
            frames.push(ev(
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0,
                       "delta": {"type": "input_json_delta", "partial_json": fragment}}),
            ));
        }
        frames.push(ev(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ));
        frames.push(ev(
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": null},
                   "usage": {"output_tokens": 9}}),
        ));
    } else if with_tools {
        frames.push(ev(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}}),
        ));
        frames.push(ev(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "let me check"}}),
        ));
        frames.push(ev(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ));
        frames.push(ev(
            "content_block_start",
            json!({"type": "content_block_start", "index": 1,
                   "content_block": {"type": "tool_use", "id": "toolu_01", "name": "get_weather", "input": {}}}),
        ));
        for fragment in ["{\"ci", "ty\": \"Pa", "ris\"}"] {
            frames.push(ev(
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 1,
                       "delta": {"type": "input_json_delta", "partial_json": fragment}}),
            ));
        }
        frames.push(ev(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 1}),
        ));
        frames.push(ev(
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": null},
                   "usage": {"output_tokens": 9}}),
        ));
    } else {
        frames.push(ev(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}}),
        ));
        for text in ["mock ", "stream"] {
            frames.push(ev(
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0,
                       "delta": {"type": "text_delta", "text": text}}),
            ));
        }
        frames.push(ev(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ));
        frames.push(ev(
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                   "usage": {"output_tokens": 4}}),
        ));
    }
    frames.push(ev("message_stop", json!({"type": "message_stop"})));

    paced_sse(frames)
}

// ---------------------------------------------------------------------------
// Gemini-dialect upstream
// ---------------------------------------------------------------------------

async fn gemini_generate(
    State(shared): State<Shared>,
    axum::extract::Path(model_action): axum::extract::Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let record_path = format!("/v1beta/models/{model_action}");
    gemini_generate_inner(shared, model_action, record_path, headers, body).await
}

async fn gemini_generate_inner(
    shared: Shared,
    model_action: String,
    record_path: String,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let (model, action) = model_action
        .split_once(':')
        .unwrap_or((model_action.as_str(), ""));
    let model = model.to_owned();
    let parsed = record(&shared, &record_path, &headers, &body).await;
    if let Some(failure) = scripted_failure(&shared, &model).await {
        return failure;
    }
    let with_tools = parsed.get("tools").is_some();
    let json_mode = parsed["generationConfig"]["responseSchema"].is_object();

    let (parts, finish) = if json_mode {
        (
            json!([{"text": "{\"answer\": \"structured\", \"n\": 7}"}]),
            "STOP",
        )
    } else if with_tools {
        (
            json!([{"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}},
                   {"functionCall": {"name": "get_weather", "args": {"city": "Tokyo"}}}]),
            "STOP",
        )
    } else {
        (json!([{"text": "mock response"}]), "STOP")
    };

    let usage = json!({"promptTokenCount": 11, "candidatesTokenCount": 5, "totalTokenCount": 16});

    if action == "streamGenerateContent" {
        let mk = |parts: Value, finish: Option<&str>| {
            let mut chunk = json!({
                "candidates": [{"content": {"role": "model", "parts": parts}, "index": 0}],
                "modelVersion": model, "responseId": "mockresp",
            });
            if let Some(f) = finish {
                chunk["candidates"][0]["finishReason"] = json!(f);
                chunk["usageMetadata"] = usage.clone();
            }
            format!("data: {chunk}\n\n")
        };
        let frames = if with_tools {
            vec![
                mk(
                    json!([{"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}}]),
                    None,
                ),
                mk(
                    json!([{"functionCall": {"name": "get_weather", "args": {"city": "Tokyo"}}}]),
                    Some("STOP"),
                ),
            ]
        } else {
            vec![
                mk(json!([{"text": "mock "}]), None),
                mk(json!([{"text": "stream"}]), Some("STOP")),
            ]
        };
        return paced_sse(frames);
    }

    axum::Json(json!({
        "candidates": [{"content": {"role": "model", "parts": parts},
                        "finishReason": finish, "index": 0}],
        "usageMetadata": usage,
        "modelVersion": model,
        "responseId": "mockresp",
    }))
    .into_response()
}

/// Emit pre-built SSE frames with 40ms pacing, so gateway buffering shows
/// up in tests as collapsed inter-frame gaps.
fn paced_sse(frames: Vec<String>) -> Response {
    let stream = futures_util::stream::unfold(0usize, move |i| {
        let frames = frames.clone();
        async move {
            if i < frames.len() {
                tokio::time::sleep(Duration::from_millis(40)).await;
                Some((Ok::<_, std::convert::Infallible>(frames[i].clone()), i + 1))
            } else {
                None
            }
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

// ---------------------------------------------------------------------------
// OpenAI Responses-dialect upstream (native relay target)
// ---------------------------------------------------------------------------

async fn openai_responses(
    State(shared): State<Shared>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let parsed = record(&shared, "/responses", &headers, &body).await;
    let model = parsed["model"].as_str().unwrap_or("unknown").to_owned();
    if let Some(failure) = scripted_failure(&shared, &model).await {
        return failure;
    }
    let with_tools = parsed.get("tools").is_some();

    if parsed["stream"] == json!(true) {
        let ev = |name: &str, data: Value| format!("event: {name}\ndata: {data}\n\n");
        let frames = if with_tools {
            vec![
                ev(
                    "response.created",
                    json!({"type": "response.created", "sequence_number": 0,
                    "response": {"id": "resp_mock", "object": "response", "status": "in_progress", "model": model, "output": []}}),
                ),
                ev(
                    "response.output_item.added",
                    json!({"type": "response.output_item.added", "sequence_number": 1,
                    "output_index": 0, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1",
                    "name": "get_weather", "arguments": "", "status": "in_progress"}}),
                ),
                ev(
                    "response.function_call_arguments.delta",
                    json!({"type": "response.function_call_arguments.delta",
                    "sequence_number": 2, "item_id": "fc_1", "output_index": 0, "delta": "{\"city\":"}),
                ),
                ev(
                    "response.function_call_arguments.delta",
                    json!({"type": "response.function_call_arguments.delta",
                    "sequence_number": 3, "item_id": "fc_1", "output_index": 0, "delta": "\"Paris\"}"}),
                ),
                ev(
                    "response.function_call_arguments.done",
                    json!({"type": "response.function_call_arguments.done",
                    "sequence_number": 4, "item_id": "fc_1", "output_index": 0, "arguments": "{\"city\":\"Paris\"}"}),
                ),
                ev(
                    "response.output_item.done",
                    json!({"type": "response.output_item.done", "sequence_number": 5,
                    "output_index": 0, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1",
                    "name": "get_weather", "arguments": "{\"city\":\"Paris\"}", "status": "completed"}}),
                ),
                ev(
                    "response.completed",
                    json!({"type": "response.completed", "sequence_number": 6,
                    "response": {"id": "resp_mock", "object": "response", "status": "completed", "model": model,
                    "output": [{"type": "function_call", "id": "fc_1", "call_id": "call_1",
                    "name": "get_weather", "arguments": "{\"city\":\"Paris\"}", "status": "completed"}],
                    "usage": {"input_tokens": 7, "output_tokens": 3, "total_tokens": 10}}}),
                ),
            ]
        } else {
            vec![
                ev(
                    "response.created",
                    json!({"type": "response.created", "sequence_number": 0,
                    "response": {"id": "resp_mock", "object": "response", "status": "in_progress", "model": model, "output": []}}),
                ),
                ev(
                    "response.output_item.added",
                    json!({"type": "response.output_item.added", "sequence_number": 1,
                    "output_index": 0, "item": {"type": "message", "id": "msg_1", "role": "assistant",
                    "status": "in_progress", "content": []}}),
                ),
                ev(
                    "response.content_part.added",
                    json!({"type": "response.content_part.added", "sequence_number": 1,
                    "item_id": "msg_1", "output_index": 0, "content_index": 0,
                    "part": {"type": "output_text", "text": "", "annotations": []}}),
                ),
                ev(
                    "response.output_text.delta",
                    json!({"type": "response.output_text.delta", "sequence_number": 2,
                    "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": "mock "}),
                ),
                ev(
                    "response.output_text.delta",
                    json!({"type": "response.output_text.delta", "sequence_number": 3,
                    "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": "stream"}),
                ),
                ev(
                    "response.output_text.done",
                    json!({"type": "response.output_text.done", "sequence_number": 4,
                    "item_id": "msg_1", "output_index": 0, "content_index": 0, "text": "mock stream"}),
                ),
                ev(
                    "response.content_part.done",
                    json!({"type": "response.content_part.done", "sequence_number": 4,
                    "item_id": "msg_1", "output_index": 0, "content_index": 0,
                    "part": {"type": "output_text", "text": "mock stream", "annotations": []}}),
                ),
                ev(
                    "response.output_item.done",
                    json!({"type": "response.output_item.done", "sequence_number": 5,
                    "output_index": 0, "item": {"type": "message", "id": "msg_1", "role": "assistant",
                    "status": "completed", "content": [{"type": "output_text", "text": "mock stream", "annotations": []}]}}),
                ),
                ev(
                    "response.completed",
                    json!({"type": "response.completed", "sequence_number": 6,
                    "response": {"id": "resp_mock", "object": "response", "status": "completed", "model": model,
                    "output": [{"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": "mock stream", "annotations": []}]}],
                    "usage": {"input_tokens": 7, "output_tokens": 3, "total_tokens": 10}}}),
                ),
            ]
        };
        return paced_sse(frames);
    }

    let output = if with_tools {
        json!([{"type": "function_call", "id": "fc_1", "call_id": "call_1",
                "name": "get_weather", "arguments": "{\"city\":\"Paris\"}", "status": "completed"}])
    } else {
        json!([{"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": "mock response", "annotations": []}]}])
    };
    axum::Json(json!({
        "id": "resp_mock",
        "object": "response",
        "status": "completed",
        "model": model,
        "output": output,
        "usage": {"input_tokens": 7, "output_tokens": 3, "total_tokens": 10},
        "error": null,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Azure-dialect upstream (deployment-addressed OpenAI)
// ---------------------------------------------------------------------------

async fn azure_chat(
    State(shared): State<Shared>,
    axum::extract::Path(deployment): axum::extract::Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let path = format!(
        "/openai/deployments/{deployment}/chat/completions?{}",
        query.unwrap_or_default()
    );
    let parsed = record(&shared, &path, &headers, &body).await;
    let model = parsed["model"].as_str().unwrap_or("unknown").to_owned();
    if let Some(failure) = scripted_failure(&shared, &model).await {
        return failure;
    }
    if parsed["stream"] == json!(true) {
        return sse_stream(model, parsed.get("tools").is_some());
    }
    axum::Json(json!({
        "id": "chatcmpl-azure-mock",
        "object": "chat.completion",
        "model": model,
        "choices": [{"index": 0,
            "message": {"role": "assistant", "content": "mock response"},
            "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Bedrock-dialect upstream (Converse + event-stream ConverseStream)
// ---------------------------------------------------------------------------

async fn bedrock_converse(
    State(shared): State<Shared>,
    axum::extract::Path(model): axum::extract::Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let parsed = record(
        &shared,
        &format!("/model/{model}/converse"),
        &headers,
        &body,
    )
    .await;
    let with_tools = parsed.get("toolConfig").is_some();

    let content = if with_tools {
        json!([
            {"text": "let me check"},
            {"toolUse": {"toolUseId": "bdrk_1", "name": "get_weather", "input": {"city": "Paris"}}}
        ])
    } else {
        json!([{"text": "mock response"}])
    };
    axum::Json(json!({
        "output": {"message": {"role": "assistant", "content": content}},
        "stopReason": if with_tools { "tool_use" } else { "end_turn" },
        "usage": {"inputTokens": 11, "outputTokens": 5, "totalTokens": 16},
    }))
    .into_response()
}

async fn bedrock_converse_stream(
    State(shared): State<Shared>,
    axum::extract::Path(model): axum::extract::Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let parsed = record(
        &shared,
        &format!("/model/{model}/converse-stream"),
        &headers,
        &body,
    )
    .await;
    let with_tools = parsed.get("toolConfig").is_some();

    let ev =
        |t: &str, v: Value| router_core::eventstream::encode_event(t, v.to_string().as_bytes());
    let mut frames: Vec<Vec<u8>> = vec![ev("messageStart", json!({"role": "assistant"}))];
    if with_tools {
        frames.push(ev(
            "contentBlockStart",
            json!({"contentBlockIndex": 0,
            "start": {"toolUse": {"toolUseId": "bdrk_1", "name": "get_weather"}}}),
        ));
        for fragment in ["{\"city\":", " \"Paris\"}"] {
            frames.push(ev(
                "contentBlockDelta",
                json!({"contentBlockIndex": 0,
                "delta": {"toolUse": {"input": fragment}}}),
            ));
        }
        frames.push(ev("contentBlockStop", json!({"contentBlockIndex": 0})));
        frames.push(ev("messageStop", json!({"stopReason": "tool_use"})));
    } else {
        for text in ["mock ", "stream"] {
            frames.push(ev(
                "contentBlockDelta",
                json!({"contentBlockIndex": 0,
                "delta": {"text": text}}),
            ));
        }
        frames.push(ev("messageStop", json!({"stopReason": "end_turn"})));
    }
    frames.push(ev(
        "metadata",
        json!({"usage": {"inputTokens": 11, "outputTokens": 5, "totalTokens": 16}}),
    ));

    let stream = futures_util::stream::unfold(0usize, move |i| {
        let frames = frames.clone();
        async move {
            if i < frames.len() {
                tokio::time::sleep(Duration::from_millis(30)).await;
                Some((
                    Ok::<_, std::convert::Infallible>(bytes::Bytes::from(frames[i].clone())),
                    i + 1,
                ))
            } else {
                None
            }
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/vnd.amazon.eventstream")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Passthrough test target: records and echoes method + path + query.
async fn record_anything(
    State(shared): State<Shared>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let path = format!("{} {}", method, uri);
    let _ = record(&shared, &path, &headers, &body).await;
    axum::Json(json!({"echo": {"method": method.as_str(), "uri": uri.to_string()}})).into_response()
}

/// Vertex serves Gemini's dialect from project/location paths; the mock
/// reuses the Gemini responder with the full path recorded.
async fn vertex_generate(
    State(shared): State<Shared>,
    axum::extract::Path((project, location, model_action)): axum::extract::Path<(
        String,
        String,
        String,
    )>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let full = format!("vertex/{project}/{location}/{model_action}");
    gemini_generate_inner(shared, model_action, full, headers, body).await
}

/// The ChatGPT Codex backend, reproduced closely enough to catch the two
/// ways a naive client silently gets it wrong.
///
/// It answers only in SSE (there is no non-streaming mode), and its
/// terminal `response.completed` carries an **empty** `output` array — so
/// a tool call is visible only on `response.output_item.done`. A client
/// that reads the terminal event alone gets a well-formed 200 with the
/// tool call missing, which is exactly the bug this mock exists to fail.
async fn codex_responses(
    State(shared): State<Shared>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let parsed = record(&shared, "/backend-api/codex/responses", &headers, &body).await;
    let model = parsed["model"].as_str().unwrap_or("gpt-5.5").to_owned();
    if let Some(failure) = scripted_failure(&shared, &model).await {
        return failure;
    }

    let mut events = vec![format!(
        "event: response.created\ndata: {}\n\n",
        json!({"type": "response.created",
               "response": {"id": "resp_mock", "model": model}})
    )];

    if parsed.get("tools").is_some() {
        events.push(format!(
            "event: response.output_item.done\ndata: {}\n\n",
            json!({"type": "response.output_item.done", "output_index": 1,
                   "item": {"type": "function_call", "call_id": "call_mock",
                            "name": "get_weather", "arguments": "{\"city\":\"SF\"}"}})
        ));
    } else {
        for delta in ["Hello", " from", " Codex"] {
            events.push(format!(
                "event: response.output_text.delta\ndata: {}\n\n",
                json!({"type": "response.output_text.delta", "delta": delta})
            ));
        }
    }

    events.push(format!(
        "event: response.completed\ndata: {}\n\n",
        json!({"type": "response.completed", "response": {
            // EMPTY, as the real backend sends it.
            "output": [],
            "usage": {"input_tokens": 11, "output_tokens": 3, "total_tokens": 14,
                      "input_tokens_details": {"cached_tokens": 4},
                      "output_tokens_details": {"reasoning_tokens": 2}}
        }})
    ));
    events.push("data: [DONE]\n\n".to_owned());

    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        events.concat(),
    )
        .into_response()
}
