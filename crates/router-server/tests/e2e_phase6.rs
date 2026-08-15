//! Phase 6 breadth, end to end: Azure (deployment addressing), Bedrock
//! (SigV4 + Converse + event-stream), Vertex (project/location paths),
//! Databricks (OpenAI-compatible workspace), and the passthrough escape
//! hatch.

use futures_util::StreamExt;
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
[providers.azure]
endpoint = "{base}"
api_version = "2024-10-21"
keys = [{{ name = "k", value = "az-key" }}]
[providers.azure.deployments]
"gpt-4o" = "my-gpt4o-deploy"

[providers.bedrock]
region = "us-east-1"
access_key_id = "AKIAEXAMPLE"
base_url = "{base}"
keys = [{{ name = "k", value = "aws-secret-example" }}]

[providers.vertex]
project = "my-project"
location = "us-central1"
base_url = "{base}"
keys = [{{ name = "k", value = "ya29.token" }}]

[providers.databricks]
base_url = "{base}"
keys = [{{ name = "k", value = "dapi-token" }}]

[providers.openai]
base_url = "{base}"
keys = [{{ name = "k", value = "sk-oai" }}]
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

async fn chat(gw: &Gateway, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", gw.url))
        .json(&body)
        .send()
        .await
        .unwrap()
}

// ===========================================================================
// Azure
// ===========================================================================

#[tokio::test]
async fn azure_maps_deployment_and_api_key() {
    let gw = gateway().await;
    let res = chat(
        &gw,
        json!({"model": "azure/gpt-4o", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-caret-provider"], "azure");
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "mock response");

    let seen = gw.mock.last_request();
    assert!(
        seen.path
            .starts_with("/openai/deployments/my-gpt4o-deploy/chat/completions"),
        "wrong azure path: {}",
        seen.path
    );
    assert!(seen.path.contains("api-version=2024-10-21"));
    assert_eq!(seen.api_key.as_deref(), Some("az-key"));
    assert!(
        seen.authorization.is_none(),
        "azure must not get a bearer header"
    );
}

#[tokio::test]
async fn azure_unmapped_model_uses_name_as_deployment() {
    let gw = gateway().await;
    let res = chat(&gw, json!({"model": "azure/gpt-35", "messages": []})).await;
    assert_eq!(res.status(), 200);
    assert!(
        gw.mock
            .last_request()
            .path
            .starts_with("/openai/deployments/gpt-35/")
    );
}

#[tokio::test]
async fn azure_streams_like_openai() {
    let gw = gateway().await;
    let res = chat(
        &gw,
        json!({"model": "azure/gpt-4o", "messages": [], "stream": true}),
    )
    .await;
    assert_eq!(res.status(), 200);
    let text = res.text().await.unwrap();
    assert!(text.contains(r#""content":"mock ""#));
    assert!(text.trim_end().ends_with("data: [DONE]"));
}

// ===========================================================================
// Bedrock
// ===========================================================================

#[tokio::test]
async fn bedrock_sync_translates_and_signs() {
    let gw = gateway().await;
    let res = chat(
        &gw,
        json!({
            "model": "bedrock/anthropic.claude-3-haiku-20240307-v1:0",
            "messages": [{"role": "user", "content": "weather?"}],
            "tools": [{"type": "function", "function": {"name": "get_weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}],
            "max_tokens": 100,
        }),
    )
    .await;
    assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());
    let body: Value = res.json().await.unwrap();
    let calls = body["choices"][0]["message"]["tool_calls"]
        .as_array()
        .unwrap();
    assert_eq!(calls[0]["id"], "bdrk_1");
    assert_eq!(calls[0]["function"]["name"], "get_weather");
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(body["usage"]["total_tokens"], 16);

    let seen = gw.mock.last_request();
    assert!(seen.path.contains("converse"), "path: {}", seen.path);
    // The wire carries `%3A`; the mock's router records the decoded id.
    assert!(
        seen.path.contains("anthropic.claude-3-haiku-20240307-v1:0"),
        "path: {}",
        seen.path
    );
    let auth = seen.authorization.expect("SigV4 authorization header");
    assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIAEXAMPLE/"));
    assert!(auth.contains("/us-east-1/bedrock/aws4_request"));
    assert!(auth.contains("SignedHeaders=content-type;host;x-amz-date"));
    // Converse translation shape.
    assert_eq!(
        seen.body["toolConfig"]["tools"][0]["toolSpec"]["name"],
        "get_weather"
    );
    assert_eq!(seen.body["inferenceConfig"]["maxTokens"], 100);
}

#[tokio::test]
async fn bedrock_event_stream_translates_to_openai_chunks() {
    let gw = gateway().await;
    let res = chat(
        &gw,
        json!({
            "model": "bedrock/anthropic.claude-3-haiku-20240307-v1:0",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
        }),
    )
    .await;
    assert_eq!(res.status(), 200);
    let mut stream = res.bytes_stream();
    let mut reads = 0;
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        text.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        reads += 1;
    }
    assert!(reads >= 3, "event-stream frames collapsed: {reads} reads");
    assert!(text.contains(r#""content":"mock ""#));
    assert!(text.contains(r#""content":"stream""#));
    assert!(text.trim_end().ends_with("data: [DONE]"));

    let mut usage = None;
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        let chunk: Value = serde_json::from_str(data).unwrap();
        if !chunk["usage"].is_null() {
            usage = Some(chunk["usage"].clone());
        }
    }
    assert_eq!(
        usage.unwrap()["total_tokens"],
        16,
        "metadata frame usage must surface"
    );
}

#[tokio::test]
async fn bedrock_streamed_tool_args_reassemble() {
    let gw = gateway().await;
    let res = chat(
        &gw,
        json!({
            "model": "bedrock/anthropic.claude-3-haiku-20240307-v1:0",
            "messages": [{"role": "user", "content": "weather"}],
            "tools": [{"type": "function", "function": {"name": "get_weather",
                "parameters": {"type": "object"}}}],
            "stream": true,
        }),
    )
    .await;
    let text = res.text().await.unwrap();
    let mut args = String::new();
    let mut name = None;
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        let chunk: Value = serde_json::from_str(data).unwrap();
        for call in chunk["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(n) = call["function"]["name"].as_str() {
                name = Some(n.to_owned());
            }
            if let Some(a) = call["function"]["arguments"].as_str() {
                args.push_str(a);
            }
        }
    }
    assert_eq!(name.as_deref(), Some("get_weather"));
    assert_eq!(args, r#"{"city": "Paris"}"#);
}

// ===========================================================================
// Vertex
// ===========================================================================

#[tokio::test]
async fn vertex_routes_gemini_dialect_through_project_paths() {
    let gw = gateway().await;
    let res = chat(
        &gw,
        json!({
            "model": "vertex/gemini-2.0-flash",
            "messages": [{"role": "user", "content": "compare"}],
            "tools": [{"type": "function", "function": {"name": "get_weather",
                "parameters": {"type": "object"}}}],
        }),
    )
    .await;
    assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());
    assert_eq!(res.headers()["x-caret-provider"], "vertex");
    let body: Value = res.json().await.unwrap();
    let calls = body["choices"][0]["message"]["tool_calls"]
        .as_array()
        .unwrap();
    assert_eq!(calls.len(), 2, "gemini adapter reuse: parallel calls");

    let seen = gw.mock.last_request();
    assert_eq!(
        seen.path,
        "vertex/my-project/us-central1/gemini-2.0-flash:generateContent"
    );
    assert!(
        seen.authorization
            .as_deref()
            .unwrap()
            .starts_with("Bearer ya29.")
    );
    assert!(seen.api_key.is_none(), "vertex must not use x-goog-api-key");
    assert_eq!(
        seen.body["tools"][0]["functionDeclarations"][0]["name"],
        "get_weather"
    );
}

#[tokio::test]
async fn vertex_streaming() {
    let gw = gateway().await;
    let res = chat(
        &gw,
        json!({
            "model": "vertex/gemini-2.0-flash",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
        }),
    )
    .await;
    assert_eq!(res.status(), 200);
    let text = res.text().await.unwrap();
    assert!(text.contains(r#""content":"mock ""#));
    assert!(text.trim_end().ends_with("data: [DONE]"));
    // `alt=sse` travels as a query param, which the mock's path capture
    // excludes.
    assert!(
        gw.mock
            .last_request()
            .path
            .ends_with("streamGenerateContent"),
        "path: {}",
        gw.mock.last_request().path
    );
}

// ===========================================================================
// Databricks
// ===========================================================================

#[tokio::test]
async fn databricks_serves_as_openai_compatible_workspace() {
    let gw = gateway().await;
    let res = chat(
        &gw,
        json!({
            "model": "databricks/databricks-meta-llama-3-3-70b-instruct",
            "messages": [{"role": "user", "content": "hi"}],
        }),
    )
    .await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-caret-provider"], "databricks");
    let seen = gw.mock.last_request();
    assert_eq!(seen.path, "/chat/completions");
    assert_eq!(seen.body["model"], "databricks-meta-llama-3-3-70b-instruct");
    assert_eq!(seen.authorization.as_deref(), Some("Bearer dapi-token"));
}

#[tokio::test]
async fn databricks_discovery_from_env_pair() {
    let source = |var: &str| match var {
        "DATABRICKS_HOST" => Some("https://my-ws.cloud.databricks.com/".to_owned()),
        "DATABRICKS_TOKEN" => Some("dapi-abc".to_owned()),
        _ => None,
    };
    let config = Config::discover_from_env(&source).expect("databricks discoverable");
    let provider = &config.providers["databricks"];
    assert_eq!(
        provider.base_url.as_deref(),
        Some("https://my-ws.cloud.databricks.com/serving-endpoints")
    );
    assert!(provider.keys[0].secret.verify("dapi-abc"));
}

// ===========================================================================
// Passthrough
// ===========================================================================

#[tokio::test]
async fn passthrough_forwards_verbatim_with_auth() {
    let gw = gateway().await;
    let client = reqwest::Client::new();

    // POST with query, body, custom path.
    let res = client
        .post(format!(
            "{}/passthrough/openai/anything/beta/new-feature?limit=5&order=desc",
            gw.url
        ))
        .json(&json!({"anything": ["goes", 1, true]}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-caret-provider"], "openai");
    let seen = gw.mock.last_request();
    assert!(seen.path.contains("POST"));
    assert!(
        seen.path
            .contains("/anything/beta/new-feature?limit=5&order=desc")
    );
    assert_eq!(seen.body["anything"][0], "goes");
    assert_eq!(seen.authorization.as_deref(), Some("Bearer sk-oai"));

    // GET works too.
    let res = client
        .get(format!("{}/passthrough/openai/anything/models", gw.url))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert!(gw.mock.last_request().path.contains("GET"));
}

#[tokio::test]
async fn passthrough_injects_provider_native_auth() {
    let gw = gateway().await;
    let client = reqwest::Client::new();
    // Azure passthrough gets api-key, not bearer.
    let res = client
        .post(format!("{}/passthrough/azure/anything/fine-tunes", gw.url))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let seen = gw.mock.last_request();
    assert_eq!(seen.api_key.as_deref(), Some("az-key"));
}

#[tokio::test]
async fn passthrough_unknown_provider_404() {
    let gw = gateway().await;
    let res = reqwest::Client::new()
        .post(format!("{}/passthrough/nope/v1/thing", gw.url))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], "not_found");
}
