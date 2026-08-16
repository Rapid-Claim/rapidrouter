//! The Responses API end to end: native relay to OpenAI-dialect targets,
//! stateless translation to Anthropic/Gemini targets, streaming event
//! sequences, tool calling, and the statefulness gate.

use mock_provider::MockProvider;
use router_core::config::{Config, Format};
use router_server::{AppState, build_router};
use serde_json::{Value, json};

struct Gateway {
    url: String,
    mock: MockProvider,
}

async fn gateway() -> Gateway {
    let mock = MockProvider::spawn().await;
    let config = Config::from_str_with_env(
        &format!(
            r#"
[providers.openai]
base_url = "{base}"
keys = [{{ name = "k", value = "sk-oai" }}]

[providers.anthropic]
base_url = "{base}"
keys = [{{ name = "k", value = "sk-ant" }}]

[providers.gemini]
base_url = "{base}"
keys = [{{ name = "k", value = "sk-gem" }}]
"#,
            base = mock.base_url()
        ),
        Format::Toml,
        &|_: &str| None,
    )
    .unwrap();
    let state = AppState::new(config);
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        router_server::serve(listener, state, app, std::future::pending())
            .await
            .unwrap()
    });
    Gateway { url, mock }
}

async fn responses(gw: &Gateway, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/responses", gw.url))
        .json(&body)
        .send()
        .await
        .unwrap()
}

fn parse_events(text: &str) -> Vec<(String, Value)> {
    let mut events = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("event: ") {
            current = name.to_owned();
        } else if let Some(data) = line.strip_prefix("data: ") {
            events.push((current.clone(), serde_json::from_str(data).unwrap()));
        }
    }
    events
}

// ===========================================================================
// Relay: OpenAI-dialect target serves the full surface natively
// ===========================================================================

#[tokio::test]
async fn relay_sync_with_state_allowed() {
    let gw = gateway().await;
    let res = responses(
        &gw,
        json!({
            "model": "openai/gpt-4o", "input": "hi",
            "store": true,
            "instructions": "be brief",
        }),
    )
    .await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-rapid-provider"], "openai");
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["object"], "response");
    assert_eq!(body["output"][0]["content"][0]["text"], "mock response");

    // Relay is verbatim: store/instructions reach the provider, model is
    // rewritten.
    let seen = gw.mock.last_request();
    assert_eq!(seen.path, "/responses");
    assert_eq!(seen.body["model"], "gpt-4o");
    assert_eq!(seen.body["store"], true);
    assert_eq!(seen.body["instructions"], "be brief");
    assert_eq!(seen.authorization.as_deref(), Some("Bearer sk-oai"));
}

#[tokio::test]
async fn relay_streaming_events_pass_through() {
    let gw = gateway().await;
    let res = responses(
        &gw,
        json!({"model": "openai/gpt-4o", "input": "hi", "stream": true}),
    )
    .await;
    assert_eq!(res.status(), 200);
    let events = parse_events(&res.text().await.unwrap());
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names.first().copied(), Some("response.created"));
    assert!(names.contains(&"response.output_text.delta"));
    assert_eq!(names.last().copied(), Some("response.completed"));
    let text: String = events
        .iter()
        .filter(|(n, _)| n == "response.output_text.delta")
        .filter_map(|(_, d)| d["delta"].as_str())
        .collect();
    assert_eq!(text, "mock stream");
}

#[tokio::test]
async fn relay_function_call_round_trip() {
    let gw = gateway().await;
    // Turn 1: model calls a tool.
    let res = responses(&gw, json!({
        "model": "openai/gpt-4o",
        "input": "weather in paris?",
        "tools": [{"type": "function", "name": "get_weather",
                   "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}],
    })).await;
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["output"][0]["type"], "function_call");
    let call_id = body["output"][0]["call_id"].as_str().unwrap().to_owned();

    // Turn 2: send the function output back.
    let res = responses(
        &gw,
        json!({
            "model": "openai/gpt-4o",
            "input": [
                {"type": "function_call", "call_id": call_id, "name": "get_weather",
                 "arguments": "{\"city\":\"Paris\"}"},
                {"type": "function_call_output", "call_id": call_id, "output": "21C"}
            ],
        }),
    )
    .await;
    assert_eq!(res.status(), 200);
    let seen = gw.mock.last_request();
    assert_eq!(seen.body["input"][1]["output"], "21C");
}

// ===========================================================================
// Translate: Anthropic/Gemini targets get the stateless core
// ===========================================================================

#[tokio::test]
async fn translate_sync_text_to_anthropic() {
    let gw = gateway().await;
    let res = responses(
        &gw,
        json!({
            "model": "anthropic/claude-x",
            "input": "hi",
            "instructions": "be brief",
            "max_output_tokens": 99,
        }),
    )
    .await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-rapid-provider"], "anthropic");
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["object"], "response");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["output"][0]["type"], "message");
    assert_eq!(body["output"][0]["content"][0]["type"], "output_text");
    assert_eq!(body["output"][0]["content"][0]["text"], "mock response");
    assert_eq!(body["usage"]["input_tokens"], 11);

    let seen = gw.mock.last_request();
    assert_eq!(seen.path, "/v1/messages");
    assert_eq!(seen.body["system"], "be brief");
    assert_eq!(seen.body["max_tokens"], 99);
}

#[tokio::test]
async fn translate_tools_to_anthropic() {
    let gw = gateway().await;
    let res = responses(&gw, json!({
        "model": "anthropic/claude-x",
        "input": [
            {"type": "message", "role": "user", "content": "compare weather"},
            {"type": "function_call", "call_id": "call_h", "name": "get_weather",
             "arguments": "{\"city\":\"Oslo\"}"},
            {"type": "function_call_output", "call_id": "call_h", "output": "3C"}
        ],
        "tools": [{"type": "function", "name": "get_weather",
                   "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}],
    })).await;
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    // Mock's anthropic endpoint answers with two parallel tool_use blocks.
    let calls: Vec<&Value> = body["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["type"] == "function_call")
        .collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["name"], "get_weather");
    let args: Value = serde_json::from_str(calls[0]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["city"], "Paris");

    // History translated: prior call + result became tool_use/tool_result.
    let seen = gw.mock.last_request();
    assert_eq!(seen.body["tools"][0]["name"], "get_weather");
    assert_eq!(seen.body["messages"][1]["content"][0]["type"], "tool_use");
    assert_eq!(seen.body["messages"][1]["content"][0]["id"], "call_h");
    assert_eq!(
        seen.body["messages"][2]["content"][0]["type"],
        "tool_result"
    );
}

#[tokio::test]
async fn translate_streaming_to_anthropic_produces_responses_events() {
    let gw = gateway().await;
    let res = responses(
        &gw,
        json!({
            "model": "anthropic/claude-x", "input": "hi", "stream": true,
        }),
    )
    .await;
    assert_eq!(res.status(), 200);
    let events = parse_events(&res.text().await.unwrap());
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names.first().copied(), Some("response.created"));
    assert!(names.contains(&"response.output_item.added"));
    assert!(names.contains(&"response.output_text.done"));
    assert_eq!(names.last().copied(), Some("response.completed"));

    let text: String = events
        .iter()
        .filter(|(n, _)| n == "response.output_text.delta")
        .filter_map(|(_, d)| d["delta"].as_str())
        .collect();
    assert_eq!(text, "mock stream");

    // The completed event carries assembled output and usage.
    let (_, completed) = events.last().unwrap();
    assert_eq!(completed["response"]["status"], "completed");
    assert_eq!(
        completed["response"]["output"][0]["content"][0]["text"],
        "mock stream"
    );
    assert_eq!(completed["response"]["usage"]["input_tokens"], 11);

    // Every event is sequence-numbered monotonically.
    let sequences: Vec<u64> = events
        .iter()
        .filter_map(|(_, d)| d["sequence_number"].as_u64())
        .collect();
    assert!(sequences.windows(2).all(|w| w[0] < w[1]));
}

#[tokio::test]
async fn translate_streamed_tool_call_events() {
    let gw = gateway().await;
    let res = responses(
        &gw,
        json!({
            "model": "anthropic/claude-x", "input": "weather", "stream": true,
            "tools": [{"type": "function", "name": "get_weather",
                       "parameters": {"type": "object"}}],
        }),
    )
    .await;
    let events = parse_events(&res.text().await.unwrap());
    let added = events
        .iter()
        .find(|(n, d)| n == "response.output_item.added" && d["item"]["type"] == "function_call")
        .expect("function_call item must be announced");
    assert_eq!(added.1["item"]["name"], "get_weather");
    let args: String = events
        .iter()
        .filter(|(n, _)| n == "response.function_call_arguments.delta")
        .filter_map(|(_, d)| d["delta"].as_str())
        .collect();
    assert_eq!(args, r#"{"city": "Paris"}"#);
    let done = events
        .iter()
        .find(|(n, _)| n == "response.function_call_arguments.done")
        .expect("arguments.done must close the call");
    assert_eq!(done.1["arguments"], r#"{"city": "Paris"}"#);
}

#[tokio::test]
async fn translate_to_gemini_works() {
    let gw = gateway().await;
    let res = responses(&gw, json!({"model": "gemini/gemini-pro", "input": "hi"})).await;
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["output"][0]["content"][0]["text"], "mock response");
    assert!(
        gw.mock
            .last_request()
            .path
            .contains("gemini-pro:generateContent")
    );
}

// ===========================================================================
// The statefulness gate
// ===========================================================================

#[tokio::test]
async fn state_rejected_on_translated_targets() {
    let gw = gateway().await;
    for body in [
        json!({"model": "anthropic/claude-x", "input": "hi", "store": true}),
        json!({"model": "anthropic/claude-x", "input": "hi", "previous_response_id": "resp_123"}),
    ] {
        let res = responses(&gw, body).await;
        assert_eq!(res.status(), 400);
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["error"]["param"], "store");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("stateless")
        );
    }
}

#[tokio::test]
async fn builtin_tools_rejected_on_translated_targets() {
    let gw = gateway().await;
    let res = responses(
        &gw,
        json!({
            "model": "anthropic/claude-x", "input": "search something",
            "tools": [{"type": "web_search"}],
        }),
    )
    .await;
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("web_search")
    );
}

#[tokio::test]
async fn image_and_file_inputs_translate() {
    let gw = gateway().await;
    let res = responses(
        &gw,
        json!({
            "model": "anthropic/claude-x",
            "input": [{"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "what's this?"},
                {"type": "input_image", "image_url": "data:image/png;base64,aW1n"},
                {"type": "input_file", "filename": "doc.pdf",
                 "file_data": "data:application/pdf;base64,cGRm"}
            ]}],
        }),
    )
    .await;
    assert_eq!(res.status(), 200);
    let seen = gw.mock.last_request();
    let blocks = seen.body["messages"][0]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[1]["type"], "image");
    assert_eq!(blocks[1]["source"]["data"], "aW1n");
    assert_eq!(blocks[2]["type"], "document");
    assert_eq!(blocks[2]["source"]["data"], "cGRm");
}
