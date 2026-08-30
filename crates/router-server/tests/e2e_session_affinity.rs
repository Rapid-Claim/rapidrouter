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

/// Load: many conversations at once, all in flight together.
///
/// The distribution question this answers is the one an operator actually
/// asks — *can a hundred conversations share ten accounts* — and the
/// concurrency is the point as much as the count: selection mutates a
/// per-key atomic and a local candidate list on every request, so hammering
/// it from many tasks at once is what would expose a race in either.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_hundred_conversations_share_the_pool_without_losing_their_seats() {
    let gw = gateway().await;
    const CONVERSATIONS: usize = 100;
    const TURNS: usize = 4;

    let mut tasks = tokio::task::JoinSet::new();
    for c in 0..CONVERSATIONS {
        let url = gw.url.clone();
        tasks.spawn(async move {
            let client = reqwest::Client::new();
            let session = format!("conversation-{c}");
            let mut codes = Vec::new();
            for _ in 0..TURNS {
                let r = client
                    .post(format!("{url}/v1/responses"))
                    .bearer_auth("ck-aaaaaa-s3cret")
                    .json(&json!({
                        "model": "pool/gpt-4o",
                        "input": "hello",
                        "prompt_cache_key": session,
                    }))
                    .send()
                    .await
                    .unwrap();
                codes.push(r.status().as_u16());
            }
            codes
        });
    }

    let mut served = 0usize;
    while let Some(res) = tasks.join_next().await {
        for code in res.unwrap() {
            assert_eq!(code, 200, "every turn is served under load");
            served += 1;
        }
    }
    assert_eq!(served, CONVERSATIONS * TURNS);

    // Group the upstream's record by the cache key it saw, and check each
    // conversation stayed on one credential.
    let mut seats_per_conversation: std::collections::BTreeMap<String, BTreeSet<String>> =
        Default::default();
    for r in gw.mock.requests() {
        let (Some(key), Some(auth)) = (
            r.body["prompt_cache_key"].as_str().map(str::to_owned),
            r.authorization.clone(),
        ) else {
            continue;
        };
        seats_per_conversation
            .entry(key)
            .or_default()
            .insert(auth.trim_start_matches("Bearer ").to_owned());
    }
    assert_eq!(
        seats_per_conversation.len(),
        CONVERSATIONS,
        "every conversation reached upstream"
    );
    let split: Vec<_> = seats_per_conversation
        .iter()
        .filter(|(_, seats)| seats.len() > 1)
        .collect();
    assert!(
        split.is_empty(),
        "under load, each conversation must still keep one account: {split:?}"
    );

    // …and the pool as a whole is still shared out.
    let used: BTreeSet<&String> = seats_per_conversation.values().flatten().collect();
    assert_eq!(used.len(), 4, "all four accounts carry conversations");
}
