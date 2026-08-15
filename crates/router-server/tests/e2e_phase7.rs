use std::sync::Arc;

use mock_provider::MockProvider;
use router_store::{BackendSpec, Command, Store};

/// Tests run against the file backend: real compare-and-swap semantics,
/// no AWS, nothing shared between tests.
async fn open_test_store(dir: &std::path::Path) -> Store {
    Store::open(
        &BackendSpec::File {
            path: dir.join("store.json"),
        },
        dir,
        "127.0.0.1:0",
    )
    .await
    .expect("the file backend opens")
}

async fn commit(store: &Store, expect: Option<u64>, command: Command) -> u64 {
    store
        .commit(expect, command)
        .await
        .expect("commit succeeds")
}
use router_core::config::{Config, Format};
use router_server::{AppState, build_router};
use serde_json::{Value, json};

struct Harness {
    url: String,
    _store: Arc<Store>,
    _dir: tempfile::TempDir,
}

async fn managed_gateway(mock: &MockProvider) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(open_test_store(dir.path()).await);
    let text = format!(
        r#"
[server]
require_auth = true

[console]
admin_keys = ["admin-test-key"]

[providers.openai]
base_url = "{}"
keys = [{{ name = "main", value = "sk-mock", models = ["gpt-4o", "gpt-4o-mini"] }}]
"#,
        mock.base_url()
    );
    commit(&store, None, Command::PutConfig { text: text.clone() }).await;
    let config = Config::from_str_with_env(&text, Format::Toml, &|_: &str| None).unwrap();
    let state = AppState::managed(config, store.clone(), dir.path().to_owned());
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        router_server::serve(listener, state, app, std::future::pending())
            .await
            .unwrap();
    });
    Harness {
        url,
        _store: store,
        _dir: dir,
    }
}

async fn admin_client(harness: &Harness) -> reqwest::Client {
    let client = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .unwrap();
    let response = client
        .post(format!("{}/admin/api/session", harness.url))
        .json(&json!({ "key": "admin-test-key" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.headers().contains_key("set-cookie"));
    client
}

#[tokio::test]
async fn console_demo_path_creates_key_serves_request_and_records_usage() {
    let mock = MockProvider::spawn().await;
    let gateway = managed_gateway(&mock).await;
    let admin = admin_client(&gateway).await;

    let created: Value = admin
        .post(format!("{}/admin/api/keys", gateway.url))
        .json(&json!({
            "name": "demo-app",
            "models": ["openai/gpt-4o"],
            "rate": { "rpm": 60, "tpm": 1000 },
            "budget": { "usd": 1.0, "period": "daily" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let key = created["key"].as_str().unwrap();
    assert!(key.starts_with("ck-"));

    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", gateway.url))
        .bearer_auth(key)
        .json(&json!({ "model": "openai/gpt-4o", "messages": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let _: Value = response.json().await.unwrap();

    let blocked = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", gateway.url))
        .bearer_auth(key)
        .json(&json!({ "model": "openai/gpt-4o-mini", "messages": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(blocked.status(), 403);

    let recent: Value = admin
        .get(format!("{}/admin/api/requests", gateway.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(recent["data"][0]["status"], 403);
    assert!(recent["data"].as_array().unwrap().iter().any(|row| {
        row["status"] == 200 && row["input_tokens"] == 7 && row["output_tokens"] == 3
    }));
}

#[tokio::test]
async fn config_writes_use_cas_and_file_mode_is_read_only() {
    let mock = MockProvider::spawn().await;
    let managed = managed_gateway(&mock).await;
    let admin = admin_client(&managed).await;
    let current: Value = admin
        .get(format!("{}/admin/api/config", managed.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let version = current["version"].as_u64().unwrap();
    let text = current["text"].as_str().unwrap();
    let first = admin
        .put(format!("{}/admin/api/config", managed.url))
        .json(&json!({ "version": version, "text": text }))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let stale = admin
        .put(format!("{}/admin/api/config", managed.url))
        .json(&json!({ "version": version, "text": text }))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(open_test_store(dir.path()).await);
    let config_text = format!(
        "[console]\nadmin_keys=[\"admin-test-key\"]\n[providers.openai]\nbase_url=\"{}\"\nkeys=[{{name=\"main\",value=\"x\"}}]\n",
        mock.base_url()
    );
    let config = Config::from_str(&config_text, Format::Toml).unwrap();
    let state = AppState::file_with_data_dir(config, store, dir.path().to_owned());
    let app = build_router(state);
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    let login = app
        .clone()
        .oneshot(
            Request::post("/admin/api/session")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"key":"admin-test-key"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let session: Value =
        serde_json::from_slice(&axum::body::to_bytes(login.into_body(), 4096).await.unwrap())
            .unwrap();
    let response = app
        .oneshot(
            Request::put("/admin/api/config")
                .header("content-type", "application/json")
                .header(
                    "authorization",
                    format!("Bearer {}", session["token"].as_str().unwrap()),
                )
                .body(Body::from(
                    json!({"version": 0, "text": config_text}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
}

#[tokio::test]
async fn embedded_console_serves_spa_and_security_headers() {
    let mock = MockProvider::spawn().await;
    let gateway = managed_gateway(&mock).await;
    for path in ["/console", "/console/keys"] {
        let response = reqwest::get(format!("{}{}", gateway.url, path))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(response.headers().contains_key("content-security-policy"));
        assert!(response.text().await.unwrap().contains("Caret Router"));
    }
}

#[tokio::test]
async fn virtual_key_rotation_has_overlap_then_revocation_is_immediate() {
    let mock = MockProvider::spawn().await;
    let gateway = managed_gateway(&mock).await;
    let admin = admin_client(&gateway).await;
    let created: Value = admin
        .post(format!("{}/admin/api/keys", gateway.url))
        .json(&json!({ "name": "rotate", "models": [] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let old = created["key"].as_str().unwrap().to_owned();
    let id = created["data"]["id"].as_str().unwrap();
    let rotated: Value = admin
        .post(format!("{}/admin/api/keys/{id}/rotate", gateway.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let new = rotated["key"].as_str().unwrap();
    for key in [&old, new] {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", gateway.url))
            .bearer_auth(key)
            .json(&json!({"model":"openai/gpt-4o","messages":[]}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }
    admin
        .put(format!("{}/admin/api/keys/{id}", gateway.url))
        .json(&json!({"enabled": false}))
        .send()
        .await
        .unwrap();
    let revoked = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", gateway.url))
        .bearer_auth(new)
        .json(&json!({"model":"openai/gpt-4o","messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), 401);
}

#[tokio::test]
async fn multipart_audio_upload_streams_through_without_gateway_collection() {
    use axum::body::Body;
    use axum::http::Request;
    use bytes::Bytes;
    use tower::ServiceExt;

    let mock = MockProvider::spawn().await;
    let config = Config::from_str(
        &format!(
            "[server]\nmax_body_size_mb=100\n[providers.openai]\nbase_url=\"{}/anything\"\nkeys=[{{name=\"main\",value=\"sk-mock\"}}]\n",
            mock.base_url()
        ),
        Format::Toml,
    )
    .unwrap();
    let app = build_router(AppState::new(config));
    let boundary = "caret-phase7-boundary";
    let prefix = Bytes::from(format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nopenai/whisper-1\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
    ));
    let payload = Bytes::from(vec![7u8; 12 * 1024 * 1024]);
    let suffix = Bytes::from(format!("\r\n--{boundary}--\r\n"));
    let stream = futures_util::stream::iter(
        [prefix, payload, suffix]
            .into_iter()
            .map(Ok::<_, std::convert::Infallible>),
    );
    let response = app
        .oneshot(
            Request::post("/v1/audio/transcriptions")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from_stream(stream))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let response_body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&response_body));
    let _: Value = serde_json::from_slice(&response_body).unwrap();
    let seen = mock.last_request();
    assert_eq!(seen.path, "POST /anything/audio/transcriptions");
    assert_eq!(seen.authorization.as_deref(), Some("Bearer sk-mock"));
}

/// A deployment must be able to move between config modes with nothing
/// more than a file copy: managed -> export -> file, and file -> import ->
/// managed, both producing the same working routing table.
#[tokio::test]
async fn config_mode_migration_round_trips_through_a_file() {
    let mock = MockProvider::spawn().await;
    let dir = tempfile::tempdir().unwrap();
    let original = format!(
        r#"
[providers.openai]
base_url = "{}"
keys = [{{ name = "main", value = "sk-mock", models = ["gpt-4o"] }}]

[aliases]
fast = "openai/gpt-4o"
"#,
        mock.base_url()
    );

    // Managed: the store owns the document.
    let store = Arc::new(open_test_store(dir.path()).await);
    commit(
        &store,
        None,
        Command::PutConfig {
            text: original.clone(),
        },
    )
    .await;
    let exported = store.read().0.config_text.expect("managed document");
    assert_eq!(
        exported, original,
        "export is byte-identical to what went in"
    );
    drop(store);

    // File: the exported text loads standalone and routes the alias.
    let path = dir.path().join("exported.toml");
    std::fs::write(&path, &exported).unwrap();
    let from_file = Config::load(&path).unwrap();
    assert!(from_file.aliases.contains_key("fast"));

    // Managed again: importing the same file reproduces the document, and
    // a second import is a new version rather than a conflict.
    let store = Arc::new(open_test_store(dir.path()).await);
    let reimported = std::fs::read_to_string(&path).unwrap();
    let version = commit(&store, None, Command::PutConfig { text: reimported }).await;
    assert!(version >= 2);
    assert_eq!(
        store.read().0.config_text.as_deref(),
        Some(exported.as_str())
    );

    // And the round-tripped document still serves traffic.
    let config = Config::from_str_with_env(&exported, Format::Toml, &|_: &str| None).unwrap();
    let state = AppState::managed(config, store.clone(), dir.path().to_owned());
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        router_server::serve(listener, state, app, std::future::pending())
            .await
            .unwrap();
    });
    let response = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .json(&json!({"model": "fast", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

/// Revoking a key must take effect on the data plane without a restart:
/// the store write propagates through the key-table swap immediately.
#[tokio::test]
async fn revocation_propagates_to_the_data_plane_without_restart() {
    let mock = MockProvider::spawn().await;
    let harness = managed_gateway(&mock).await;
    let admin = admin_client(&harness).await;

    let created: Value = admin
        .post(format!("{}/admin/api/keys", harness.url))
        .json(&json!({ "name": "revocable", "models": ["openai/gpt-4o"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = created["key"]
        .as_str()
        .expect("secret shown once")
        .to_owned();
    let id = created["data"]["id"].as_str().unwrap().to_owned();

    let call = async |token: &str| -> u16 {
        reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", harness.url))
            .bearer_auth(token)
            .json(
                &json!({"model": "openai/gpt-4o", "messages": [{"role": "user", "content": "hi"}]}),
            )
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    };

    assert_eq!(call(&token).await, 200, "a fresh key serves traffic");

    // Disable: the very next request is refused.
    let disabled = admin
        .put(format!("{}/admin/api/keys/{id}", harness.url))
        .json(&json!({ "enabled": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(disabled.status(), 200);
    assert_eq!(call(&token).await, 401, "disable is effective immediately");

    // Re-enable, then delete outright — also immediate.
    admin
        .put(format!("{}/admin/api/keys/{id}", harness.url))
        .json(&json!({ "enabled": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(call(&token).await, 200, "re-enable restores access");

    let deleted = admin
        .delete(format!("{}/admin/api/keys/{id}", harness.url))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 200);
    assert_eq!(
        call(&token).await,
        401,
        "revocation is effective immediately"
    );
}
