//! Kill-matrix: scripted provider-failure sequences through a live
//! gateway, asserting fallback order, breaker open/recovery timing, and
//! reload behavior under concurrent load.

use std::sync::Arc;

use mock_provider::MockProvider;
use router_core::config::{Config, Format};
use router_server::{AppState, build_router};
use serde_json::{Value, json};

struct Gateway {
    url: String,
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

async fn chat(gw: &Gateway, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", gw.url))
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn dead_primary_fails_over_to_fallback_provider() {
    let gw = gateway_with(|mock| {
        format!(
            r#"
[providers.internal]
type = "openai_compat"
base_url = "http://127.0.0.1:9"
keys = [{{ name = "k", value = "sk-dead" }}]

[providers.openai]
base_url = "{base}"
keys = [{{ name = "k", value = "sk-live" }}]

[fallbacks]
"internal/m" = ["openai/gpt-4o"]

[reliability.retries]
max_attempts = 1
"#,
            base = mock.base_url()
        )
    })
    .await;

    let res = chat(&gw, json!({"model": "internal/m", "messages": []})).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-rapid-provider"], "openai");
    assert_eq!(res.headers()["x-rapid-model"], "gpt-4o");
    let attempts: u32 = res.headers()["x-rapid-attempts"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(attempts, 2, "one dead attempt + one fallback attempt");
    // The fallback saw its own model name, not the primary's.
    assert_eq!(gw.mock.last_request().body["model"], "gpt-4o");
}

#[tokio::test]
async fn failing_status_advances_chain_and_serves_fallback() {
    let gw = gateway_with(|mock| {
        format!(
            r#"
[providers.openai]
base_url = "{base}"
keys = [{{ name = "k", value = "sk" }}]

[aliases]
primary = "openai/err-500"

[fallbacks]
primary = ["openai/gpt-4o"]

[reliability.retries]
max_attempts = 1
"#,
            base = mock.base_url()
        )
    })
    .await;

    let res = chat(&gw, json!({"model": "primary", "messages": []})).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-rapid-model"], "gpt-4o");
    let bodies: Vec<String> = gw
        .mock
        .requests()
        .iter()
        .map(|r| r.body["model"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        bodies,
        vec!["err-500", "gpt-4o"],
        "chain order must be primary then fallback"
    );
}

#[tokio::test]
async fn breaker_opens_and_stops_hammering_dead_target() {
    // Distinct providers: breakers are per (provider, key), so a chain
    // sharing one key would reset its own breaker on every fallback
    // success.
    let gw = gateway_with(|mock| {
        format!(
            r#"
[providers.internal]
type = "openai_compat"
base_url = "{base}"
keys = [{{ name = "k", value = "sk-int" }}]

[providers.openai]
base_url = "{base}"
keys = [{{ name = "k", value = "sk-oai" }}]

[aliases]
primary = "internal/err-500"

[fallbacks]
primary = ["openai/gpt-4o"]

[reliability.breaker]
failure_threshold = 3
window_secs = 30
cooldown_secs = 60

[reliability.retries]
max_attempts = 1
"#,
            base = mock.base_url()
        )
    })
    .await;

    // Three requests trip the primary's breaker (one 500 each).
    for _ in 0..3 {
        let res = chat(&gw, json!({"model": "primary", "messages": []})).await;
        assert_eq!(res.status(), 200); // fallback serves every time
    }
    let hits_while_closed = gw
        .mock
        .requests()
        .iter()
        .filter(|r| r.body["model"] == "err-500")
        .count();
    assert_eq!(hits_while_closed, 3);

    // Breaker now open: further requests must not touch the dead target.
    for _ in 0..5 {
        let res = chat(&gw, json!({"model": "primary", "messages": []})).await;
        assert_eq!(res.status(), 200);
        assert_eq!(
            res.headers()["x-rapid-attempts"],
            "1",
            "must skip the open target"
        );
    }
    let hits_after_open = gw
        .mock
        .requests()
        .iter()
        .filter(|r| r.body["model"] == "err-500")
        .count();
    assert_eq!(hits_after_open, 3, "open breaker must stop upstream hits");
}

#[tokio::test]
async fn breaker_probe_recovers_a_healed_target() {
    let gw = gateway_with(|mock| {
        format!(
            r#"
[providers.internal]
type = "openai_compat"
base_url = "{base}"
keys = [{{ name = "k", value = "sk-int" }}]

[providers.openai]
base_url = "{base}"
keys = [{{ name = "k", value = "sk-oai" }}]

[aliases]
primary = "internal/recover-after-2"

[fallbacks]
primary = ["openai/gpt-4o"]

[reliability.breaker]
failure_threshold = 2
window_secs = 30
cooldown_secs = 1

[reliability.retries]
max_attempts = 1
"#,
            base = mock.base_url()
        )
    })
    .await;

    // Two failures trip the breaker; both serve from fallback.
    for _ in 0..2 {
        let res = chat(&gw, json!({"model": "primary", "messages": []})).await;
        assert_eq!(res.status(), 200);
    }
    // Open: served by fallback without touching primary.
    let res = chat(&gw, json!({"model": "primary", "messages": []})).await;
    assert_eq!(res.headers()["x-rapid-attempts"], "1");

    // After cooldown the probe goes through; the target has healed
    // (recover-after-2 succeeds from the 3rd hit), so service returns.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let res = chat(&gw, json!({"model": "primary", "messages": []})).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-rapid-model"], "recover-after-2");

    // Closed again: straight to primary.
    let res = chat(&gw, json!({"model": "primary", "messages": []})).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-rapid-model"], "recover-after-2");
    assert_eq!(res.headers()["x-rapid-attempts"], "1");
}

#[tokio::test]
async fn key_level_failover_within_provider() {
    // Key `bad` points at a dead port via... a second provider is needed
    // for that; instead: bad key trips on err-500 only when used. Here we
    // verify that when one key's breaker opens, the other key serves.
    let gw = gateway_with(|mock| {
        format!(
            r#"
[providers.openai]
base_url = "{base}"
keys = [
  {{ name = "a", value = "sk-a", weight = 1.0 }},
  {{ name = "b", value = "sk-b", weight = 1.0 }},
]

[reliability.breaker]
failure_threshold = 1
window_secs = 30
cooldown_secs = 60

[reliability.retries]
max_attempts = 2
"#,
            base = mock.base_url()
        )
    })
    .await;

    // One 500 opens whichever key served it; the retry must come from
    // the other key.
    let res = chat(&gw, json!({"model": "openai/err-500", "messages": []})).await;
    assert_eq!(res.status(), 500); // both keys burned on err-500 (last candidate serves)
    let auths: Vec<String> = gw
        .mock
        .requests()
        .iter()
        .filter_map(|r| r.authorization.clone())
        .collect();
    assert_eq!(auths.len(), 2);
    assert_ne!(auths[0], auths[1], "retry must rotate to the other key");

    // Both breakers now open; a normal request has nothing healthy.
    let res = chat(&gw, json!({"model": "openai/gpt-4o", "messages": []})).await;
    assert_eq!(res.status(), 503);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], "no_capacity");
}

#[tokio::test]
async fn reload_under_concurrent_load_never_drops() {
    let gw = gateway_with(|mock| {
        format!(
            r#"
[providers.openai]
base_url = "{base}"
keys = [{{ name = "k", value = "sk-1" }}]
"#,
            base = mock.base_url()
        )
    })
    .await;
    let gw = Arc::new(gw);

    let mut workers = Vec::new();
    for _ in 0..8 {
        let gw = gw.clone();
        workers.push(tokio::spawn(async move {
            for _ in 0..25 {
                let res = chat(
                    &gw,
                    json!({"model": "openai/gpt-4o", "messages": [], "stream": true}),
                )
                .await;
                assert_eq!(res.status(), 200);
                let text = res.text().await.unwrap();
                assert!(text.contains("[DONE]"), "stream truncated during reload");
            }
        }));
    }

    // Hammer config swaps while the load runs.
    for i in 0..50 {
        let config = Config::from_str_with_env(
            &format!(
                r#"
[providers.openai]
base_url = "{base}"
keys = [{{ name = "k{i}", value = "sk-{i}" }}]
"#,
                base = gw.mock.base_url(),
            ),
            Format::Toml,
            &|_: &str| None,
        )
        .unwrap();
        gw.state.apply_config(config);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    for worker in workers {
        worker.await.unwrap();
    }
}

#[tokio::test]
async fn routing_group_splits_primary_traffic_by_weight() {
    let gw = gateway_with(|mock| {
        format!(
            r#"
[providers.openai]
base_url = "{base}"
keys = [{{ name = "k", value = "sk" }}]

[groups.fast]
primary = [
  {{ target = "openai/heavy", weight = 9 }},
  {{ target = "openai/light", weight = 1 }},
]
"#,
            base = mock.base_url()
        )
    })
    .await;

    for _ in 0..200 {
        let res = chat(&gw, json!({"model": "fast", "messages": []})).await;
        assert_eq!(res.status(), 200);
    }
    let heavy = gw
        .mock
        .requests()
        .iter()
        .filter(|r| r.body["model"] == "heavy")
        .count();
    // 9:1 over 200 draws. A wide band — this is asserting the split
    // exists and points the right way, not the RNG's exact quality.
    assert!(
        (140..200).contains(&heavy),
        "expected ~180 of 200 on the weight-9 model, got {heavy}"
    );
}

#[tokio::test]
async fn routing_group_exhausts_primary_pool_before_fallback() {
    // Two providers over one mock, told apart by the credential each
    // presents: both primaries fail, so the reserve must serve, and it
    // must not be reached until neither primary has anything left.
    let gw = gateway_with(|mock| {
        format!(
            r#"
[providers.openai]
base_url = "{base}"
keys = [{{ name = "k", value = "sk-one" }}]

[providers.alt]
type = "openai_compat"
base_url = "{base}"
keys = [{{ name = "k", value = "sk-two" }}]

[groups.fast]
primary = [
  {{ target = "openai/err-500", weight = 5 }},
  {{ target = "alt/err-500", weight = 5 }},
]
fallback = [{{ target = "openai/gpt-4o" }}]

[reliability.retries]
max_attempts = 1
"#,
            base = mock.base_url()
        )
    })
    .await;

    let res = chat(&gw, json!({"model": "fast", "messages": []})).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-rapid-model"], "gpt-4o");

    let requests = gw.mock.requests();
    let models: Vec<&str> = requests
        .iter()
        .map(|r| r.body["model"].as_str().unwrap())
        .collect();
    assert_eq!(
        models,
        vec!["err-500", "err-500", "gpt-4o"],
        "both primaries, then the reserve"
    );
    let mut credentials: Vec<String> = requests[..2]
        .iter()
        .map(|r| r.authorization.clone().unwrap())
        .collect();
    credentials.sort();
    assert_eq!(
        credentials,
        vec!["Bearer sk-one", "Bearer sk-two"],
        "each primary was tried once, not one of them twice"
    );
}
