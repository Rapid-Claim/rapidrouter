//! Subscription seats end to end: a Claude Code credential and a Codex
//! credential serving ordinary `/v1/chat/completions` traffic.
//!
//! These assert the things that make a subscription transport different
//! from a metered one — the credential presented, the headers that admit
//! it, the identity it must carry, and what happens when a seat runs out
//! of quota — because none of that is covered by the dialect suites the
//! rest of the gateway is tested with.

use mock_provider::MockProvider;
use router_core::config::{Config, Format};
use router_server::{AppState, build_router};
use serde_json::{Value, json};

/// No ambient environment: every credential in these tests is inline or
/// file-backed, so a stray `ANTHROPIC_API_KEY` on the runner cannot
/// change what is under test.
struct NoEnv;
impl router_core::config::EnvSource for NoEnv {
    fn get(&self, _name: &str) -> Option<String> {
        None
    }
}

/// A Codex `auth.json`, with a JWT-shaped access token whose `exp` is far
/// enough out that nothing tries to renew it mid-test.
fn codex_auth_json(exp_secs: u64) -> String {
    use base64::Engine;
    let encode = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
    let jwt = |claims: Value| {
        format!(
            "{}.{}.{}",
            encode(br#"{"alg":"RS256"}"#),
            encode(claims.to_string().as_bytes()),
            encode(b"sig")
        )
    };
    json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "access_token": jwt(json!({"exp": exp_secs})),
            "refresh_token": "rt.1.AAAtest",
            "id_token": jwt(json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": "acct-test"}
            })),
            "account_id": "acct-test"
        }
    })
    .to_string()
}

async fn gateway() -> (String, MockProvider, tempfile::TempDir) {
    let mock = MockProvider::spawn().await;
    let dir = tempfile::tempdir().unwrap();
    let auth_path = dir.path().join("auth.json");
    std::fs::write(&auth_path, codex_auth_json(4_000_000_000)).unwrap();

    let config = Config::from_str_with_env(
        &format!(
            r#"
[providers.claude-max]
type = "claude_subscription"
base_url = "{base}"
keys = [{{ name = "seat-1", value = "sk-ant-oat01-test-seat" }}]

[providers.codex]
type = "codex_subscription"
base_url = "{base}"
codex = {{ version = "0.199.0", reasoning_effort = "medium" }}
keys = [{{ name = "seat-1", value = "file:{auth}" }}]

[console]
admin_keys = ["probe-test-key"]
"#,
            base = mock.base_url(),
            auth = auth_path.display(),
        ),
        Format::Toml,
        &NoEnv,
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
    (url, mock, dir)
}

async fn chat(url: &str, body: Value) -> (reqwest::StatusCode, Value) {
    let res = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = res.status();
    let text = res.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or_else(|_| json!({"raw": text})),
    )
}

// ---------------------------------------------------------------------------
// Claude
// ---------------------------------------------------------------------------

#[tokio::test]
async fn claude_seat_presents_a_bearer_token_and_the_oauth_beta() {
    let (url, mock, _dir) = gateway().await;
    let (status, body) = chat(
        &url,
        json!({"model": "claude-max/claude-sonnet-4-5",
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let request = mock.last_request();
    assert_eq!(request.path, "/v1/messages");
    assert_eq!(
        request.authorization.as_deref(),
        Some("Bearer sk-ant-oat01-test-seat"),
        "a subscription token is a bearer, never an x-api-key",
    );
    assert_eq!(
        request.api_key, None,
        "sending x-api-key alongside would be a second, wrong credential",
    );
    assert!(
        request
            .header("anthropic-beta")
            .is_some_and(|v| v.contains("oauth-2025-04-20")),
        "without the OAuth beta the token is not accepted as an inference credential",
    );
    assert!(request.header("anthropic-version").is_some());
}

#[tokio::test]
async fn claude_seat_pins_the_claude_code_identity_ahead_of_the_caller_prompt() {
    let (url, mock, _dir) = gateway().await;
    let (status, _) = chat(
        &url,
        json!({"model": "claude-max/claude-sonnet-4-5",
               "messages": [
                   {"role": "system", "content": "You are a pirate."},
                   {"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, 200);

    let system = mock.last_request().body["system"].clone();
    let blocks = system.as_array().expect("system is a block array");
    assert_eq!(
        blocks[0]["text"], "You are Claude Code, Anthropic's official CLI for Claude.",
        "the token is authorized for the Claude Code identity",
    );
    assert_eq!(
        blocks[1]["text"], "You are a pirate.",
        "the caller's prompt survives as its own block",
    );
}

#[tokio::test]
async fn a_caller_beta_rides_along_with_the_oauth_beta() {
    let (url, mock, _dir) = gateway().await;
    let res = reqwest::Client::new()
        .post(format!("{url}/anthropic/v1/messages"))
        .header("anthropic-beta", "prompt-caching-2024-07-31")
        .json(
            &json!({"model": "claude-max/claude-sonnet-4-5", "max_tokens": 16,
                      "messages": [{"role": "user", "content": "hi"}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    let betas = mock
        .last_request()
        .header("anthropic-beta")
        .unwrap()
        .to_owned();
    assert!(betas.contains("oauth-2025-04-20"), "{betas}");
    assert!(
        betas.contains("prompt-caching-2024-07-31"),
        "dropping the caller's beta would silently disable their caching: {betas}",
    );
}

#[tokio::test]
async fn claude_seat_streams_like_any_other_anthropic_target() {
    let (url, _mock, _dir) = gateway().await;
    let res = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .json(
            &json!({"model": "claude-max/claude-sonnet-4-5", "stream": true,
                      "messages": [{"role": "user", "content": "hi"}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body = res.text().await.unwrap();
    assert!(body.contains("chat.completion.chunk"), "{body}");
    assert!(body.contains("[DONE]"));
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

#[tokio::test]
async fn codex_seat_presents_the_cli_header_set() {
    let (url, mock, _dir) = gateway().await;
    let (status, body) = chat(
        &url,
        json!({"model": "codex/gpt-5.5",
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let request = mock.last_request();
    assert_eq!(request.path, "/backend-api/codex/responses");
    assert_eq!(
        request.header("chatgpt-account-id"),
        Some("acct-test"),
        "the account id is decoded from the credential, not configured",
    );
    assert_eq!(
        request.header("version"),
        Some("0.199.0"),
        "the configured version must reach the backend — it is a model gate",
    );
    assert_eq!(request.header("user-agent"), Some("codex_cli_rs/0.199.0"));
    assert_eq!(request.header("originator"), Some("codex_cli_rs"));
    assert_eq!(
        request.header("openai-beta"),
        Some("responses=experimental")
    );
    assert!(
        request.header("session_id").is_some_and(|s| !s.is_empty()),
        "the CLI sends a fresh session id per request",
    );
    assert!(
        request
            .authorization
            .as_deref()
            .unwrap()
            .starts_with("Bearer ey"),
        "the access token out of auth.json, not the document itself",
    );
}

#[tokio::test]
async fn codex_body_is_the_responses_shape_the_backend_accepts() {
    let (url, mock, _dir) = gateway().await;
    let (status, _) = chat(
        &url,
        json!({"model": "codex/gpt-5.5", "max_tokens": 100,
               "messages": [
                   {"role": "system", "content": "Be terse."},
                   {"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, 200);

    let body = mock.last_request().body;
    assert_eq!(body["store"], false);
    assert_eq!(
        body["stream"], true,
        "this backend has no non-streaming mode"
    );
    assert_eq!(body["instructions"], "Be terse.");
    assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(
        body["reasoning"]["effort"], "medium",
        "the configured floor, not the built-in default",
    );
    assert!(
        body.get("max_output_tokens").is_none(),
        "the backend 400s on this parameter, so max_tokens cannot be carried",
    );
}

#[tokio::test]
async fn a_codex_tool_call_survives_the_empty_completed_output() {
    // The failure this guards is silent: read only response.completed and
    // the caller gets a 200 with no tool_calls at all.
    let (url, _mock, _dir) = gateway().await;
    let (status, body) = chat(
        &url,
        json!({"model": "codex/gpt-5.5",
               "messages": [{"role": "user", "content": "weather?"}],
               "tools": [{"type": "function", "function": {
                   "name": "get_weather",
                   "parameters": {"type": "object"}}}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let call = &body["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["id"], "call_mock", "{body}");
    assert_eq!(call["function"]["name"], "get_weather");
    assert_eq!(call["function"]["arguments"], "{\"city\":\"SF\"}");
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
}

#[tokio::test]
async fn a_codex_stream_is_aggregated_for_a_non_streaming_caller() {
    let (url, _mock, _dir) = gateway().await;
    let (status, body) = chat(
        &url,
        json!({"model": "codex/gpt-5.5",
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["choices"][0]["message"]["content"], "Hello from Codex",
        "the caller asked for a whole response; the backend only speaks SSE",
    );
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["usage"]["prompt_tokens"], 11);
    assert_eq!(body["usage"]["completion_tokens"], 3);
}

#[tokio::test]
async fn codex_streams_through_to_a_streaming_caller() {
    let (url, _mock, _dir) = gateway().await;
    let res = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .json(&json!({"model": "codex/gpt-5.5", "stream": true,
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body = res.text().await.unwrap();
    assert!(body.contains("chat.completion.chunk"), "{body}");
    assert!(body.contains("Hello"), "{body}");
    assert!(body.contains("[DONE]"));
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[test]
fn a_bad_codex_knob_fails_config_validation() {
    // The backend answers an unsupported value with a 400 on *every*
    // request, so a typo here must not produce a config that starts and
    // then refuses all traffic.
    let err = Config::from_str_with_env(
        r#"
[providers.codex]
type = "codex_subscription"
codex = { reasoning_effort = "ludicrous" }
keys = [{ name = "s", value = "inline" }]
"#,
        Format::Toml,
        &NoEnv,
    )
    .unwrap_err();
    let rendered = format!("{err:?}");
    assert!(rendered.contains("reasoning_effort"), "{rendered}");
}

#[test]
fn the_codex_block_is_rejected_on_other_providers() {
    let err = Config::from_str_with_env(
        r#"
[providers.openai]
codex = { version = "0.146.0" }
keys = [{ name = "s", value = "sk-test" }]
"#,
        Format::Toml,
        &NoEnv,
    )
    .unwrap_err();
    assert!(format!("{err:?}").contains("codex"));
}

#[test]
fn an_empty_reasoning_effort_means_the_backend_default() {
    let config = Config::from_str_with_env(
        r#"
[providers.codex]
type = "codex_subscription"
codex = { reasoning_effort = "", verbosity = "" }
keys = [{ name = "s", value = "inline" }]
"#,
        Format::Toml,
        &NoEnv,
    )
    .unwrap();
    let codex = config.providers["codex"].codex.as_ref().unwrap();
    assert_eq!(codex.reasoning_effort, None);
    assert_eq!(codex.verbosity, None);
}

// ---------------------------------------------------------------------------
// Quota
// ---------------------------------------------------------------------------

/// A seat that is out of quota must stay out for the window the provider
/// reported, not for the breaker's configured cooldown.
///
/// The check is that the *upstream* stops being asked. Re-probing an
/// exhausted seat is not merely wasteful: each probe consumes one of the
/// caller's retry attempts and earns another 429, which is how a pool
/// spends its whole retry budget achieving nothing.
async fn seat_is_benched_after_a_429(model: &str) {
    let (url, mock, _dir) = gateway().await;
    let (status, _) = chat(
        &url,
        json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, 429, "the caller sees the provider's own status");
    let after_first = mock.request_count();

    for _ in 0..3 {
        let (status, _) = chat(
            &url,
            json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_ne!(status, 200);
    }
    assert_eq!(
        mock.request_count(),
        after_first,
        "a benched seat must not be probed again before its window rolls",
    );
}

#[tokio::test]
async fn a_codex_seat_is_benched_from_headers_alone() {
    // Codex sends no retry-after; the window is only in x-codex-*.
    seat_is_benched_after_a_429("codex/quota-codex").await;
}

#[tokio::test]
async fn a_claude_seat_is_benched_for_the_reported_window() {
    seat_is_benched_after_a_429("claude-max/quota-claude").await;
}

/// A streaming Responses request reaches a Codex seat verbatim.
///
/// The Codex backend *is* the Responses API, so the surface must not be
/// forced through the stateless chat core on the way: that round trip
/// rejected every input item the core does not model — `additional_tools`
/// among them — turning a request the backend understands perfectly into
/// a 400 from us. Relaying keeps unknown-to-us-but-known-to-them items
/// intact, which is the only way this stays compatible as OpenAI adds to
/// the surface.
#[tokio::test]
async fn codex_relays_the_responses_surface_verbatim() {
    let (url, mock, _dir) = gateway().await;
    let res = reqwest::Client::new()
        .post(format!("{url}/v1/responses"))
        .json(&json!({
            "model": "codex/gpt-5.5",
            "stream": true,
            "instructions": "You are Codex.",
            "input": [
                { "type": "message", "role": "user",
                  "content": [{ "type": "input_text", "text": "hi" }] },
                { "type": "additional_tools", "tools": ["web_search"] }
            ]
        }))
        .send()
        .await
        .unwrap();
    let status = res.status();
    let text = res.text().await.unwrap();
    assert_eq!(status, 200, "{text}");

    let request = mock.last_request();
    assert_eq!(request.path, "/backend-api/codex/responses");
    let sent = &request.body;
    let items = sent["input"].as_array().expect("input relayed as sent");
    assert!(
        items.iter().any(|i| i["type"] == "additional_tools"),
        "an input item the chat core cannot model must still reach the backend: {sent}",
    );
    assert_eq!(
        sent["model"], "gpt-5.5",
        "the routed model replaces the alias"
    );
    assert_eq!(sent["stream"], true, "the backend speaks only SSE");
    assert_eq!(sent["store"], false, "the backend keeps no state for us");
}

// ---------------------------------------------------------------------------
// The console's "Check" button
// ---------------------------------------------------------------------------

/// Probe one credential through the admin API, as the console does.
/// `model` empty means "send no override", which is what the console
/// itself sends.
async fn probe(url: &str, provider: &str, model: &str) -> Value {
    let client = reqwest::Client::new();
    let token = client
        .post(format!("{url}/admin/api/session"))
        .json(&json!({"key": "probe-test-key"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["token"]
        .as_str()
        .expect("a session token")
        .to_owned();
    client
        .post(format!("{url}/admin/api/providers/{provider}/probe"))
        .bearer_auth(&token)
        .json(&if model.is_empty() {
            json!({})
        } else {
            json!({ "model": model })
        })
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["results"][0]
        .clone()
}

/// The check has to ask for what the backend actually serves.
///
/// A hand-rolled `/chat/completions` body is not a request the Codex
/// backend answers — it serves its private Responses surface at its own
/// path and nothing else — so a probe built that way is refused whatever
/// the seat's real state, and every credential reads as broken.
#[tokio::test]
async fn a_codex_probe_asks_the_backend_for_what_it_serves() {
    let (url, mock, _dir) = gateway().await;
    let result = probe(&url, "codex", "gpt-5.5").await;
    assert_eq!(result["status"], "ok", "{result}");

    let request = mock.last_request();
    assert_eq!(request.path, "/backend-api/codex/responses");
    assert_eq!(request.body["model"], "gpt-5.5");
    assert_eq!(request.body["stream"], true, "the backend speaks only SSE");
    assert!(
        request.body["input"].is_array(),
        "the Responses shape, not a chat body: {}",
        request.body,
    );
}

/// The same for a Claude seat: its token is only authorized for the
/// Claude Code identity, so a probe without it is refused by the model.
#[tokio::test]
async fn a_claude_probe_carries_the_identity_its_token_is_authorized_for() {
    let (url, mock, _dir) = gateway().await;
    let result = probe(&url, "claude-max", "claude-sonnet-4-5").await;
    assert_eq!(result["status"], "ok", "{result}");

    let request = mock.last_request();
    assert_eq!(request.path, "/v1/messages");
    assert_eq!(
        request.body["system"][0]["text"],
        "You are Claude Code, Anthropic's official CLI for Claude.",
    );
}

/// A check is also how the plan windows first get reported, and the
/// console labels each window from the length that arrives here. Codex
/// sizes its windows per plan — this one is weekly — so a gateway that
/// dropped the length would leave the console guessing, which is how a
/// weekly window ends up drawn as a five-hour one.
#[tokio::test]
async fn a_codex_probe_reports_the_window_length_the_plan_actually_has() {
    let (url, _mock, _dir) = gateway().await;
    let result = probe(&url, "codex", "quota-codex").await;
    assert_eq!(result["status"], "rate_limited", "{result}");

    let client = reqwest::Client::new();
    let token = client
        .post(format!("{url}/admin/api/session"))
        .json(&json!({"key": "probe-test-key"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let providers = client
        .get(format!("{url}/admin/api/providers"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let seat = providers["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "codex")
        .expect("the codex provider")["keys"][0]
        .clone();

    assert_eq!(
        seat["quota"]["primary"]["length_s"], 604_800,
        "the plan's window is a week, and the console names it from this: {seat}",
    );
    assert_eq!(seat["quota"]["primary"]["utilization"], 1.0);
    assert!(
        seat["quota"]["secondary"].is_null(),
        "a plan with one window must not grow a second, empty one: {seat}",
    );
}

/// A seat is checkable the moment it is added.
///
/// The console's button sends no model, and these seats declare none —
/// what a subscription serves is decided by the plan, not configured. A
/// probe that gave up here would report "no model declared" for every
/// freshly added credential, which reads as a broken seat.
#[tokio::test]
async fn a_seat_can_be_checked_before_any_model_is_declared() {
    let (url, mock, _dir) = gateway().await;
    let result = probe(&url, "codex", "").await;
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(result["model"], "gpt-5.5", "the plan's own model: {result}");
    assert_eq!(mock.last_request().path, "/backend-api/codex/responses");

    let result = probe(&url, "claude-max", "").await;
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(mock.last_request().path, "/v1/messages");
}
