//! Subscription seats against the REAL backends.
//!
//! Ignored by default — these spend a live subscription's quota and need
//! credentials that only exist on a developer's machine:
//!
//! ```sh
//! LIVE_CLAUDE_SUBSCRIPTION_TOKEN=sk-ant-oat01-… \
//! LIVE_CODEX_AUTH_JSON=~/.codex/auth.json \
//!   cargo test -p router-server --test live_subscriptions -- --ignored --nocapture
//! ```
//!
//! Assertions are semantic, and a `429` is treated as a **pass** for the
//! credential-acceptance checks: a rate-limit answer is proof the request
//! authenticated and reached quota enforcement, which is the thing being
//! verified. A `401` is the failure — that is a credential the backend
//! would not take.
//!
//! Getting the Claude token on macOS:
//!
//! ```sh
//! security find-generic-password -s "Claude Code-credentials" -w \
//!   | python3 -c 'import json,sys; print(json.load(sys.stdin)["claudeAiOauth"]["accessToken"])'
//! ```

use router_core::config::{Config, Format};
use router_server::{AppState, build_router};
use serde_json::{Value, json};

struct Env;
impl router_core::config::EnvSource for Env {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// A gateway carrying whichever subscription credentials are configured.
/// `None` when neither is, so the suite skips rather than fails.
async fn live_gateway() -> Option<(String, Vec<&'static str>)> {
    let mut sections = String::new();
    let mut kinds = Vec::new();

    if std::env::var("LIVE_CLAUDE_SUBSCRIPTION_TOKEN").is_ok() {
        sections.push_str(
            r#"
[providers.claude-sub]
type = "claude_subscription"
keys = [{ name = "seat-1", value = "env.LIVE_CLAUDE_SUBSCRIPTION_TOKEN" }]
"#,
        );
        kinds.push("claude");
    }
    if let Ok(path) = std::env::var("LIVE_CODEX_AUTH_JSON") {
        sections.push_str(&format!(
            r#"
[providers.codex-sub]
type = "codex_subscription"
keys = [{{ name = "seat-1", value = "file:{path}" }}]
"#
        ));
        kinds.push("codex");
    }
    if kinds.is_empty() {
        eprintln!("skipping: no subscription credentials configured");
        return None;
    }

    let config = Config::from_str_with_env(&sections, Format::Toml, &Env)
        .expect("live subscription config is valid");
    let state = AppState::new(config);
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        router_server::serve(listener, state, app, std::future::pending())
            .await
            .unwrap()
    });
    Some((url, kinds))
}

async fn post(url: &str, body: Value) -> (reqwest::StatusCode, String) {
    let res = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("request reaches the gateway");
    (res.status(), res.text().await.unwrap_or_default())
}

/// A `429` means authenticated-and-out-of-quota, which proves the
/// credential and headers were accepted. A `401` means they were not.
fn assert_credential_accepted(kind: &str, status: reqwest::StatusCode, body: &str) {
    assert_ne!(
        status, 401,
        "{kind}: the backend refused the credential — {body}"
    );
    assert!(
        status == 200 || status == 429,
        "{kind}: unexpected {status} — {body}"
    );
    if status == 429 {
        eprintln!("{kind}: out of quota (credential accepted, quota enforced)");
    }
}

fn model_for(kind: &str) -> (&'static str, String) {
    match kind {
        "claude" => (
            "claude-sub",
            std::env::var("LIVE_CLAUDE_SUBSCRIPTION_MODEL")
                .unwrap_or_else(|_| "claude-haiku-4-5".into()),
        ),
        _ => (
            "codex-sub",
            std::env::var("LIVE_CODEX_MODEL").unwrap_or_else(|_| "gpt-5.5".into()),
        ),
    }
}

#[tokio::test]
#[ignore = "spends live subscription quota"]
async fn a_seat_credential_is_accepted_by_its_backend() {
    let Some((url, kinds)) = live_gateway().await else {
        return;
    };
    for kind in kinds {
        let (provider, model) = model_for(kind);
        let (status, body) = post(
            &url,
            json!({"model": format!("{provider}/{model}"),
                   "messages": [{"role": "user", "content": "Reply with exactly: PONG"}]}),
        )
        .await;
        assert_credential_accepted(kind, status, &body);
        if status == 200 {
            let value: Value = serde_json::from_str(&body).unwrap();
            assert!(
                value["choices"][0]["message"]["content"]
                    .as_str()
                    .is_some_and(|c| !c.is_empty()),
                "{kind}: a 200 with no content — {body}"
            );
            assert!(value["usage"]["prompt_tokens"].as_u64().unwrap_or(0) > 0);
        }
    }
}

#[tokio::test]
#[ignore = "spends live subscription quota"]
async fn a_seat_streams() {
    let Some((url, kinds)) = live_gateway().await else {
        return;
    };
    for kind in kinds {
        let (provider, model) = model_for(kind);
        let res = reqwest::Client::new()
            .post(format!("{url}/v1/chat/completions"))
            .json(
                &json!({"model": format!("{provider}/{model}"), "stream": true,
                          "messages": [{"role": "user", "content": "count to three"}]}),
            )
            .send()
            .await
            .unwrap();
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        assert_credential_accepted(kind, status, &body);
        if status == 200 {
            assert!(body.contains("chat.completion.chunk"), "{kind}: {body}");
            assert!(body.contains("[DONE]"), "{kind}: stream never terminated");
        }
    }
}

#[tokio::test]
#[ignore = "spends live subscription quota"]
async fn a_seat_calls_a_tool() {
    let Some((url, kinds)) = live_gateway().await else {
        return;
    };
    for kind in kinds {
        let (provider, model) = model_for(kind);
        let (status, body) = post(
            &url,
            json!({"model": format!("{provider}/{model}"),
                   "messages": [{"role": "user", "content": "What is the weather in SF? Use the tool."}],
                   "tools": [{"type": "function", "function": {
                       "name": "get_weather",
                       "description": "Current weather for a city",
                       "parameters": {"type": "object",
                                      "properties": {"city": {"type": "string"}},
                                      "required": ["city"]}}}]}),
        )
        .await;
        assert_credential_accepted(kind, status, &body);
        if status == 200 {
            let value: Value = serde_json::from_str(&body).unwrap();
            let call = &value["choices"][0]["message"]["tool_calls"][0];
            assert_eq!(call["function"]["name"], "get_weather", "{kind}: {body}");
            let arguments = call["function"]["arguments"].as_str().unwrap_or("");
            serde_json::from_str::<Value>(arguments).unwrap_or_else(|e| {
                panic!("{kind}: tool arguments are not JSON ({e}): {arguments}")
            });
        }
    }
}

#[tokio::test]
#[ignore = "spends live subscription quota"]
async fn a_claude_seat_keeps_the_callers_system_prompt() {
    // The identity block is prepended, not substituted: a caller's own
    // instructions must still steer the answer.
    if std::env::var("LIVE_CLAUDE_SUBSCRIPTION_TOKEN").is_err() {
        return;
    }
    let Some((url, _)) = live_gateway().await else {
        return;
    };
    let model = std::env::var("LIVE_CLAUDE_SUBSCRIPTION_MODEL")
        .unwrap_or_else(|_| "claude-haiku-4-5".into());
    let (status, body) = post(
        &url,
        json!({"model": format!("claude-sub/{model}"),
               "messages": [
                   {"role": "system",
                    "content": "You always answer with exactly the word BANANA and nothing else."},
                   {"role": "user", "content": "What is the capital of France?"}]}),
    )
    .await;
    assert_credential_accepted("claude", status, &body);
    if status == 200 {
        let value: Value = serde_json::from_str(&body).unwrap();
        let content = value["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_uppercase();
        assert!(
            content.contains("BANANA"),
            "the caller's system prompt was not honoured — identity pinning may be \
             overriding it rather than leading it: {content}"
        );
    }
}
