//! An in-process OpenAI-shaped provider for integration tests: records
//! every request it receives and scripts its behavior off the request's
//! `model` field, so tests drive error paths without real network flakiness.
//!
//! Behavior models:
//! - `err-500`  -> 500 with a provider-style error body
//! - `err-429`  -> 429 with `retry-after: 7`
//! - `slow`     -> 2s delay before responding
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
            .route("/v1/messages", post(anthropic_messages))
            .route("/v1beta/models/{model_action}", post(gemini_generate))
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

async fn record(shared: &Shared, path: &str, headers: &HeaderMap, body: &[u8]) -> Value {
    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    shared.requests.lock().unwrap().push(RecordedRequest {
        path: path.to_owned(),
        authorization: headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        api_key: headers
            .get("x-api-key")
            .or_else(|| headers.get("x-goog-api-key"))
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        body: parsed.clone(),
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

    axum::Json(json!({
        "id": "msg_mock",
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {"input_tokens": 11, "output_tokens": 5}
    }))
    .into_response()
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
    let (model, action) = model_action
        .split_once(':')
        .unwrap_or((model_action.as_str(), ""));
    let model = model.to_owned();
    let parsed = record(
        &shared,
        &format!("/v1beta/models/{model_action}"),
        &headers,
        &body,
    )
    .await;
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
