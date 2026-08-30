//! Session affinity end to end: one conversation keeps one account, so the
//! prefix it re-sends every turn is still cached where it was cached last
//! time.
//!
//! Like the services suite, every assertion reads the credential the mock
//! upstream actually received. Which account served is invisible from the
//! gateway's own side — that is finding G2 — so the upstream's view is the
//! only thing that proves this works.
//!
//! The pin is decided in `run_responses` from the caller's body, before any
//! provider dialect branch, so an OpenAI-compatible pool exercises the same
//! code path a Codex subscription pool takes.

use std::collections::BTreeSet;

use mock_provider::MockProvider;
use router_core::config::{Config, Format};
use router_core::vkey;
use router_server::{AppState, build_router};
use serde_json::json;

struct Gateway {
    url: String,
    mock: MockProvider,
}

/// Four accounts, all one service — the shape a routed agent run meets.
async fn gateway() -> Gateway {
    let mock = MockProvider::spawn().await;
    let text = format!(
        r#"
tenants = ["optimizer"]

[server]
require_auth = true

[providers.pool]
type = "openai_compat"
base_url = "{base}"
keys = [
  {{ name = "seat-1", value = "sk-1", tenant = "optimizer" }},
  {{ name = "seat-2", value = "sk-2", tenant = "optimizer" }},
  {{ name = "seat-3", value = "sk-3", tenant = "optimizer" }},
  {{ name = "seat-4", value = "sk-4", tenant = "optimizer" }},
]

[[virtual_keys]]
name = "optimizer"
id = "aaaaaa"
secret_hash = "{hash}"
tenant = "optimizer"
"#,
        base = mock.base_url(),
        hash = vkey::hash_secret("s3cret"),
    );
    let config = Config::from_str_with_env(&text, Format::Toml, &|_: &str| None)
        .expect("the config is valid");
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

/// One turn. `conversation` is the `prompt_cache_key` a CLI sends.
async fn turn(gw: &Gateway, conversation: Option<&str>) -> reqwest::Response {
    let mut body = json!({ "model": "pool/gpt-4o", "input": "hello" });
    if let Some(key) = conversation {
        body["prompt_cache_key"] = json!(key);
    }
    reqwest::Client::new()
        .post(format!("{}/v1/responses", gw.url))
        .bearer_auth("ck-aaaaaa-s3cret")
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// The credentials the upstream saw, deduplicated.
fn accounts_used(gw: &Gateway) -> BTreeSet<String> {
    gw.mock
        .requests()
        .into_iter()
        .filter_map(|r| r.authorization)
        .map(|auth| auth.trim_start_matches("Bearer ").to_owned())
        .collect()
}

/// The whole point: twenty turns of one conversation, one account.
#[tokio::test]
async fn a_conversation_keeps_one_account() {
    let gw = gateway().await;
    for _ in 0..20 {
        assert_eq!(turn(&gw, Some("conversation-42")).await.status(), 200);
    }
    let used = accounts_used(&gw);
    assert_eq!(
        used.len(),
        1,
        "every turn should land on the account holding its cached prefix, saw {used:?}"
    );
}

/// A caller that names no conversation is levelled across the pool exactly
/// as before. This is the guard on the derived-key trap: the proxy passes
/// a pin only when the *client* sent one, and the largest caller here
/// sends none.
#[tokio::test]
async fn turns_with_no_conversation_still_spread() {
    let gw = gateway().await;
    for _ in 0..20 {
        assert_eq!(turn(&gw, None).await.status(), 200);
    }
    assert_eq!(
        accounts_used(&gw).len(),
        4,
        "unpinned traffic still uses the whole pool"
    );
}

/// …and pinning must not turn one seat into the pool's hot spot.
#[tokio::test]
async fn separate_conversations_use_separate_accounts() {
    let gw = gateway().await;
    for n in 0..24 {
        assert_eq!(
            turn(&gw, Some(&format!("conversation-{n}"))).await.status(),
            200
        );
    }
    assert!(
        accounts_used(&gw).len() > 1,
        "24 conversations must not all land on one account"
    );
}

/// The pin reads the cache key; it must not consume it. Upstream still has
/// to receive it, because that is what makes the prefix cacheable at all.
#[tokio::test]
async fn the_cache_key_still_reaches_upstream() {
    let gw = gateway().await;
    assert_eq!(turn(&gw, Some("conversation-42")).await.status(), 200);
    let seen: Vec<_> = gw
        .mock
        .requests()
        .into_iter()
        .filter_map(|r| r.body["prompt_cache_key"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        seen,
        vec!["conversation-42".to_owned()],
        "the caller's cache key is forwarded, not swallowed"
    );
}
