//! Lending an account's credential to a service that cannot be proxied.
//!
//! A vendor CLI signed in to a subscription talks to its own backend on its
//! own terms. For those callers the gateway stays where accounts are owned
//! and allocated, and hands the credential out instead of standing in front
//! of it — under the same ownership rule, and never with a live refresh
//! token.

use base64::Engine;
use mock_provider::MockProvider;
use router_core::config::{Config, Format};
use router_core::vkey;
use router_server::{AppState, build_router};
use serde_json::{Value, json};

struct Gateway {
    url: String,
    _dir: tempfile::TempDir,
}

fn codex_auth_json(refresh: &str) -> String {
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let jwt = |claims: Value| {
        format!(
            "{}.{}.{}",
            b64(br#"{"alg":"RS256"}"#),
            b64(claims.to_string().as_bytes()),
            b64(b"sig")
        )
    };
    json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "access_token": jwt(json!({"exp": 4_000_000_000u64})),
            "refresh_token": refresh,
            "id_token": jwt(json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": "acct-test"}
            })),
            "account_id": "acct-test"
        }
    })
    .to_string()
}

/// Two subscription seats, one per service, and three keys: one allowed to
/// hold a credential, one not, one belonging to no service at all.
async fn gateway() -> Gateway {
    let mock = MockProvider::spawn().await;
    let dir = tempfile::tempdir().unwrap();
    let kris = dir.path().join("kris.json");
    let opt = dir.path().join("opt.json");
    std::fs::write(&kris, codex_auth_json("rt-kris-live")).unwrap();
    std::fs::write(&opt, codex_auth_json("rt-opt-live")).unwrap();

    let text = format!(
        r#"
tenants = ["kris", "optimizer"]

[server]
require_auth = true

[providers.codex]
type = "codex_subscription"
base_url = "{base}"
keys = [
  {{ name = "seat-kris", value = "file:{kris}", tenant = "kris" }},
  {{ name = "seat-opt",  value = "file:{opt}",  tenant = "optimizer" }},
]

[[virtual_keys]]
name = "optimizer-runner"
id = "aaaaaa"
secret_hash = "{h}"
tenant = "optimizer"
lease_accounts = true

[[virtual_keys]]
name = "app-key"
id = "bbbbbb"
secret_hash = "{h}"
tenant = "optimizer"

[[virtual_keys]]
name = "no-service"
id = "cccccc"
secret_hash = "{h}"
lease_accounts = true
"#,
        base = mock.base_url(),
        kris = kris.display(),
        opt = opt.display(),
        h = vkey::hash_secret("s3cret"),
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
    Gateway { url, _dir: dir }
}

async fn lease(gw: &Gateway, id: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/accounts/lease", gw.url))
        .bearer_auth(format!("ck-{id}-s3cret"))
        .json(&json!({ "provider": "codex" }))
        .send()
        .await
        .unwrap()
}

/// The whole point: a service is handed an account labelled for it, ready to
/// write into a CLI's home directory.
#[tokio::test]
async fn a_service_is_lent_its_own_account() {
    let gw = gateway().await;
    let response = lease(&gw, "aaaaaa").await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();

    assert_eq!(
        body["account"], "seat-opt",
        "kris's seat is not the optimizer's to take"
    );
    assert_eq!(body["provider"], "codex");

    let auth: Value =
        serde_json::from_str(body["auth"].as_str().expect("auth is a document")).unwrap();
    assert!(
        auth["tokens"]["access_token"]
            .as_str()
            .unwrap_or_default()
            .len()
            > 20,
        "the usable half of the credential must survive"
    );
}

/// The invariant that makes lending safe at all. A borrower holding a live
/// refresh token could rotate it out from under the gateway and kill the
/// account for every other holder.
#[tokio::test]
async fn the_refresh_token_never_leaves_the_gateway() {
    let gw = gateway().await;
    let body: Value = lease(&gw, "aaaaaa").await.json().await.unwrap();
    let raw = body["auth"].as_str().unwrap();

    assert!(
        !raw.contains("rt-opt-live"),
        "the refresh token was lent out: {raw}"
    );
    let auth: Value = serde_json::from_str(raw).unwrap();
    assert_eq!(
        auth["tokens"]["refresh_token"], "",
        "the field should remain, blanked, so a consumer can prove it was defanged"
    );
}

/// Holding a credential is strictly more than spending it through us, so it
/// is its own opt-in — naming a service is not enough.
#[tokio::test]
async fn a_key_that_may_not_hold_credentials_is_refused() {
    let gw = gateway().await;
    let response = lease(&gw, "bbbbbb").await;
    assert_eq!(response.status(), 403);
    let body: Value = response.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("lease_accounts"),
        "the refusal should say which switch turns it on: {body}"
    );
}

/// And a key naming no service owns nothing to be lent.
#[tokio::test]
async fn a_key_with_no_service_is_lent_nothing() {
    let gw = gateway().await;
    let response = lease(&gw, "cccccc").await;
    assert_eq!(response.status(), 403);
    let body: Value = response.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("owns no account"),
        "got {body}"
    );
}

#[tokio::test]
async fn leasing_needs_a_key_and_a_known_provider() {
    let gw = gateway().await;
    let unknown = reqwest::Client::new()
        .post(format!("{}/v1/accounts/lease", gw.url))
        .bearer_auth("ck-aaaaaa-s3cret")
        .json(&json!({ "provider": "nope" }))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);

    let unauthenticated = reqwest::Client::new()
        .post(format!("{}/v1/accounts/lease", gw.url))
        .json(&json!({ "provider": "codex" }))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), 401);
}
