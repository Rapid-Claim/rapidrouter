//! End-to-end: a real gateway on a real port proxying to the in-process
//! mock provider. Covers routing, auth injection, splice correctness,
//! streaming, tool-call passthrough, and every error path a client can hit.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use mock_provider::MockProvider;
use router_core::config::{Config, Format};
use router_server::{AppState, build_router};
use serde_json::{Value, json};

struct Gateway {
    url: String,
    #[allow(dead_code)]
    state: Arc<AppState>,
    mock: MockProvider,
}

async fn gateway_with(config_toml: impl Fn(&MockProvider) -> String) -> Gateway {
    let mock = MockProvider::spawn().await;
    let config =
        Config::from_str_with_env(&config_toml(&mock), Format::Toml, &|_: &str| None).unwrap();
    let state = AppState::new(config);
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let serve_state = state.clone();
    tokio::spawn(async move {
        router_server::serve(listener, serve_state, app, std::future::pending())
            .await
            .unwrap()
    });
    Gateway { url, state, mock }
}

async fn gateway() -> Gateway {
    gateway_with(|mock| {
        format!(
            r#"
[providers.openai]
base_url = "{base}"
keys = [
  {{ name = "catalog", value = "sk-mock-catalog", models = ["gpt-4o-mini"] }},
  {{ name = "wide", value = "sk-mock-wide" }},
]

[providers.groq]
base_url = "{base}"
keys = [{{ name = "main", value = "gsk-mock" }}]

[aliases]
fast = "groq/llama-3.3-70b-versatile"
"#,
            base = mock.base_url()
        )
    })
    .await
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

async fn chat(gw: &Gateway, body: Value) -> reqwest::Response {
    client()
        .post(format!("{}/v1/chat/completions", gw.url))
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn chat_completion_round_trip() {
    let gw = gateway().await;
    let res = chat(
        &gw,
        json!({"model": "openai/gpt-4o", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;

    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-rapid-provider"], "openai");
    assert_eq!(res.headers()["x-rapid-model"], "gpt-4o");
    assert!(res.headers().contains_key("x-request-id"));
    let overhead: u64 = res.headers()["x-rapid-overhead-us"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        overhead < 500_000,
        "overhead header implausible: {overhead}us"
    );

    let body: Value = res.json().await.unwrap();
    assert_eq!(body["model"], "gpt-4o");
    assert_eq!(body["choices"][0]["message"]["content"], "mock response");

    // The upstream saw the stripped model, the injected key, and the
    // untouched messages array.
    let seen = gw.mock.last_request();
    assert_eq!(seen.body["model"], "gpt-4o");
    assert_eq!(seen.body["messages"][0]["content"], "hi");
    assert_eq!(seen.authorization.as_deref(), Some("Bearer sk-mock-wide"));
}

#[tokio::test]
async fn bare_model_resolves_through_catalog() {
    let gw = gateway().await;
    let res = chat(&gw, json!({"model": "gpt-4o-mini", "messages": []})).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-rapid-provider"], "openai");
    assert_eq!(gw.mock.last_request().body["model"], "gpt-4o-mini");
}

#[tokio::test]
async fn alias_resolves_and_strips() {
    let gw = gateway().await;
    let res = chat(&gw, json!({"model": "fast", "messages": []})).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-rapid-provider"], "groq");
    let seen = gw.mock.last_request();
    assert_eq!(seen.body["model"], "llama-3.3-70b-versatile");
    assert_eq!(seen.authorization.as_deref(), Some("Bearer gsk-mock"));
}

#[tokio::test]
async fn unknown_model_is_openai_shaped_404() {
    let gw = gateway().await;
    let res = chat(&gw, json!({"model": "nope", "messages": []})).await;
    assert_eq!(res.status(), 404);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "not_found");
    assert_eq!(body["error"]["param"], "model");
}

#[tokio::test]
async fn malformed_body_is_400() {
    let gw = gateway().await;
    let res = client()
        .post(format!("{}/v1/chat/completions", gw.url))
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn streaming_passes_through_incrementally() {
    let gw = gateway().await;
    let res = chat(
        &gw,
        json!({"model": "openai/gpt-4o", "messages": [], "stream": true}),
    )
    .await;
    assert_eq!(res.status(), 200);
    assert!(
        res.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );

    let mut stream = res.bytes_stream();
    let mut arrivals: Vec<(Instant, String)> = Vec::new();
    while let Some(chunk) = stream.next().await {
        arrivals.push((
            Instant::now(),
            String::from_utf8(chunk.unwrap().to_vec()).unwrap(),
        ));
    }
    let text: String = arrivals.iter().map(|(_, s)| s.as_str()).collect();

    assert!(text.contains(r#""content":"mock ""#));
    assert!(text.contains(r#""content":"stream""#));
    assert!(text.trim_end().ends_with("data: [DONE]"));

    // The mock spaces its 5 frames 100ms apart; if the gateway buffered
    // the stream, everything would arrive in one burst.
    let spread = arrivals.last().unwrap().0 - arrivals.first().unwrap().0;
    assert!(
        spread >= Duration::from_millis(250),
        "stream arrived in {spread:?}; the gateway is buffering"
    );
    assert!(
        arrivals.len() >= 3,
        "stream collapsed into {} reads",
        arrivals.len()
    );
}

#[tokio::test]
async fn tool_call_stream_is_byte_faithful() {
    let gw = gateway().await;
    let request = json!({
        "model": "openai/gpt-4o",
        "messages": [{"role": "user", "content": "weather in paris?"}],
        "stream": true,
        "tools": [{
            "type": "function",
            "function": {
                "name": "get_weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }
        }],
        "tool_choice": "auto"
    });
    let res = chat(&gw, request.clone()).await;
    let text = res.text().await.unwrap();

    // Tool definitions reached the provider untouched.
    let seen = gw.mock.last_request();
    assert_eq!(seen.body["tools"], request["tools"]);
    assert_eq!(seen.body["tool_choice"], "auto");

    // Streamed tool-call argument deltas reassemble exactly.
    let mut args = String::new();
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        let chunk: Value = serde_json::from_str(data).unwrap();
        if let Some(fragment) =
            chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str()
        {
            args.push_str(fragment);
        }
    }
    assert_eq!(args, r#"{"city":"Paris"}"#);
    let parsed: Value = serde_json::from_str(&args).unwrap();
    assert_eq!(parsed["city"], "Paris");
}

#[tokio::test]
async fn upstream_429_passes_through_with_retry_after() {
    let gw = gateway().await;
    let res = chat(&gw, json!({"model": "openai/err-429", "messages": []})).await;
    assert_eq!(res.status(), 429);
    assert_eq!(res.headers()["retry-after"], "7");
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");
}

#[tokio::test]
async fn upstream_500_passes_through_verbatim() {
    let gw = gateway().await;
    let res = chat(&gw, json!({"model": "openai/err-500", "messages": []})).await;
    assert_eq!(res.status(), 500);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["message"], "upstream exploded");
}

#[tokio::test]
async fn dead_upstream_maps_to_502() {
    let gw = gateway_with(|_| {
        r#"
[providers.internal]
type = "openai_compat"
base_url = "http://127.0.0.1:9"
keys = [{ name = "k", value = "sk" }]
"#
        .to_owned()
    })
    .await;
    let res = chat(&gw, json!({"model": "internal/m", "messages": []})).await;
    assert_eq!(res.status(), 502);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], "upstream_error");
    assert_eq!(body["error"]["metadata"]["provider"], "internal");
}

#[tokio::test]
async fn slow_upstream_times_out_as_504() {
    let gw = gateway_with(|mock| {
        format!(
            r#"
[providers.openai]
base_url = "{}"
timeout_secs = 1
keys = [{{ name = "k", value = "sk" }}]
"#,
            mock.base_url()
        )
    })
    .await;
    let res = chat(&gw, json!({"model": "openai/slow", "messages": []})).await;
    assert_eq!(res.status(), 504);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], "timeout");
}

#[tokio::test]
async fn gateway_auth_enforced_when_configured() {
    let gw = gateway_with(|mock| {
        format!(
            r#"
[server]
auth_keys = ["ck-secret"]

[providers.openai]
base_url = "{}"
keys = [{{ name = "k", value = "sk" }}]
"#,
            mock.base_url()
        )
    })
    .await;

    let body = json!({"model": "openai/gpt-4o", "messages": []});
    let res = chat(&gw, body.clone()).await;
    assert_eq!(res.status(), 401);

    let res = client()
        .post(format!("{}/v1/chat/completions", gw.url))
        .bearer_auth("wrong")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);

    let res = client()
        .post(format!("{}/v1/chat/completions", gw.url))
        .bearer_auth("ck-secret")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    // Health stays open without gateway auth.
    let res = client()
        .get(format!("{}/health", gw.url))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn both_weighted_keys_get_traffic() {
    let gw = gateway_with(|mock| {
        format!(
            r#"
[providers.openai]
base_url = "{}"
keys = [
  {{ name = "a", value = "sk-a", weight = 0.5 }},
  {{ name = "b", value = "sk-b", weight = 0.5 }},
]
"#,
            mock.base_url()
        )
    })
    .await;

    for _ in 0..40 {
        let res = chat(&gw, json!({"model": "openai/gpt-4o", "messages": []})).await;
        assert_eq!(res.status(), 200);
    }
    let auths: std::collections::BTreeSet<String> = gw
        .mock
        .requests()
        .into_iter()
        .filter_map(|r| r.authorization)
        .collect();
    assert_eq!(auths.len(), 2, "expected both keys used, saw: {auths:?}");
}

#[tokio::test]
async fn embeddings_and_completions_relay() {
    let gw = gateway().await;

    let res = client()
        .post(format!("{}/v1/embeddings", gw.url))
        .json(&json!({"model": "openai/gpt-4o", "input": "hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["data"][0]["object"], "embedding");
    assert_eq!(gw.mock.last_request().path, "/embeddings");

    let res = client()
        .post(format!("{}/v1/completions", gw.url))
        .json(&json!({"model": "openai/gpt-4o", "prompt": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(gw.mock.last_request().path, "/completions");
}

#[tokio::test]
async fn models_endpoint_lists_catalog_and_aliases() {
    let gw = gateway().await;
    let res = client()
        .get(format!("{}/v1/models", gw.url))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"openai/gpt-4o-mini"));
    assert!(ids.contains(&"fast"));
    assert!(
        !ids.iter().any(|id| id.contains("err-")),
        "error stubs must not be listed"
    );
}

#[tokio::test]
async fn oversized_body_is_413() {
    let gw = gateway_with(|mock| {
        format!(
            r#"
[server]
max_body_size_mb = 1

[providers.openai]
base_url = "{}"
keys = [{{ name = "k", value = "sk" }}]
"#,
            mock.base_url()
        )
    })
    .await;
    let huge = "x".repeat(2 * 1024 * 1024);
    let sent = client()
        .post(format!("{}/v1/chat/completions", gw.url))
        .json(&json!({"model": "openai/gpt-4o", "messages": [{"role": "user", "content": huge}]}))
        .send()
        .await;
    // The gateway refuses the oversized body outright. Whether the client
    // reads the 413 or has its in-flight write reset depends on how much
    // of the body reached socket buffers before the refusal — both are
    // the same refusal, and only "upstream accepted it" is a failure.
    match sent {
        Ok(res) => assert_eq!(res.status(), 413),
        Err(err) => assert!(
            err.is_request() || err.is_body(),
            "expected a write-side refusal, got: {err}"
        ),
    }
}

#[tokio::test]
async fn hot_reload_swaps_routing_live() {
    let gw = gateway().await;
    assert_eq!(
        chat(&gw, json!({"model": "fast", "messages": []}))
            .await
            .status(),
        200
    );

    // Repoint the alias at openai via a config apply, as SIGHUP would.
    let new_config = Config::from_str_with_env(
        &format!(
            r#"
[providers.openai]
base_url = "{base}"
keys = [{{ name = "primary", value = "sk-mock-primary" }}]

[aliases]
fast = "openai/gpt-4o"
"#,
            base = gw.mock.base_url()
        ),
        Format::Toml,
        &|_: &str| None,
    )
    .unwrap();
    gw.state.apply_config(new_config);

    let res = chat(&gw, json!({"model": "fast", "messages": []})).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-rapid-provider"], "openai");
    assert_eq!(gw.mock.last_request().body["model"], "gpt-4o");
}
