//! The conformance corpus: every request-shape case — message formats,
//! content parts (text/image/file/audio), tool definitions and choices,
//! tool-call histories, response formats, sampling params, edge cases —
//! run against every target dialect, sync and streaming.
//!
//! Each scenario declares its expected outcome per target: `Pass`, or
//! `Reject` with the offending parameter. The same corpus reruns against
//! live providers in `live_validation.rs` when keys are present.

use mock_provider::MockProvider;
use router_core::config::{Config, Format};
use router_server::{AppState, build_router};
use serde_json::{Value, json};

#[derive(Clone, Copy, PartialEq)]
enum Expect {
    Pass,
    /// 400 whose `error.param` equals this.
    Reject(&'static str),
}

struct Scenario {
    name: &'static str,
    /// Build the request body given the target model string.
    body: fn(&str) -> Value,
    openai: Expect,
    anthropic: Expect,
    gemini: Expect,
    /// Also meaningful to run with `"stream": true`.
    streamable: bool,
}

fn scenarios() -> Vec<Scenario> {
    fn s(
        name: &'static str,
        body: fn(&str) -> Value,
        (openai, anthropic, gemini): (Expect, Expect, Expect),
        streamable: bool,
    ) -> Scenario {
        Scenario {
            name,
            body,
            openai,
            anthropic,
            gemini,
            streamable,
        }
    }
    use Expect::*;

    vec![
        // --- message formats -------------------------------------------------
        s(
            "text_simple",
            |m| json!({"model": m, "messages": [{"role": "user", "content": "hi"}]}),
            (Pass, Pass, Pass),
            true,
        ),
        s(
            "system_prompt",
            |m| {
                json!({"model": m, "messages": [
            {"role": "system", "content": "be terse"},
            {"role": "user", "content": "hi"}]})
            },
            (Pass, Pass, Pass),
            true,
        ),
        s(
            "developer_role",
            |m| {
                json!({"model": m, "messages": [
            {"role": "developer", "content": "be terse"},
            {"role": "user", "content": "hi"}]})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "multiturn_text",
            |m| {
                json!({"model": m, "messages": [
            {"role": "user", "content": "one"},
            {"role": "assistant", "content": "two"},
            {"role": "user", "content": "three"}]})
            },
            (Pass, Pass, Pass),
            true,
        ),
        s(
            "content_parts_multi_text",
            |m| {
                json!({"model": m, "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "part one. "},
                {"type": "text", "text": "part two."}]}]})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "empty_assistant_in_history",
            |m| {
                json!({"model": m, "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": ""},
            {"role": "user", "content": "again"}]})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "unicode_heavy",
            |m| {
                json!({"model": m, "messages": [
            {"role": "user", "content": "héllo → wörld 你好 🎉 \"quoted\" \\backslash\\ \nnewline\ttab"}]})
            },
            (Pass, Pass, Pass),
            true,
        ),
        s(
            "consecutive_same_role",
            |m| {
                json!({"model": m, "messages": [
            {"role": "user", "content": "first"},
            {"role": "user", "content": "second"}]})
            },
            (Pass, Pass, Pass),
            false,
        ),
        // --- media parts -----------------------------------------------------
        s(
            "image_data_uri",
            |m| {
                json!({"model": m, "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "describe"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,aW1hZ2U="}}]}]})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "image_https_url",
            |m| {
                json!({"model": m, "messages": [
            {"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "https://example.com/cat.png", "detail": "low"}}]}]})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "multiple_images",
            |m| {
                json!({"model": m, "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "compare"},
                {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,b25l"}},
                {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,dHdv"}}]}]})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "pdf_file_part",
            |m| {
                json!({"model": m, "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "summarize"},
                {"type": "file", "file": {"filename": "doc.pdf",
                 "file_data": "data:application/pdf;base64,cGRmZGF0YQ=="}}]}]})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "audio_input_part",
            |m| {
                json!({"model": m, "messages": [
            {"role": "user", "content": [
                {"type": "input_audio", "input_audio": {"data": "YXVkaW8=", "format": "wav"}}]}]})
            },
            (Pass, Reject("messages"), Pass),
            false,
        ),
        // --- tools: definitions and choices ---------------------------------
        s(
            "tool_single",
            |m| {
                json!({"model": m,
            "messages": [{"role": "user", "content": "weather?"}], "tools": [tool_def()]})
            },
            (Pass, Pass, Pass),
            true,
        ),
        s(
            "tools_multiple_defs",
            |m| {
                json!({"model": m,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [tool_def(), {"type": "function", "function": {"name": "get_time",
                "parameters": {"type": "object", "properties": {}}}}]})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "tool_choice_auto",
            |m| {
                json!({"model": m,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [tool_def()], "tool_choice": "auto"})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "tool_choice_none",
            |m| {
                json!({"model": m,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [tool_def()], "tool_choice": "none"})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "tool_choice_required",
            |m| {
                json!({"model": m,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [tool_def()], "tool_choice": "required"})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "tool_choice_named",
            |m| {
                json!({"model": m,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [tool_def()],
            "tool_choice": {"type": "function", "function": {"name": "get_weather"}}})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "parallel_tool_calls_disabled",
            |m| {
                json!({"model": m,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [tool_def()], "parallel_tool_calls": false})
            },
            (Pass, Pass, Pass),
            false,
        ),
        // --- tools: histories ------------------------------------------------
        s(
            "tool_history_single",
            |m| {
                json!({"model": m, "messages": [
            {"role": "user", "content": "weather?"},
            {"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function",
                "function": {"name": "get_weather", "arguments": "{\"city\":\"Oslo\"}"}}]},
            {"role": "tool", "tool_call_id": "call_1", "content": "{\"temp\": 3}"}],
            "tools": [tool_def()]})
            },
            (Pass, Pass, Pass),
            true,
        ),
        s(
            "tool_history_parallel",
            |m| {
                json!({"model": m, "messages": [
            {"role": "user", "content": "compare"},
            {"role": "assistant", "tool_calls": [
                {"id": "call_1", "type": "function",
                 "function": {"name": "get_weather", "arguments": "{\"city\":\"Oslo\"}"}},
                {"id": "call_2", "type": "function",
                 "function": {"name": "get_weather", "arguments": "{\"city\":\"Rome\"}"}}]},
            {"role": "tool", "tool_call_id": "call_1", "content": "3C"},
            {"role": "tool", "tool_call_id": "call_2", "content": "19C"}],
            "tools": [tool_def()]})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "tool_history_mixed_text_and_call",
            |m| {
                json!({"model": m, "messages": [
            {"role": "user", "content": "weather?"},
            {"role": "assistant", "content": "let me check",
             "tool_calls": [{"id": "call_1", "type": "function",
                "function": {"name": "get_weather", "arguments": "{\"city\":\"Oslo\"}"}}]},
            {"role": "tool", "tool_call_id": "call_1", "content": "3C"}],
            "tools": [tool_def()]})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "tool_result_error_text",
            |m| {
                json!({"model": m, "messages": [
            {"role": "user", "content": "weather?"},
            {"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function",
                "function": {"name": "get_weather", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "call_1",
             "content": "error: city parameter missing"}],
            "tools": [tool_def()]})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "tool_call_empty_arguments",
            |m| {
                json!({"model": m, "messages": [
            {"role": "user", "content": "time?"},
            {"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function",
                "function": {"name": "get_time", "arguments": ""}}]},
            {"role": "tool", "tool_call_id": "call_1", "content": "noon"}]})
            },
            (Pass, Pass, Pass),
            false,
        ),
        // --- response formats ------------------------------------------------
        s(
            "json_object_mode",
            |m| {
                json!({"model": m,
            "messages": [{"role": "user", "content": "give json"}],
            "response_format": {"type": "json_object"}})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "json_schema_mode",
            |m| {
                json!({"model": m,
            "messages": [{"role": "user", "content": "give json"}],
            "response_format": {"type": "json_schema", "json_schema": {"name": "out",
                "schema": {"type": "object", "properties": {"answer": {"type": "string"}}}}}})
            },
            (Pass, Pass, Pass),
            true,
        ),
        s(
            "json_schema_additional_props",
            |m| {
                json!({"model": m,
            "messages": [{"role": "user", "content": "give json"}],
            "response_format": {"type": "json_schema", "json_schema": {"name": "out", "strict": true,
                "schema": {"type": "object", "additionalProperties": false,
                    "properties": {"n": {"type": "integer"}}}}}})
            },
            (Pass, Pass, Pass),
            false,
        ),
        // --- params and capability edges ------------------------------------
        s(
            "stop_string",
            |m| {
                json!({"model": m,
            "messages": [{"role": "user", "content": "count"}], "stop": "END"})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "stop_array",
            |m| {
                json!({"model": m,
            "messages": [{"role": "user", "content": "count"}], "stop": ["END", "STOP"]})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "sampling_params",
            |m| {
                json!({"model": m,
            "messages": [{"role": "user", "content": "hi"}],
            "temperature": 0.2, "top_p": 0.9, "max_tokens": 64})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "max_completion_tokens",
            |m| {
                json!({"model": m,
            "messages": [{"role": "user", "content": "hi"}], "max_completion_tokens": 64})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "penalties_and_seed",
            |m| {
                json!({"model": m,
            "messages": [{"role": "user", "content": "hi"}],
            "presence_penalty": 0.1, "frequency_penalty": 0.2, "seed": 7})
            },
            (Pass, Pass, Pass),
            false,
        ),
        s(
            "n_greater_than_one",
            |m| {
                json!({"model": m,
            "messages": [{"role": "user", "content": "hi"}], "n": 2})
            },
            (Pass, Reject("n"), Pass),
            false,
        ),
        s(
            "logprobs_requested",
            |m| {
                json!({"model": m,
            "messages": [{"role": "user", "content": "hi"}], "logprobs": true})
            },
            (Pass, Reject("logprobs"), Reject("logprobs")),
            false,
        ),
        s(
            "unknown_extra_field",
            |m| {
                json!({"model": m,
            "messages": [{"role": "user", "content": "hi"}],
            "some_vendor_extension": {"nested": [1, 2, 3]}})
            },
            (Pass, Pass, Pass),
            false,
        ),
    ]
}

fn tool_def() -> Value {
    json!({"type": "function", "function": {"name": "get_weather",
        "description": "look up weather",
        "parameters": {"type": "object",
            "properties": {"city": {"type": "string"}}, "required": ["city"]}}})
}

async fn gateway() -> (String, MockProvider) {
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
[providers.azure]
endpoint = "{base}"
api_version = "2024-10-21"
keys = [{{ name = "k", value = "az-key" }}]
[providers.vertex]
project = "conf-project"
base_url = "{base}"
keys = [{{ name = "k", value = "vx-token" }}]
[providers.bedrock]
region = "us-east-1"
access_key_id = "AKIACONF"
base_url = "{base}"
keys = [{{ name = "k", value = "aws-secret" }}]
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
    (url, mock)
}

async fn run_target(target: &str, model: &str, expect_of: fn(&Scenario) -> Expect) {
    let (url, _mock) = gateway().await;
    let client = reqwest::Client::new();
    let mut failures: Vec<String> = Vec::new();

    for scenario in scenarios() {
        let expected = expect_of(&scenario);
        // Sync run.
        let body = (scenario.body)(model);
        let res = client
            .post(format!("{url}/v1/chat/completions"))
            .json(&body)
            .send()
            .await
            .unwrap();
        match expected {
            Expect::Pass => {
                if res.status() != 200 {
                    let err = res.text().await.unwrap_or_default();
                    failures.push(format!(
                        "{target}/{}: expected 200, got: {err}",
                        scenario.name
                    ));
                    continue;
                }
                let value: Value = res.json().await.unwrap();
                let message = &value["choices"][0]["message"];
                if message["content"].is_null() && message["tool_calls"].is_null() {
                    failures.push(format!(
                        "{target}/{}: response has neither content nor tool_calls: {value}",
                        scenario.name
                    ));
                }
            }
            Expect::Reject(param) => {
                if res.status() != 400 {
                    failures.push(format!(
                        "{target}/{}: expected 400 for `{param}`, got {}",
                        scenario.name,
                        res.status()
                    ));
                    continue;
                }
                let value: Value = res.json().await.unwrap();
                if value["error"]["param"] != param {
                    failures.push(format!(
                        "{target}/{}: expected param `{param}`, got: {}",
                        scenario.name, value["error"]
                    ));
                }
            }
        }

        // Streaming run, where meaningful and passing.
        if scenario.streamable && expected == Expect::Pass {
            let mut body = (scenario.body)(model);
            body["stream"] = json!(true);
            let res = client
                .post(format!("{url}/v1/chat/completions"))
                .json(&body)
                .send()
                .await
                .unwrap();
            if res.status() != 200 {
                failures.push(format!(
                    "{target}/{} (stream): status {}",
                    scenario.name,
                    res.status()
                ));
                continue;
            }
            let text = res.text().await.unwrap();
            if !text.trim_end().ends_with("data: [DONE]") {
                failures.push(format!(
                    "{target}/{} (stream): missing [DONE]",
                    scenario.name
                ));
            }
            let chunks = text.lines().filter(|l| l.starts_with("data: {")).count();
            if chunks < 2 {
                failures.push(format!(
                    "{target}/{} (stream): only {chunks} chunks",
                    scenario.name
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} conformance failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[tokio::test]
async fn conformance_openai_target() {
    run_target("openai", "openai/gpt-4o", |s| s.openai).await;
}

#[tokio::test]
async fn conformance_anthropic_target() {
    run_target("anthropic", "anthropic/claude-x", |s| s.anthropic).await;
}

#[tokio::test]
async fn conformance_gemini_target() {
    run_target("gemini", "gemini/gemini-pro", |s| s.gemini).await;
}

/// Azure keeps the OpenAI dialect, so it inherits its expectations.
#[tokio::test]
async fn conformance_azure_target() {
    run_target("azure", "azure/gpt-4o", |s| s.openai).await;
}

/// Vertex serves the Gemini dialect, so it inherits its expectations.
#[tokio::test]
async fn conformance_vertex_target() {
    run_target("vertex", "vertex/gemini-pro", |s| s.gemini).await;
}

/// Bedrock tracks Anthropic's capability profile except remote image
/// URLs, which Converse cannot fetch.
#[tokio::test]
async fn conformance_bedrock_target() {
    run_target("bedrock", "bedrock/claude-x", |s| match s.name {
        "image_https_url" => Expect::Reject("messages"),
        _ => s.anthropic,
    })
    .await;
}

/// The same tool/text corpus through the Responses surface, translated.
#[tokio::test]
async fn conformance_responses_surface() {
    let (url, _mock) = gateway().await;
    let client = reqwest::Client::new();
    let cases: Vec<(&str, Value)> = vec![
        (
            "text",
            json!({"model": "anthropic/claude-x", "input": "hi"}),
        ),
        (
            "instructions",
            json!({"model": "anthropic/claude-x", "input": "hi", "instructions": "be terse"}),
        ),
        (
            "items_with_image",
            json!({"model": "anthropic/claude-x", "input": [
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "see"},
                {"type": "input_image", "image_url": "data:image/png;base64,aW1n"}]}]}),
        ),
        (
            "tools",
            json!({"model": "anthropic/claude-x", "input": "weather",
            "tools": [{"type": "function", "name": "get_weather", "parameters": {"type": "object"}}]}),
        ),
        (
            "tool_round_trip",
            json!({"model": "anthropic/claude-x", "input": [
            {"type": "function_call", "call_id": "c1", "name": "get_weather", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": "3C"}]}),
        ),
        (
            "json_schema",
            json!({"model": "anthropic/claude-x", "input": "structured",
            "text": {"format": {"type": "json_schema", "name": "out",
                "schema": {"type": "object", "properties": {"answer": {"type": "string"}}}}}}),
        ),
    ];
    for (name, body) in cases {
        let res = client
            .post(format!("{url}/v1/responses"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            200,
            "responses/{name} failed: {}",
            res.text().await.unwrap()
        );
    }
}

/// A curated subset through the Anthropic and Gemini inbound dialects.
#[tokio::test]
async fn conformance_inbound_dialects() {
    let (url, _mock) = gateway().await;
    let client = reqwest::Client::new();

    // Anthropic wire -> each target.
    for model in ["anthropic/claude-x", "openai/gpt-4o", "gemini/gemini-pro"] {
        for (name, body) in [
            (
                "text",
                json!({"model": model, "max_tokens": 64,
                "messages": [{"role": "user", "content": "hi"}]}),
            ),
            (
                "system_image",
                json!({"model": model, "max_tokens": 64, "system": "be terse",
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "see"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aW1n"}}]}]}),
            ),
            (
                "tools",
                json!({"model": model, "max_tokens": 64,
                "messages": [{"role": "user", "content": "weather"}],
                "tools": [{"name": "get_weather", "input_schema": {"type": "object"}}]}),
            ),
        ] {
            let res = client
                .post(format!("{url}/anthropic/v1/messages"))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                200,
                "anthropic-in/{model}/{name}: {}",
                res.text().await.unwrap()
            );
        }
    }

    // Gemini wire -> each target (aliases dodge `/` in the URL path).
    for model in ["gem-self", "gem-oai", "gem-ant"] {
        let res = client
            .post(format!("{url}/genai/v1beta/models/{model}:generateContent"))
            .json(&json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]}))
            .send()
            .await
            .unwrap();
        // These aliases are not in this gateway's config: expect 404 to
        // prove routing tried; full gemini-inbound coverage lives in the
        // dialect matrix suite.
        assert_eq!(res.status(), 404, "gemini-in/{model}");
    }
}
