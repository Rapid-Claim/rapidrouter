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
//! flake the suite. Each scenario runs against every configured provider
//! and reports all failures — one broken provider must not mask the rest.

use std::sync::Arc;

use router_core::config::Config;
use router_server::{AppState, build_router};
use serde_json::{Value, json};

struct Live {
    url: String,
    providers: Vec<(String, String)>, // (provider, model)
}

async fn live_gateway() -> Option<Arc<Live>> {
    let env = |var: &str| std::env::var(var).ok();
    let config = Config::discover_from_env(&env)?;
    let mut providers = Vec::new();
    for (name, default_model) in [
        ("openai", "gpt-4o-mini"),
        ("anthropic", "claude-haiku-4-5"),
        ("gemini", "gemini-flash-latest"),
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
    Some(Arc::new(Live { url, providers }))
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
            Some(live) if !live.providers.is_empty() => live,
            _ => {
                eprintln!("live validation skipped: no provider keys in the environment");
                return;
            }
        }
    };
}

/// Run one scenario against every configured provider, collecting
/// failures instead of stopping at the first — a CI report must show
/// every provider's state, not just the first broken one.
async fn for_each_provider<F, Fut>(live: &Arc<Live>, scenario: &str, run: F)
where
    F: Fn(Arc<Live>, String, String) -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    let mut failures = Vec::new();
    for (provider, model) in live.providers.clone() {
        match run(live.clone(), provider.clone(), model.clone()).await {
            Ok(note) => println!("  ok  {provider}/{model} {scenario} {note}"),
            Err(err) => {
                println!("  FAIL {provider}/{model} {scenario}: {err}");
                failures.push(format!("{provider}: {err}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{scenario} failed for: {}",
        failures.join(" | ")
    );
}

fn ensure_status(status: reqwest::StatusCode, body: &str) -> Result<(), String> {
    if status == 200 {
        Ok(())
    } else {
        Err(format!("HTTP {status}: {body}"))
    }
}

#[tokio::test]
#[ignore = "requires provider API keys; run with --ignored in the keyed CI job"]
async fn live_sync_text_every_provider() {
    let live = require_live!();
    for_each_provider(&live, "sync", |live, provider, model| async move {
        let res = chat(
            &live,
            json!({
                "model": format!("{provider}/{model}"),
                "messages": [{"role": "user", "content": "Reply with exactly the word: pong"}],
                "max_tokens": 20,
            }),
        )
        .await;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        ensure_status(status, &text)?;
        let body: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let content = body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default();
        if !content.to_lowercase().contains("pong") {
            return Err(format!("unexpected content: {content:?}"));
        }
        if body["usage"]["total_tokens"].as_u64().unwrap_or(0) == 0 {
            return Err("no usage reported".into());
        }
        Ok(String::new())
    })
    .await;
}

#[tokio::test]
#[ignore = "requires provider API keys; run with --ignored in the keyed CI job"]
async fn live_streaming_every_provider() {
    let live = require_live!();
    for_each_provider(&live, "streaming", |live, provider, model| async move {
        let res = chat(
            &live,
            json!({
                "model": format!("{provider}/{model}"),
                "messages": [{"role": "user", "content": "Count from 1 to 5, digits only."}],
                "max_tokens": 50, "stream": true,
            }),
        )
        .await;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        ensure_status(status, &text)?;
        if !text.trim_end().ends_with("data: [DONE]") {
            return Err("stream missing [DONE]".into());
        }
        let chunks = text.lines().filter(|l| l.starts_with("data: {")).count();
        if chunks < 2 {
            return Err(format!("stream collapsed into {chunks} chunk(s)"));
        }
        Ok(format!("({chunks} chunks)"))
    })
    .await;
}

#[tokio::test]
#[ignore = "requires provider API keys; run with --ignored in the keyed CI job"]
async fn live_tool_call_every_provider() {
    let live = require_live!();
    for_each_provider(&live, "tool call", |live, provider, model| async move {
        let res = chat(
            &live,
            json!({
                "model": format!("{provider}/{model}"),
                "messages": [{"role": "user", "content": "What's the weather in Paris? Use the tool."}],
                "tools": [{"type": "function", "function": {"name": "get_weather",
                    "description": "Get current weather for a city",
                    "parameters": {"type": "object",
                        "properties": {"city": {"type": "string"}}, "required": ["city"]}}}],
                "tool_choice": "required",
                "max_tokens": 500,
            }),
        )
        .await;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        ensure_status(status, &text)?;
        let body: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let calls = body["choices"][0]["message"]["tool_calls"]
            .as_array()
            .ok_or_else(|| format!("no tool_calls in {body}"))?;
        if calls[0]["function"]["name"] != "get_weather" {
            return Err(format!("wrong tool: {}", calls[0]["function"]["name"]));
        }
        let args: Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap_or("{}"))
                .map_err(|e| format!("unparseable arguments: {e}"))?;
        if !args["city"].as_str().unwrap_or_default().to_lowercase().contains("paris") {
            return Err(format!("args missed the city: {args}"));
        }
        Ok(format!("({args})"))
    })
    .await;
}

#[tokio::test]
#[ignore = "requires provider API keys; run with --ignored in the keyed CI job"]
async fn live_multi_turn_tool_round_trip() {
    let live = require_live!();
    for_each_provider(
        &live,
        "multi-turn round trip",
        |live, provider, model| async move {
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
                "max_tokens": 200,
            }),
        )
        .await;
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            ensure_status(status, &text)?;
            let body: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            let content = body["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase();
            if !(content.contains("21") || content.contains("sunny")) {
                return Err(format!("answer ignored the tool result: {content:?}"));
            }
            Ok(String::new())
        },
    )
    .await;
}

#[tokio::test]
#[ignore = "requires provider API keys; run with --ignored in the keyed CI job"]
async fn live_vision_every_provider() {
    // A 1x1 red PNG.
    const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg==";
    let live = require_live!();
    for_each_provider(&live, "vision", |live, provider, model| async move {
        let res = chat(
            &live,
            json!({
                "model": format!("{provider}/{model}"),
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "Reply with the dominant color of this image, one word."},
                    {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{PIXEL}")}}]}],
                "max_tokens": 100,
            }),
        )
        .await;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        ensure_status(status, &text)?;
        Ok(String::new())
    })
    .await;
}

#[tokio::test]
#[ignore = "requires provider API keys; run with --ignored in the keyed CI job"]
async fn live_cross_dialect_agent_shape() {
    // Anthropic wire format driving every provider — the coding-agent path.
    let live = require_live!();
    for_each_provider(
        &live,
        "anthropic-wire agent shape",
        |live, provider, model| async move {
            let res = reqwest::Client::new()
                .post(format!("{}/anthropic/v1/messages", live.url))
                .json(&json!({
                    "model": format!("{provider}/{model}"),
                    "max_tokens": 30,
                    "messages": [{"role": "user", "content": "Reply with exactly the word: pong"}],
                }))
                .timeout(std::time::Duration::from_secs(120))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            ensure_status(status, &text)?;
            let body: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            if body["type"] != "message" || body["role"] != "assistant" {
                return Err(format!("not an anthropic message shape: {body}"));
            }
            Ok(String::new())
        },
    )
    .await;
}
