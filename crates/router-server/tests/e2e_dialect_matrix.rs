//! The cross-dialect matrix, end to end: every inbound dialect driving
//! every outbound dialect through a live gateway and the dialect-aware
//! mock. Covers sync, streaming, tool calls (parallel + split
//! arguments), tool_choice, multi-turn tool results, json_schema, and
//! same-dialect passthrough fidelity.

use mock_provider::MockProvider;
use router_core::config::{Config, Format};
use router_server::{AppState, build_router};
use serde_json::{Value, json};

struct Gateway {
    url: String,
    mock: MockProvider,
}

/// One gateway fronting all three dialect upstreams (one mock).
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

[aliases]
oai-target = "openai/gpt-4o"
ant-target = "anthropic/claude-x"
gem-target = "gemini/gemini-pro"
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

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

async fn oai_chat(gw: &Gateway, body: Value) -> reqwest::Response {
    client()
        .post(format!("{}/v1/chat/completions", gw.url))
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn ant_msg(gw: &Gateway, body: Value) -> reqwest::Response {
    client()
        .post(format!("{}/anthropic/v1/messages", gw.url))
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn gem_gen(gw: &Gateway, model: &str, stream: bool, body: Value) -> reqwest::Response {
    let action = if stream {
        "streamGenerateContent"
    } else {
        "generateContent"
    };
    client()
        .post(format!("{}/genai/v1beta/models/{model}:{action}", gw.url))
        .json(&body)
        .send()
        .await
        .unwrap()
}

const TOOLS_OAI: &str = r#"[{"type": "function", "function": {"name": "get_weather",
    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}]"#;

// ===========================================================================
// openai inbound -> anthropic target
// ===========================================================================

#[tokio::test]
async fn oai_to_ant_sync_text() {
    let gw = gateway().await;
    let res = oai_chat(
        &gw,
        json!({"model": "ant-target", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-rapid-provider"], "anthropic");
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["choices"][0]["message"]["content"], "mock response");
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["usage"]["total_tokens"], 16);

    // Upstream saw the anthropic wire format with our key header.
    let seen = gw.mock.last_request();
    assert_eq!(seen.path, "/v1/messages");
    assert_eq!(seen.api_key.as_deref(), Some("sk-ant"));
    assert_eq!(seen.body["max_tokens"], 4096, "max_tokens must be injected");
    assert_eq!(seen.body["messages"][0]["content"][0]["text"], "hi");
}

#[tokio::test]
async fn oai_to_ant_sync_parallel_tools() {
    let gw = gateway().await;
    let tools: Value = serde_json::from_str(TOOLS_OAI).unwrap();
    let res = oai_chat(
        &gw,
        json!({
            "model": "ant-target",
            "messages": [{"role": "user", "content": "compare"}],
            "tools": tools, "tool_choice": "auto",
        }),
    )
    .await;
    let body: Value = res.json().await.unwrap();
    let calls = body["choices"][0]["message"]["tool_calls"]
        .as_array()
        .unwrap();
    assert_eq!(
        calls.len(),
        2,
        "parallel calls must both survive translation"
    );
    assert_eq!(calls[0]["id"], "toolu_01");
    assert_eq!(calls[0]["function"]["name"], "get_weather");
    let args: Value =
        serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["city"], "Paris");
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");

    // Upstream got translated tool schemas.
    let seen = gw.mock.last_request();
    assert_eq!(seen.body["tools"][0]["name"], "get_weather");
    assert!(seen.body["tools"][0]["input_schema"].is_object());
    assert_eq!(seen.body["tool_choice"]["type"], "auto");
}

#[tokio::test]
async fn oai_to_ant_streamed_tool_args_reassemble() {
    let gw = gateway().await;
    let tools: Value = serde_json::from_str(TOOLS_OAI).unwrap();
    let res = oai_chat(
        &gw,
        json!({
            "model": "ant-target", "stream": true,
            "messages": [{"role": "user", "content": "weather"}],
            "tools": tools,
        }),
    )
    .await;
    assert_eq!(res.status(), 200);
    let text = res.text().await.unwrap();
    assert!(
        text.trim_end().ends_with("data: [DONE]"),
        "openai stream must end with [DONE]"
    );

    let mut content = String::new();
    let mut args = String::new();
    let mut ids = Vec::new();
    let mut finish = None;
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        let chunk: Value = serde_json::from_str(data).unwrap();
        let delta = &chunk["choices"][0]["delta"];
        if let Some(t) = delta["content"].as_str() {
            content.push_str(t)
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            if let Some(id) = call["id"].as_str() {
                ids.push(id.to_owned())
            }
            if let Some(a) = call["function"]["arguments"].as_str() {
                args.push_str(a)
            }
        }
        if let Some(f) = chunk["choices"][0]["finish_reason"].as_str() {
            finish = Some(f.to_owned())
        }
    }
    assert_eq!(content, "let me check");
    // The mock split these arguments mid-token across three deltas.
    assert_eq!(args, r#"{"city": "Paris"}"#);
    assert_eq!(ids, ["toolu_01"]);
    assert_eq!(finish.as_deref(), Some("tool_calls"));
}

#[tokio::test]
async fn oai_to_ant_tool_choice_variants() {
    let gw = gateway().await;
    let tools: Value = serde_json::from_str(TOOLS_OAI).unwrap();
    for (choice, expected) in [
        (json!("auto"), json!({"type": "auto"})),
        (json!("required"), json!({"type": "any"})),
        (
            json!({"type": "function", "function": {"name": "get_weather"}}),
            json!({"type": "tool", "name": "get_weather"}),
        ),
    ] {
        let res = oai_chat(
            &gw,
            json!({
                "model": "ant-target", "messages": [{"role": "user", "content": "x"}],
                "tools": tools, "tool_choice": choice,
            }),
        )
        .await;
        assert_eq!(res.status(), 200);
        assert_eq!(gw.mock.last_request().body["tool_choice"], expected);
    }
    // `none` must remove the tools entirely.
    let res = oai_chat(
        &gw,
        json!({
            "model": "ant-target", "messages": [{"role": "user", "content": "x"}],
            "tools": tools, "tool_choice": "none",
        }),
    )
    .await;
    assert_eq!(res.status(), 200);
    assert!(gw.mock.last_request().body.get("tools").is_none());
}

#[tokio::test]
async fn oai_to_ant_multi_turn_tool_results() {
    let gw = gateway().await;
    let res = oai_chat(
        &gw,
        json!({
            "model": "ant-target",
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "tool_calls": [
                    {"id": "call_9", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}]},
                {"role": "tool", "tool_call_id": "call_9", "content": "{\"temp\": 21}"}
            ],
        }),
    )
    .await;
    assert_eq!(res.status(), 200);
    let seen = gw.mock.last_request();
    // Tool call id must survive into tool_use / tool_result blocks.
    assert_eq!(seen.body["messages"][1]["content"][0]["type"], "tool_use");
    assert_eq!(seen.body["messages"][1]["content"][0]["id"], "call_9");
    assert_eq!(
        seen.body["messages"][2]["content"][0]["type"],
        "tool_result"
    );
    assert_eq!(
        seen.body["messages"][2]["content"][0]["tool_use_id"],
        "call_9"
    );
}

#[tokio::test]
async fn oai_to_ant_json_schema_emulated() {
    let gw = gateway().await;
    let res = oai_chat(&gw, json!({
        "model": "ant-target",
        "messages": [{"role": "user", "content": "answer"}],
        "response_format": {"type": "json_schema", "json_schema": {
            "name": "out", "schema": {"type": "object", "properties": {"answer": {"type": "string"}, "n": {"type": "integer"}}}}},
    })).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-rapid-emulated"], "json_schema");
    let body: Value = res.json().await.unwrap();
    // The forced tool's arguments came back as content JSON.
    let content: Value =
        serde_json::from_str(body["choices"][0]["message"]["content"].as_str().unwrap()).unwrap();
    assert_eq!(content["answer"], "structured");
    assert_eq!(content["n"], 7);
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert!(body["choices"][0]["message"].get("tool_calls").is_none());
}

#[tokio::test]
async fn oai_to_ant_capability_rejects() {
    let gw = gateway().await;
    let res = oai_chat(&gw, json!({"model": "ant-target", "messages": [], "n": 3})).await;
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["param"], "n");

    let res = oai_chat(
        &gw,
        json!({
            "model": "ant-target",
            "messages": [{"role": "user", "content": [
                {"type": "input_audio", "input_audio": {"data": "aGk=", "format": "wav"}}]}],
        }),
    )
    .await;
    assert_eq!(res.status(), 400);
}

// ===========================================================================
// openai inbound -> gemini target
// ===========================================================================

#[tokio::test]
async fn oai_to_gem_sync_and_tools() {
    let gw = gateway().await;
    let tools: Value = serde_json::from_str(TOOLS_OAI).unwrap();
    let res = oai_chat(
        &gw,
        json!({
            "model": "gem-target",
            "messages": [{"role": "user", "content": "compare"}],
            "tools": tools,
        }),
    )
    .await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-rapid-provider"], "gemini");
    let body: Value = res.json().await.unwrap();
    let calls = body["choices"][0]["message"]["tool_calls"]
        .as_array()
        .unwrap();
    assert_eq!(calls.len(), 2);
    // Synthesized, distinct ids.
    assert_ne!(calls[0]["id"], calls[1]["id"]);
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");

    let seen = gw.mock.last_request();
    assert!(seen.path.contains("gemini-pro:generateContent"));
    assert_eq!(seen.api_key.as_deref(), Some("sk-gem"));
    assert_eq!(
        seen.body["tools"][0]["functionDeclarations"][0]["name"],
        "get_weather"
    );
}

#[tokio::test]
async fn oai_to_gem_streaming() {
    let gw = gateway().await;
    let res = oai_chat(
        &gw,
        json!({
            "model": "gem-target", "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        }),
    )
    .await;
    assert_eq!(res.status(), 200);
    let text = res.text().await.unwrap();
    assert!(text.trim_end().ends_with("data: [DONE]"));
    let mut content = String::new();
    let mut usage = None;
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        let chunk: Value = serde_json::from_str(data).unwrap();
        if let Some(t) = chunk["choices"][0]["delta"]["content"].as_str() {
            content.push_str(t)
        }
        if !chunk["usage"].is_null() {
            usage = Some(chunk["usage"].clone())
        }
    }
    assert_eq!(content, "mock stream");
    assert_eq!(usage.unwrap()["total_tokens"], 16);
}

#[tokio::test]
async fn oai_to_gem_json_schema_native() {
    let gw = gateway().await;
    let res = oai_chat(
        &gw,
        json!({
            "model": "gem-target",
            "messages": [{"role": "user", "content": "answer"}],
            "response_format": {"type": "json_schema", "json_schema": {
                "name": "out", "schema": {"type": "object", "additionalProperties": false,
                    "properties": {"answer": {"type": "string"}}}}},
        }),
    )
    .await;
    assert_eq!(res.status(), 200);
    let seen = gw.mock.last_request();
    assert_eq!(
        seen.body["generationConfig"]["responseMimeType"],
        "application/json"
    );
    // additionalProperties must be stripped for Gemini.
    assert!(
        seen.body["generationConfig"]["responseSchema"]
            .get("additionalProperties")
            .is_none()
    );
    let body: Value = res.json().await.unwrap();
    let content: Value =
        serde_json::from_str(body["choices"][0]["message"]["content"].as_str().unwrap()).unwrap();
    assert_eq!(content["answer"], "structured");
}

// ===========================================================================
// anthropic inbound -> openai target (the Claude Code shape)
// ===========================================================================

#[tokio::test]
async fn ant_to_oai_sync_text() {
    let gw = gateway().await;
    let res = ant_msg(
        &gw,
        json!({
            "model": "oai-target", "max_tokens": 100,
            "system": "be brief",
            "messages": [{"role": "user", "content": "hi"}],
        }),
    )
    .await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-rapid-provider"], "openai");
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["role"], "assistant");
    assert_eq!(body["content"][0]["type"], "text");
    assert_eq!(body["content"][0]["text"], "mock response");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["usage"]["input_tokens"], 7);

    let seen = gw.mock.last_request();
    assert_eq!(seen.path, "/chat/completions");
    assert_eq!(seen.authorization.as_deref(), Some("Bearer sk-oai"));
    assert_eq!(seen.body["messages"][0]["role"], "system");
    assert_eq!(seen.body["messages"][0]["content"], "be brief");
    assert_eq!(seen.body["max_tokens"], 100);
}

#[tokio::test]
async fn ant_to_oai_streamed_tool_call() {
    let gw = gateway().await;
    let res = ant_msg(&gw, json!({
        "model": "oai-target", "max_tokens": 100, "stream": true,
        "messages": [{"role": "user", "content": "weather"}],
        "tools": [{"name": "get_weather", "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}}],
    })).await;
    assert_eq!(res.status(), 200);
    let text = res.text().await.unwrap();

    // Parse the anthropic event stream the gateway produced.
    let mut events: Vec<(String, Value)> = Vec::new();
    let mut current_event = String::new();
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("event: ") {
            current_event = name.to_owned();
        } else if let Some(data) = line.strip_prefix("data: ") {
            events.push((current_event.clone(), serde_json::from_str(data).unwrap()));
        }
    }
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names.first().copied(), Some("message_start"));
    assert!(names.contains(&"content_block_start"));
    assert!(names.contains(&"content_block_delta"));
    assert_eq!(names.last().copied(), Some("message_stop"));

    // The tool block reassembles: name in start, args across deltas.
    let tool_start = events
        .iter()
        .find(|(n, d)| n == "content_block_start" && d["content_block"]["type"] == "tool_use")
        .unwrap();
    assert_eq!(tool_start.1["content_block"]["name"], "get_weather");
    let args: String = events
        .iter()
        .filter(|(n, d)| n == "content_block_delta" && d["delta"]["type"] == "input_json_delta")
        .filter_map(|(_, d)| d["delta"]["partial_json"].as_str())
        .collect();
    assert_eq!(args, r#"{"city":"Paris"}"#);
    let stop = events.iter().find(|(n, _)| n == "message_delta").unwrap();
    assert_eq!(stop.1["delta"]["stop_reason"], "tool_use");

    // Upstream got OpenAI-shaped tools.
    let seen = gw.mock.last_request();
    assert_eq!(seen.body["tools"][0]["type"], "function");
    assert_eq!(seen.body["tools"][0]["function"]["name"], "get_weather");
}

#[tokio::test]
async fn ant_to_oai_multi_turn_tool_results() {
    let gw = gateway().await;
    let res = ant_msg(&gw, json!({
        "model": "oai-target", "max_tokens": 100,
        "messages": [
            {"role": "user", "content": "weather?"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_9", "name": "get_weather", "input": {"city": "Paris"}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_9", "content": "21C"}]}
        ],
    })).await;
    assert_eq!(res.status(), 200);
    let seen = gw.mock.last_request();
    let messages = seen.body["messages"].as_array().unwrap();
    let assistant = messages.iter().find(|m| m["role"] == "assistant").unwrap();
    assert_eq!(assistant["tool_calls"][0]["id"], "toolu_9");
    assert_eq!(
        assistant["tool_calls"][0]["function"]["name"],
        "get_weather"
    );
    let tool = messages.iter().find(|m| m["role"] == "tool").unwrap();
    assert_eq!(tool["tool_call_id"], "toolu_9");
    assert_eq!(tool["content"], "21C");
}

// ===========================================================================
// anthropic inbound -> gemini target
// ===========================================================================

#[tokio::test]
async fn ant_to_gem_sync_tools() {
    let gw = gateway().await;
    let res = ant_msg(
        &gw,
        json!({
            "model": "gem-target", "max_tokens": 100,
            "messages": [{"role": "user", "content": "compare"}],
            "tools": [{"name": "get_weather", "input_schema": {"type": "object"}}],
        }),
    )
    .await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-rapid-provider"], "gemini");
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["type"], "message");
    let tool_uses: Vec<&Value> = body["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["type"] == "tool_use")
        .collect();
    assert_eq!(
        tool_uses.len(),
        2,
        "parallel gemini calls -> parallel tool_use blocks"
    );
    assert_eq!(tool_uses[0]["name"], "get_weather");
    assert_eq!(tool_uses[0]["input"]["city"], "Paris");
    assert_eq!(body["stop_reason"], "tool_use");

    let seen = gw.mock.last_request();
    assert!(seen.path.contains("gemini-pro:generateContent"));
    assert_eq!(
        seen.body["tools"][0]["functionDeclarations"][0]["name"],
        "get_weather"
    );
}

// ===========================================================================
// gemini inbound -> openai target
// ===========================================================================

#[tokio::test]
async fn gem_to_oai_sync_text() {
    let gw = gateway().await;
    let res = gem_gen(
        &gw,
        "oai-target",
        false,
        json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": {"temperature": 0.3, "maxOutputTokens": 50},
        }),
    )
    .await;
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["candidates"][0]["content"]["role"], "model");
    assert_eq!(
        body["candidates"][0]["content"]["parts"][0]["text"],
        "mock response"
    );
    assert_eq!(body["candidates"][0]["finishReason"], "STOP");
    assert_eq!(body["usageMetadata"]["totalTokenCount"], 10);

    let seen = gw.mock.last_request();
    assert_eq!(seen.path, "/chat/completions");
    assert_eq!(seen.body["temperature"], 0.3);
    assert_eq!(seen.body["max_tokens"], 50);
}

#[tokio::test]
async fn gem_to_oai_streamed_tool_call() {
    let gw = gateway().await;
    let res = gem_gen(&gw, "oai-target", true, json!({
        "contents": [{"role": "user", "parts": [{"text": "weather"}]}],
        "tools": [{"functionDeclarations": [{"name": "get_weather", "parameters": {"type": "object"}}]}],
    })).await;
    assert_eq!(res.status(), 200);
    let text = res.text().await.unwrap();
    let mut call = None;
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let chunk: Value = serde_json::from_str(data).unwrap();
        for part in chunk["candidates"][0]["content"]["parts"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if part.get("functionCall").is_some() {
                call = Some(part["functionCall"].clone());
            }
        }
    }
    // OpenAI streams the args in fragments; the gateway reassembles them
    // into one whole functionCall for the Gemini client.
    let call = call.expect("functionCall must appear in the gemini stream");
    assert_eq!(call["name"], "get_weather");
    assert_eq!(call["args"]["city"], "Paris");
}

#[tokio::test]
async fn gem_to_oai_multi_turn_function_response() {
    let gw = gateway().await;
    let res = gem_gen(&gw, "oai-target", false, json!({
        "contents": [
            {"role": "user", "parts": [{"text": "weather?"}]},
            {"role": "model", "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}}]},
            {"role": "user", "parts": [{"functionResponse": {"name": "get_weather", "response": {"temp": 21}}}]}
        ],
    })).await;
    assert_eq!(res.status(), 200);
    let seen = gw.mock.last_request();
    let messages = seen.body["messages"].as_array().unwrap();
    let assistant = messages.iter().find(|m| m["role"] == "assistant").unwrap();
    let tool = messages.iter().find(|m| m["role"] == "tool").unwrap();
    // Synthesized id links the call to its result.
    assert_eq!(assistant["tool_calls"][0]["id"], tool["tool_call_id"]);
}

// ===========================================================================
// same-dialect passthrough fidelity
// ===========================================================================

#[tokio::test]
async fn ant_to_ant_passthrough_preserves_unknown_fields() {
    let gw = gateway().await;
    let res = ant_msg(
        &gw,
        json!({
            "model": "ant-target", "max_tokens": 100,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}]}],
            "thinking": {"type": "enabled", "budget_tokens": 1024},
        }),
    )
    .await;
    assert_eq!(res.status(), 200);
    let seen = gw.mock.last_request();
    assert_eq!(seen.path, "/v1/messages");
    // Anthropic-specific extensions must survive same-dialect transit.
    assert_eq!(
        seen.body["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(seen.body["thinking"]["budget_tokens"], 1024);
    assert_eq!(
        seen.body["model"], "claude-x",
        "model must still be rewritten"
    );
}

#[tokio::test]
async fn gem_to_gem_passthrough_body_untouched() {
    let gw = gateway().await;
    let res = gem_gen(
        &gw,
        "gem-target",
        false,
        json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "safetySettings": [{"category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE"}],
        }),
    )
    .await;
    assert_eq!(res.status(), 200);
    let seen = gw.mock.last_request();
    assert!(seen.path.contains("gemini-pro:generateContent"));
    assert_eq!(seen.body["safetySettings"][0]["threshold"], "BLOCK_NONE");
}

// ===========================================================================
// cross-dialect error mapping
// ===========================================================================

#[tokio::test]
async fn upstream_error_rendered_in_inbound_dialect() {
    let gw = gateway().await;
    // OpenAI inbound, anthropic target failing: OpenAI error shape out.
    let res = oai_chat(&gw, json!({"model": "anthropic/err-500", "messages": []})).await;
    assert_eq!(res.status(), 500);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["type"], "api_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("upstream exploded")
    );

    // Anthropic inbound, openai target failing: anthropic error shape out.
    let res = ant_msg(
        &gw,
        json!({"model": "openai/err-500", "max_tokens": 10, "messages": []}),
    )
    .await;
    assert_eq!(res.status(), 500);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("upstream exploded")
    );

    // Gemini inbound, unknown model: google error shape out.
    let res = gem_gen(&gw, "nope-model", false, json!({"contents": []})).await;
    assert_eq!(res.status(), 404);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["status"], "NOT_FOUND");
}
