//! Services end to end: an account carries the name of the service it
//! belongs to, a key carries the name of the service it is, and a request
//! may only spend the accounts whose label matches.
//!
//! The assertions read the token the mock upstream actually received,
//! because that is the only thing that proves which account served — every
//! other signal is identical whichever one is picked.

use std::collections::BTreeSet;

use mock_provider::MockProvider;
use router_core::config::{Config, Format};
use router_core::vkey;
use router_server::{AppState, build_router};
use serde_json::{Value, json};

struct Gateway {
    url: String,
    mock: MockProvider,
}

/// Four accounts: two labelled `agi`, one `kris`, one left unassigned.
async fn gateway(virtual_keys: &str) -> Gateway {
    let mock = MockProvider::spawn().await;
    let text = format!(
        r#"
tenants = ["kris", "agi"]

[server]
require_auth = true

[providers.pool]
type = "openai_compat"
base_url = "{base}"
keys = [
  {{ name = "seat-1", value = "sk-1", tenant = "agi" }},
  {{ name = "seat-2", value = "sk-2", tenant = "agi" }},
  {{ name = "seat-3", value = "sk-3", tenant = "kris" }},
  {{ name = "seat-4", value = "sk-4" }},
]

# A subscription pool, where an out-of-quota answer really benches the seat.
[providers.subs]
type = "claude_subscription"
base_url = "{base}"
keys = [
  {{ name = "sub-agi",  value = "sk-ant-oat01-agi",  tenant = "agi"  }},
  {{ name = "sub-kris", value = "sk-ant-oat01-kris", tenant = "kris" }},
]

# No labels anywhere: this pool is shared, exactly as before services existed.
[providers.open]
type = "openai_compat"
base_url = "{base}"
keys = [
  {{ name = "free-1", value = "sk-f1" }},
  {{ name = "free-2", value = "sk-f2" }},
]

{virtual_keys}
"#,
        base = mock.base_url(),
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

fn key_entry(id: &str, secret: &str, extra: &str) -> String {
    format!(
        r#"
[[virtual_keys]]
name = "key-{id}"
id = "{id}"
secret_hash = "{hash}"
{extra}
"#,
        hash = vkey::hash_secret(secret),
    )
}

async fn chat(gw: &Gateway, token: &str, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", gw.url))
        .bearer_auth(token)
        .json(&json!({ "model": model, "messages": [] }))
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

/// A service spends the accounts labelled for it, and only those.
#[tokio::test]
async fn a_service_spends_only_its_own_accounts() {
    let gw = gateway(&key_entry("aaaaaa", "s3cret", r#"tenant = "agi""#)).await;
    for _ in 0..30 {
        assert_eq!(
            chat(&gw, "ck-aaaaaa-s3cret", "pool/gpt-4o").await.status(),
            200
        );
    }
    assert_eq!(
        accounts_used(&gw),
        BTreeSet::from(["sk-1".to_owned(), "sk-2".to_owned()]),
        "agi's two accounts, never kris's and never the unassigned one"
    );
}

/// Two services on one pool never overlap.
#[tokio::test]
async fn two_services_never_share_an_account() {
    let mut entries = key_entry("aaaaaa", "s3cret", r#"tenant = "agi""#);
    entries.push_str(&key_entry("bbbbbb", "s3cret", r#"tenant = "kris""#));
    let gw = gateway(&entries).await;

    for _ in 0..20 {
        assert_eq!(
            chat(&gw, "ck-aaaaaa-s3cret", "pool/gpt-4o").await.status(),
            200
        );
    }
    let agi = accounts_used(&gw);
    for _ in 0..20 {
        assert_eq!(
            chat(&gw, "ck-bbbbbb-s3cret", "pool/gpt-4o").await.status(),
            200
        );
    }
    let kris: BTreeSet<String> = accounts_used(&gw).difference(&agi).cloned().collect();
    assert_eq!(agi, BTreeSet::from(["sk-1".to_owned(), "sk-2".to_owned()]));
    assert_eq!(kris, BTreeSet::from(["sk-3".to_owned()]));
}

/// A key naming no service owns nothing in a labelled pool — and is told
/// so as a configuration problem, not a capacity one.
#[tokio::test]
async fn a_key_with_no_service_owns_nothing() {
    let gw = gateway(&key_entry("cccccc", "s3cret", "")).await;
    let refused = chat(&gw, "ck-cccccc-s3cret", "pool/gpt-4o").await;
    assert_eq!(refused.status(), 403);
    let body: Value = refused.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("owns no account"), "got `{message}`");
    assert!(accounts_used(&gw).is_empty(), "nothing reached upstream");
}

/// A pool nobody has labelled is shared, exactly as before.
#[tokio::test]
async fn an_unlabelled_pool_serves_everyone() {
    let gw = gateway(&key_entry("cccccc", "s3cret", "")).await;
    for _ in 0..20 {
        assert_eq!(
            chat(&gw, "ck-cccccc-s3cret", "open/gpt-4o").await.status(),
            200
        );
    }
    assert_eq!(accounts_used(&gw).len(), 2);
}

/// Labels are an admission rule, not an authentication one.
#[tokio::test]
async fn a_service_does_not_weaken_authentication() {
    let gw = gateway(&key_entry("aaaaaa", "s3cret", r#"tenant = "agi""#)).await;
    assert_eq!(
        chat(&gw, "ck-aaaaaa-wrong", "pool/gpt-4o").await.status(),
        401
    );
}

/// A service whose own accounts are all spent is told so — and does not
/// fall through onto another service's accounts.
///
/// `quota-claude` is the mock's scripted "seat out of quota": it answers
/// with the rate-limit headers the real backend sends, which is what
/// benches the seat for the window the provider reported.
#[tokio::test]
async fn a_service_out_of_quota_does_not_reach_another_service() {
    let gw = gateway(&key_entry("bbbbbb", "s3cret", r#"tenant = "kris""#)).await;

    // Kris's one seat answers "out of quota" and is benched for the window.
    assert_eq!(
        chat(&gw, "ck-bbbbbb-s3cret", "subs/quota-claude")
            .await
            .status(),
        429
    );

    let refused = chat(&gw, "ck-bbbbbb-s3cret", "subs/claude-sonnet-4-5").await;
    assert_eq!(refused.status(), 429);
    let body: Value = refused.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("all 1 of its accounts are out of quota"),
        "the count is what says whether to move one across; got `{message}`"
    );
    assert_eq!(
        accounts_used(&gw),
        BTreeSet::from(["sk-ant-oat01-kris".to_owned()]),
        "kris must never have touched agi's seat"
    );
}

/// The relay is not a way around the rule. It used to take the first
/// account in the list, bypassing health, load balancing and ownership
/// alike.
#[tokio::test]
async fn the_relay_honours_the_labels() {
    let gw = gateway(&key_entry("aaaaaa", "s3cret", r#"tenant = "agi""#)).await;
    for _ in 0..12 {
        let res = reqwest::Client::new()
            .post(format!("{}/passthrough/pool/anything/ping", gw.url))
            .bearer_auth("ck-aaaaaa-s3cret")
            .json(&json!({ "hello": "world" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }
    let used = accounts_used(&gw);
    assert!(
        used.iter().all(|a| a == "sk-1" || a == "sk-2"),
        "the relay must stay inside agi's accounts; saw {used:?}"
    );
    assert!(used.len() > 1, "and it should still spread across them");
}
