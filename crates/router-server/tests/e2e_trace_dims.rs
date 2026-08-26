//! Caller-supplied log dimensions, end to end.
//!
//! Callers attribute their traffic by putting identifiers under
//! `metadata` in the request body — which workflow this call belongs to,
//! which chart, which agent, which pipeline stage. This asserts the
//! whole path: the gateway reads them off a real request, stores them on
//! the usage record, and `/requests` narrows on them.
//!
//! Two body shapes are exercised because two client libraries are in
//! use and they disagree about nesting; a gateway that only understands
//! one of them silently loses attribution for half the fleet.

use mock_provider::MockProvider;
use router_core::config::{Config, Format};
use router_server::{AppState, build_router};
use serde_json::{Value, json};

async fn gateway() -> (String, tempfile::TempDir, MockProvider) {
    let mock = MockProvider::spawn().await;
    let dir = tempfile::tempdir().unwrap();
    let config = Config::from_str_with_env(
        &format!(
            r#"
[providers.openai]
base_url = "{base}"
keys = [{{ name = "k", value = "sk-oai" }}]

[console]
admin_keys = ["probe-test-key"]
"#,
            base = mock.base_url(),
        ),
        Format::Toml,
        &|_: &str| None,
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

async fn admin_token(url: &str) -> String {
    reqwest::Client::new()
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
        .to_owned()
}

async fn chat(url: &str, metadata: Value) {
    let res = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .json(&json!({
            "model": "openai/gpt-4o-mini",
            "messages": [{"role": "user", "content": "code this chart"}],
            "metadata": metadata,
        }))
        .send()
        .await
        .unwrap();
    assert!(
        res.status().is_success(),
        "the gateway must serve a request that carries metadata: {}",
        res.status()
    );
    // The usage record is written when the response body ends, not when
    // its headers arrive — so the body has to be drained before the
    // record this test is about exists at all.
    res.bytes().await.unwrap();
    settle().await;
}

/// Let the gateway finish attributing the request it just served.
///
/// The record lands in the in-memory ring synchronously, but that
/// happens on the server task as the body closes, which is not ordered
/// against the client's read returning.
async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
}

/// Read `/requests`, optionally narrowed by `meta.*` terms.
async fn requests(url: &str, token: &str, query: &str) -> Vec<Value> {
    let res = reqwest::Client::new()
        .get(format!("{url}/admin/api/requests?limit=100&{query}"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success(), "requests read failed");
    res.json::<Value>().await.unwrap()["data"]
        .as_array()
        .expect("a data array")
        .clone()
}

/// The nested Langfuse shape, which is what LiteLLM-based callers send.
#[tokio::test]
async fn nested_metadata_becomes_filterable_dimensions() {
    let (url, _dir, _mock) = gateway().await;
    chat(
        &url,
        json!({
            "trace_name": "agentic_dag_coder",
            "trace_user_id": "org-uuid",
            "session_id": "org-uuid_chart-9",
            "tags": ["orgId:org-uuid", "agno", "temporal"],
            "trace_metadata": {
                "org_id": "org-uuid",
                "chart_id": "chart-9",
                "workflow_id": "WORKFLOW_HCC_CONFIRMED",
                "service": "agentic_dag_coder",
                "generation_name": "icd_coder",
                "event_processing_tag": "ICD_EXTRACTION",
                "agent": "icd_coder",
                "env": "prod",
            },
        }),
    )
    .await;

    let token = admin_token(&url).await;
    let rows = requests(&url, &token, "").await;
    let meta = &rows.first().expect("one record")["meta"];
    assert_eq!(meta["workflow_id"], "WORKFLOW_HCC_CONFIRMED");
    assert_eq!(meta["chart_id"], "chart-9");
    assert_eq!(meta["agent"], "icd_coder");
    // Canonicalised on the way in, so one filter spans both client shapes.
    assert_eq!(meta["stage"], "ICD_EXTRACTION");
    assert_eq!(meta["generation"], "icd_coder");
    // The redundant spellings are not stored a second time.
    assert!(meta.get("trace_user_id").is_none());
    assert!(meta.get("session_id").is_none());
    assert!(meta.get("tags").is_none());
}

/// The flat shape, which is what clients talking to the gateway directly
/// send. Same dimensions, no nesting.
#[tokio::test]
async fn flat_metadata_becomes_filterable_dimensions() {
    let (url, _dir, _mock) = gateway().await;
    chat(
        &url,
        json!({
            "service": "agentic_dag_coder",
            "agent": "mdm_adjudicator",
            "org_id": "org-1",
            "chart_id": "chart-1",
            "workflow_id": "WORKFLOW_RISE_SILVER",
        }),
    )
    .await;

    let token = admin_token(&url).await;
    let rows = requests(&url, &token, "").await;
    let meta = &rows.first().expect("one record")["meta"];
    assert_eq!(meta["workflow_id"], "WORKFLOW_RISE_SILVER");
    assert_eq!(meta["agent"], "mdm_adjudicator");
    assert_eq!(meta["service"], "agentic_dag_coder");
}

/// The point of the whole feature: narrowing a log to one workflow, one
/// stage, one agent — and the terms composing rather than replacing.
#[tokio::test]
async fn requests_narrow_on_meta_terms() {
    let (url, _dir, _mock) = gateway().await;
    for (workflow, stage) in [
        ("WORKFLOW_HCC_CONFIRMED", "ICD_EXTRACTION"),
        ("WORKFLOW_HCC_CONFIRMED", "CPT_SEARCH"),
        ("WORKFLOW_RISE_HCS", "ICD_EXTRACTION"),
    ] {
        chat(
            &url,
            json!({"workflow_id": workflow, "stage": stage, "chart_id": "chart-1"}),
        )
        .await;
    }
    let token = admin_token(&url).await;
    assert_eq!(requests(&url, &token, "").await.len(), 3, "unfiltered");

    let one_workflow = requests(&url, &token, "meta.workflow_id=WORKFLOW_HCC_CONFIRMED").await;
    assert_eq!(one_workflow.len(), 2, "narrowed to one workflow");

    // Two terms narrow further rather than widening.
    let one_stage = requests(
        &url,
        &token,
        "meta.workflow_id=WORKFLOW_HCC_CONFIRMED&meta.stage=ICD_EXTRACTION",
    )
    .await;
    assert_eq!(one_stage.len(), 1, "narrowed to one workflow and stage");
    assert_eq!(one_stage[0]["meta"]["stage"], "ICD_EXTRACTION");

    // A value nothing carries matches nothing — it is never ignored.
    assert!(
        requests(&url, &token, "meta.workflow_id=WORKFLOW_NOPE")
            .await
            .is_empty(),
        "an unmatched term must return nothing, not everything"
    );
    // And so does a dimension nothing carries.
    assert!(
        requests(&url, &token, "meta.nonexistent=x")
            .await
            .is_empty(),
        "an unknown dimension must return nothing, not everything"
    );
}

/// The summary is scanned with the same filter as the page, so the two
/// can never disagree about what is being counted.
#[tokio::test]
async fn the_summary_narrows_with_the_page() {
    let (url, _dir, _mock) = gateway().await;
    for workflow in [
        "WORKFLOW_HCC_CONFIRMED",
        "WORKFLOW_HCC_CONFIRMED",
        "WORKFLOW_RISE_HCS",
    ] {
        chat(&url, json!({"workflow_id": workflow})).await;
    }
    let token = admin_token(&url).await;
    let summary: Value = reqwest::Client::new()
        .get(format!(
            "{url}/admin/api/requests/summary?meta.workflow_id=WORKFLOW_HCC_CONFIRMED"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        summary["requests"], 2,
        "the summary counts the filtered set"
    );
}

/// The gateway *consumes* `metadata`; upstreams must never see it.
///
/// Not tidiness: the field name is taken on every provider and none of
/// them accept the shape callers send here. OpenAI allows at most
/// sixteen string pairs, Anthropic allows `user_id` alone — so a
/// forwarded `trace_metadata` is a 400, not a harmless extra key.
#[tokio::test]
async fn metadata_is_consumed_and_never_forwarded() {
    let (url, _dir, mock) = gateway().await;
    chat(
        &url,
        json!({
            "trace_metadata": {"workflow_id": "WORKFLOW_HCC_CONFIRMED", "chart_id": "chart-9"},
            "tags": ["agno"],
            "session_id": "s",
        }),
    )
    .await;

    let sent = mock.last_request();
    assert!(
        sent.body.get("metadata").is_none(),
        "metadata reached the provider: {}",
        sent.body
    );
    // Everything else the caller sent is still there, byte-faithful —
    // dropping one member must not disturb the rest of the request.
    assert_eq!(sent.body["messages"][0]["content"], "code this chart");
    assert!(sent.body.get("model").is_some());

    // And the gateway kept what it took.
    let token = admin_token(&url).await;
    let rows = requests(&url, &token, "").await;
    assert_eq!(
        rows.first().expect("one record")["meta"]["workflow_id"],
        "WORKFLOW_HCC_CONFIRMED"
    );
}

/// The same rule on the path that carries no `metadata` at all: the
/// request is forwarded unchanged.
#[tokio::test]
async fn a_body_without_metadata_is_forwarded_intact() {
    let (url, _dir, mock) = gateway().await;
    let res = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .json(&json!({
            "model": "openai/gpt-4o-mini",
            "messages": [{"role": "user", "content": "hello"}],
            "temperature": 0.5,
        }))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success());
    res.bytes().await.unwrap();

    let sent = mock.last_request();
    assert_eq!(sent.body["temperature"], 0.5);
    assert_eq!(sent.body["messages"][0]["content"], "hello");
}

/// A caller that sends no metadata at all is completely unaffected —
/// the record simply carries no dimensions.
#[tokio::test]
async fn a_request_without_metadata_still_records() {
    let (url, _dir, _mock) = gateway().await;
    let res = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .json(&json!({
            "model": "openai/gpt-4o-mini",
            "messages": [{"role": "user", "content": "hello"}],
        }))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success());
    res.bytes().await.unwrap();
    settle().await;

    let token = admin_token(&url).await;
    let rows = requests(&url, &token, "").await;
    let row = rows.first().expect("one record");
    assert_eq!(row["status"], 200);
    // Absent rather than an empty object: the field is skipped when empty.
    assert!(row.get("meta").is_none() || row["meta"].as_object().unwrap().is_empty());
}
