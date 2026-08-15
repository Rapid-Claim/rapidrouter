//! Live-provider validation: the conformance shapes against REAL provider
//! APIs, through a gateway configured from environment keys.
//!
//! Ignored by default — CI runs it with `--ignored` when provider secrets
//! are configured:
//!
//! ```sh
//! OPENAI_API_KEY=… ANTHROPIC_API_KEY=… GEMINI_API_KEY=… \
//!   cargo test -p router-server --test live_validation -- --ignored --nocapture
//! ```
//!
//! Model choices are overridable (`LIVE_OPENAI_MODEL`,
//! `LIVE_ANTHROPIC_MODEL`, `LIVE_GEMINI_MODEL`). Assertions are semantic
//! (the tool was called with parseable arguments; the stream terminated
//! properly) rather than exact-output, so model nondeterminism cannot
//! flake the suite.

use std::sync::Arc;

use router_core::config::Config;
use router_server::{AppState, build_router};
use serde_json::{Value, json};

struct Live {
    url: String,
    providers: Vec<(String, String)>, // (provider, model)
}

async fn live_gateway() -> Option<Live> {
    let env = |var: &str| std::env::var(var).ok();
    let config = Config::discover_from_env(&env)?;
    let mut providers = Vec::new();
    for (name, default_model) in [
        ("openai", "gpt-4o-mini"),
        ("anthropic", "claude-3-5-haiku-latest"),
        ("gemini", "gemini-2.0-flash"),
    ] {
        if config.providers.contains_key(name) {
            let model = std::env::var(format!("LIVE_{}_MODEL", name.to_uppercase()))
                .unwrap_or_else(|_| default_model.to_owned());
            providers.push((name.to_owned(), model));
        }
    }

    let state = AppState::new(config);
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        router_server::serve(listener, state, app, std::future::pending())
            .await
            .unwrap()
    });
    Some(Live { url, providers })
}

async fn chat(live: &Live, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", live.url))
        .json(&body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .unwrap()
}

macro_rules! require_live {
    () => {
        match live_gateway().await {
            Some(live) if !live.providers.is_empty() => Arc::new(live),
            _ => {
                eprintln!("live validation skipped: no provider keys in the environment");
                return;
            }
        }
    };
}

#[tokio::test]
#[ignore = "requires provider API keys; run with --ignored in the keyed CI job"]
async fn live_sync_text_every_provider() {
    let live = require_live!();
    for (provider, model) in &live.providers {
        let res = chat(
            &live,
            json!({
                "model": format!("{provider}/{model}"),
                "messages": [{"role": "user", "content": "Reply with exactly the word: pong"}],
                "max_tokens": 20,
            }),
        )
        .await;
        assert_eq!(
            res.status(),
            200,
            "{provider} sync failed: {}",
            res.text().await.unwrap()
        );
        let body: Value = res.json().await.unwrap();
        let content = body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default();
        assert!(
            content.to_lowercase().contains("pong"),
            "{provider}: unexpected content: {content:?}"
        );
        assert!(
            body["usage"]["total_tokens"].as_u64().unwrap_or(0) > 0,
            "{provider}: no usage"
        );
        println!("  ok  {provider}/{model} sync");
    }
}

#[tokio::test]
#[ignore = "requires provider API keys; run with --ignored in the keyed CI job"]
async fn live_streaming_every_provider() {
    let live = require_live!();
    for (provider, model) in &live.providers {
        let res = chat(
            &live,
            json!({
                "model": format!("{provider}/{model}"),
                "messages": [{"role": "user", "content": "Count from 1 to 5, digits only."}],
                "max_tokens": 50, "stream": true,
            }),
        )
        .await;
        assert_eq!(res.status(), 200, "{provider} stream failed");
        let text = res.text().await.unwrap();
        assert!(
            text.trim_end().ends_with("data: [DONE]"),
            "{provider}: no [DONE]"
        );
        let chunks = text.lines().filter(|l| l.starts_with("data: {")).count();
        assert!(
            chunks >= 2,
            "{provider}: stream collapsed into {chunks} chunk(s)"
        );
        println!("  ok  {provider}/{model} streaming ({chunks} chunks)");
    }
}

#[tokio::test]
#[ignore = "requires provider API keys; run with --ignored in the keyed CI job"]
async fn live_tool_call_every_provider() {
    let live = require_live!();
    for (provider, model) in &live.providers {
        let res = chat(&live, json!({
            "model": format!("{provider}/{model}"),
            "messages": [{"role": "user", "content": "What's the weather in Paris? Use the tool."}],
            "tools": [{"type": "function", "function": {"name": "get_weather",
                "description": "Get current weather for a city",
                "parameters": {"type": "object",
                    "properties": {"city": {"type": "string"}}, "required": ["city"]}}}],
            "tool_choice": "required",
            "max_tokens": 200,
        })).await;
        assert_eq!(
            res.status(),
            200,
            "{provider} tools failed: {}",
            res.text().await.unwrap()
        );
        let body: Value = res.json().await.unwrap();
        let calls = body["choices"][0]["message"]["tool_calls"]
            .as_array()
            .unwrap_or_else(|| panic!("{provider}: no tool_calls in {body}"));
        assert_eq!(calls[0]["function"]["name"], "get_weather", "{provider}");
        let args: Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        let city = args["city"].as_str().unwrap_or_default().to_lowercase();
        assert!(city.contains("paris"), "{provider}: args {args}");
        println!("  ok  {provider}/{model} tool call ({args})");
    }
}

#[tokio::test]
#[ignore = "requires provider API keys; run with --ignored in the keyed CI job"]
async fn live_multi_turn_tool_round_trip() {
    let live = require_live!();
    for (provider, model) in &live.providers {
        let res = chat(
            &live,
            json!({
                "model": format!("{provider}/{model}"),
                "messages": [
                    {"role": "user", "content": "What's the weather in Paris?"},
                    {"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}]},
                    {"role": "tool", "tool_call_id": "call_1",
                     "content": "{\"temp_c\": 21, \"conditions\": \"sunny\"}"}
                ],
                "tools": [{"type": "function", "function": {"name": "get_weather",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}],
                "max_tokens": 100,
            }),
        )
        .await;
        assert_eq!(
            res.status(),
            200,
            "{provider} round trip failed: {}",
            res.text().await.unwrap()
        );
        let body: Value = res.json().await.unwrap();
        let content = body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase();
        assert!(
            content.contains("21") || content.contains("sunny"),
            "{provider}: answer ignored the tool result: {content:?}"
        );
        println!("  ok  {provider}/{model} multi-turn round trip");
    }
}

#[tokio::test]
#[ignore = "requires provider API keys; run with --ignored in the keyed CI job"]
async fn live_vision_every_provider() {
    // A 1x1 red PNG.
    const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg==";
    let live = require_live!();
    for (provider, model) in &live.providers {
        let res = chat(&live, json!({
            "model": format!("{provider}/{model}"),
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "Reply with the dominant color of this image, one word."},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{PIXEL}")}}]}],
            "max_tokens": 20,
        })).await;
        assert_eq!(
            res.status(),
            200,
            "{provider} vision failed: {}",
            res.text().await.unwrap()
        );
        println!("  ok  {provider}/{model} vision");
    }
}

#[tokio::test]
#[ignore = "requires provider API keys; run with --ignored in the keyed CI job"]
async fn live_cross_dialect_agent_shape() {
    // Anthropic wire format driving every provider — the coding-agent path.
    let live = require_live!();
    let client = reqwest::Client::new();
    for (provider, model) in &live.providers {
        let res = client
            .post(format!("{}/anthropic/v1/messages", live.url))
            .json(&json!({
                "model": format!("{provider}/{model}"),
                "max_tokens": 30,
                "messages": [{"role": "user", "content": "Reply with exactly the word: pong"}],
            }))
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            200,
            "{provider} agent-shape failed: {}",
            res.text().await.unwrap()
        );
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["type"], "message", "{provider}");
        assert_eq!(body["role"], "assistant", "{provider}");
        println!("  ok  anthropic-wire -> {provider}/{model}");
    }
}
