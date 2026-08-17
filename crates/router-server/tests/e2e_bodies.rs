//! Request bodies: captured, retrievable, and paged.
//!
//! The log drawer is only as good as what was stored, and what was
//! stored is invisible from the outside — so these assert the whole trip
//! rather than any one function: a request goes through the gateway, and
//! what the operator opens afterwards is what the caller actually sent
//! and what the provider actually returned.

use mock_provider::MockProvider;
use router_core::config::{Config, Format};
use router_server::{AppState, build_router};
use serde_json::{Value, json};

struct NoEnv;
impl router_core::config::EnvSource for NoEnv {
    fn get(&self, _name: &str) -> Option<String> {
        None
    }
}

async fn gateway(capture: &str) -> (String, tempfile::TempDir, MockProvider) {
    let mock = MockProvider::spawn().await;
    let dir = tempfile::tempdir().unwrap();
    let config = Config::from_str_with_env(
        &format!(
            r#"
[console]
admin_keys = ["test-admin"]

[usage]
capture_bodies = "{capture}"
flush_interval_secs = 1

[providers.openai]
base_url = "{base}"
keys = [{{ name = "main", value = "sk-test" }}]
"#,
            base = mock.base_url(),
        ),
        Format::Toml,
        &NoEnv,
    )
    .unwrap();

    let state = AppState::with_data_dir(config, dir.path().to_path_buf());
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        router_server::serve(listener, state, app, std::future::pending())
            .await
            .unwrap()
    });
    (url, dir, mock)
}

async fn chat(url: &str, content: &str) -> Value {
    let res = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .json(&json!({
            "model": "openai/gpt-4o",
            "messages": [{ "role": "user", "content": content }],
        }))
        .send()
        .await
        .unwrap();
    res.json().await.unwrap()
}

#[tokio::test]
async fn a_served_request_can_be_opened_afterwards() {
    let (url, _dir, _mock) = gateway("all").await;
    chat(&url, "a distinctive prompt about claim 12345").await;

    // The flusher writes on its interval; wait for the partition.
    let client = reqwest::Client::new();
    let mut record = None;
    let mut last_listing = Value::Null;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let listing: Value = client
            .get(format!("{url}/admin/api/requests?limit=5&since_ms=0"))
            .bearer_auth("test-admin")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(first) = listing["data"].as_array().and_then(|a| a.first()) {
            record = Some(first.clone());
            break;
        }
        last_listing = listing;
    }
    let record = record.unwrap_or_else(|| {
        panic!("the request must appear in the log; listing was: {last_listing}")
    });
    let id = record["request_id"].as_str().unwrap();
    let ts = record["ts"].as_u64().unwrap();

    // Bodies land on the flusher's beat, like the records.
    let mut bodies = Value::Null;
    for _ in 0..40 {
        bodies = client
            .get(format!("{url}/admin/api/requests/{id}/bodies?ts={ts}"))
            .bearer_auth("test-admin")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if bodies["input"].as_str().is_some_and(|s| !s.is_empty()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    let input = bodies["input"].as_str().unwrap_or_default();
    assert!(
        input.contains("a distinctive prompt about claim 12345"),
        "the stored input must be what the caller sent, got: {input}",
    );
    assert!(
        bodies["output"].as_str().is_some_and(|s| !s.is_empty()),
        "the provider's answer must be stored too, got: {bodies}",
    );
}

/// Capture off means nothing is written, and the console is told why
/// rather than shown an empty panel it cannot explain.
#[tokio::test]
async fn capture_off_stores_nothing_and_says_so() {
    let (url, _dir, _mock) = gateway("off").await;
    chat(&url, "this must not be stored").await;
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;

    let client = reqwest::Client::new();
    let listing: Value = client
        .get(format!("{url}/admin/api/requests?limit=5&since_ms=0"))
        .bearer_auth("test-admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let record = listing["data"].as_array().and_then(|a| a.first()).cloned();
    if let Some(record) = record {
        let id = record["request_id"].as_str().unwrap();
        let ts = record["ts"].as_u64().unwrap();
        let bodies: Value = client
            .get(format!("{url}/admin/api/requests/{id}/bodies?ts={ts}"))
            .bearer_auth("test-admin")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            bodies["input"].is_null(),
            "nothing may be stored when capture is off"
        );
        assert_eq!(
            bodies["reason"].as_str(),
            Some("body capture is off for this gateway"),
        );
    }
}

/// Pages must not overlap or skip, which is the whole reason for paging
/// by cursor rather than offset.
#[tokio::test]
async fn pages_do_not_repeat_or_skip_rows() {
    let (url, _dir, _mock) = gateway("off").await;
    for i in 0..25 {
        chat(&url, &format!("request {i}")).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;

    let client = reqwest::Client::new();
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let query = match &cursor {
            Some(after) => format!("limit=7&since_ms=0&after={after}"),
            None => "limit=7&since_ms=0".to_owned(),
        };
        let page: Value = client
            .get(format!("{url}/admin/api/requests?{query}"))
            .bearer_auth("test-admin")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        for row in page["data"].as_array().unwrap() {
            seen.push(row["request_id"].as_str().unwrap().to_owned());
        }
        match page["next"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }

    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        seen.len(),
        "a row must not appear on two pages"
    );
    assert!(
        seen.len() >= 25,
        "every request must be reachable by paging, saw {}",
        seen.len()
    );
}
