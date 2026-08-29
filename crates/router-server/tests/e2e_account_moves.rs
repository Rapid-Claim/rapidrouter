//! Moving an account between services — the management operation this
//! whole design exists to make cheap.
//!
//! Against a store-backed gateway, because the move is a control-plane
//! write: it edits one field of the config document and commits it, and the
//! node adopts the new routing table. The assertions read the credential
//! the mock upstream actually received, so "it moved" means traffic moved,
//! not that an API returned 200.

use std::collections::BTreeSet;
use std::sync::Arc;

use mock_provider::MockProvider;
use router_core::config::{Config, Format};
use router_core::vkey;
use router_server::{AppState, build_router};
use router_store::{BackendSpec, Command, Store};
use serde_json::{Value, json};

struct Gateway {
    url: String,
    mock: MockProvider,
    _store: Arc<Store>,
    _dir: tempfile::TempDir,
}

/// Two accounts: one labelled `agi`, one unassigned and waiting to be
/// given to somebody.
async fn gateway() -> Gateway {
    let mock = MockProvider::spawn().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        Store::open(
            &BackendSpec::File {
                path: dir.path().join("store.json"),
            },
            dir.path(),
            "127.0.0.1:0",
        )
        .await
        .expect("the file backend opens"),
    );

    let text = format!(
        r#"
tenants = ["kris", "agi"]

[server]
require_auth = true

[console]
admin_keys = ["admin-test-key"]

[providers.pool]
type = "openai_compat"
base_url = "{base}"
keys = [
  {{ name = "seat-1", value = "sk-1", tenant = "agi" }},
  {{ name = "seat-2", value = "sk-2" }},
]

[[virtual_keys]]
name = "kris-key"
id = "aaaaaa"
secret_hash = "{kris}"
tenant = "kris"
"#,
        base = mock.base_url(),
        kris = vkey::hash_secret("s3cret"),
    );
    store
        .commit(None, Command::PutConfig { text: text.clone() })
        .await
        .expect("seed the config");

    let config = Config::from_str_with_env(&text, Format::Toml, &|_: &str| None)
        .expect("the config is valid");
    let state = AppState::managed(config, store.clone(), dir.path().to_owned());
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        router_server::serve(listener, state, app, std::future::pending())
            .await
            .unwrap()
    });
    Gateway {
        url,
        mock,
        _store: store,
        _dir: dir,
    }
}

async fn admin(gw: &Gateway) -> reqwest::Client {
    let client = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .unwrap();
    let response = client
        .post(format!("{}/admin/api/session", gw.url))
        .json(&json!({ "key": "admin-test-key" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "admin session");
    client
}

async fn move_account(
    admin: &reqwest::Client,
    gw: &Gateway,
    account: &str,
    tenant: Value,
) -> reqwest::Response {
    admin
        .put(format!(
            "{}/admin/api/providers/pool/keys/{account}/tenant",
            gw.url
        ))
        .json(&json!({ "tenant": tenant }))
        .send()
        .await
        .unwrap()
}

async fn chat(gw: &Gateway, token: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", gw.url))
        .bearer_auth(token)
        .json(&json!({ "model": "pool/gpt-4o", "messages": [] }))
        .send()
        .await
        .unwrap()
}

fn accounts_used(gw: &Gateway) -> BTreeSet<String> {
    gw.mock
        .requests()
        .into_iter()
        .filter_map(|r| r.authorization)
        .map(|auth| auth.trim_start_matches("Bearer ").to_owned())
        .collect()
}

/// The whole management operation: one call, and traffic moves.
#[tokio::test]
async fn giving_an_account_to_a_service_moves_its_traffic() {
    let gw = gateway().await;
    let admin = admin(&gw).await;

    // Before: kris owns nothing here, and is told so.
    assert_eq!(chat(&gw, "ck-aaaaaa-s3cret").await.status(), 403);
    assert!(accounts_used(&gw).is_empty());

    assert_eq!(
        move_account(&admin, &gw, "seat-2", json!("kris"))
            .await
            .status(),
        200
    );

    // After: it serves, on exactly the account it was given.
    for _ in 0..5 {
        assert_eq!(chat(&gw, "ck-aaaaaa-s3cret").await.status(), 200);
    }
    assert_eq!(
        accounts_used(&gw),
        BTreeSet::from(["sk-2".to_owned()]),
        "kris got seat-2 and only seat-2 — agi's seat-1 is untouched"
    );
}

/// Taking it back is the same call with `null`.
#[tokio::test]
async fn unassigning_an_account_takes_it_out_of_service() {
    let gw = gateway().await;
    let admin = admin(&gw).await;

    assert_eq!(
        move_account(&admin, &gw, "seat-2", json!("kris"))
            .await
            .status(),
        200
    );
    assert_eq!(chat(&gw, "ck-aaaaaa-s3cret").await.status(), 200);

    assert_eq!(
        move_account(&admin, &gw, "seat-2", Value::Null)
            .await
            .status(),
        200
    );
    assert_eq!(
        chat(&gw, "ck-aaaaaa-s3cret").await.status(),
        403,
        "an unassigned account belongs to nobody again"
    );
}

/// A service nobody declared is refused, and nothing is changed.
#[tokio::test]
async fn moving_an_account_to_a_service_that_does_not_exist_is_refused() {
    let gw = gateway().await;
    let admin = admin(&gw).await;

    let refused = move_account(&admin, &gw, "seat-2", json!("ghost")).await;
    assert_eq!(refused.status(), 422);
    let body: Value = refused.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no service named `ghost`"),
        "got {body}"
    );

    // Unchanged: kris still owns nothing.
    assert_eq!(chat(&gw, "ck-aaaaaa-s3cret").await.status(), 403);
}

/// An account that does not exist is a 404, not a silent no-op.
#[tokio::test]
async fn moving_an_account_that_does_not_exist_is_a_404() {
    let gw = gateway().await;
    let admin = admin(&gw).await;
    assert_eq!(
        move_account(&admin, &gw, "no-such-seat", json!("kris"))
            .await
            .status(),
        404
    );
}

/// An account can be given its service at the moment it is added, so a new
/// account never has to spend a moment belonging to nobody.
#[tokio::test]
async fn an_account_can_be_added_straight_into_a_service() {
    let gw = gateway().await;
    let admin = admin(&gw).await;

    let added = admin
        .post(format!("{}/admin/api/providers/pool/keys", gw.url))
        .json(&json!({ "name": "seat-3", "value": "sk-3", "tenant": "kris" }))
        .send()
        .await
        .unwrap();
    assert_eq!(added.status(), 200);

    for _ in 0..5 {
        assert_eq!(chat(&gw, "ck-aaaaaa-s3cret").await.status(), 200);
    }
    assert_eq!(
        accounts_used(&gw),
        BTreeSet::from(["sk-3".to_owned()]),
        "the new account serves kris immediately, and nothing else does"
    );
}

/// Adding an account for a service nobody declared is refused, rather than
/// silently creating an account that serves nobody.
#[tokio::test]
async fn adding_an_account_for_a_ghost_service_is_refused() {
    let gw = gateway().await;
    let admin = admin(&gw).await;

    let refused = admin
        .post(format!("{}/admin/api/providers/pool/keys", gw.url))
        .json(&json!({ "name": "seat-3", "value": "sk-3", "tenant": "ghost" }))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 422);
    let body: Value = refused.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no service named `ghost`"),
        "got {body}"
    );
}

/// Deleting an account removes it from its service's pool, and the service
/// is then told it has nothing rather than quietly falling through.
#[tokio::test]
async fn deleting_an_account_takes_it_out_of_its_service() {
    let gw = gateway().await;
    let admin = admin(&gw).await;

    assert_eq!(
        move_account(&admin, &gw, "seat-2", json!("kris"))
            .await
            .status(),
        200
    );
    assert_eq!(chat(&gw, "ck-aaaaaa-s3cret").await.status(), 200);

    let removed = admin
        .delete(format!("{}/admin/api/providers/pool/keys/seat-2", gw.url))
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), 200);

    assert_eq!(
        chat(&gw, "ck-aaaaaa-s3cret").await.status(),
        403,
        "kris owns nothing again, and does not reach agi's seat-1"
    );
}

// ---------------------------------------------------------------------------
// Declaring and removing a service.
//
// The roster is the vocabulary every service control draws on, and until it
// could be edited here the console could show the whole operation and not let
// anyone begin it. Removal is the half that needs the care: a name deleted
// while anything still points at it strands whatever pointed, silently.
// ---------------------------------------------------------------------------

async fn add_tenant(admin: &reqwest::Client, gw: &Gateway, name: &str) -> reqwest::Response {
    admin
        .post(format!("{}/admin/api/tenants", gw.url))
        .json(&json!({ "name": name }))
        .send()
        .await
        .unwrap()
}

async fn remove_tenant(admin: &reqwest::Client, gw: &Gateway, name: &str) -> reqwest::Response {
    admin
        .delete(format!("{}/admin/api/tenants/{name}", gw.url))
        .send()
        .await
        .unwrap()
}

async fn roster(admin: &reqwest::Client, gw: &Gateway) -> BTreeSet<String> {
    let body: Value = admin
        .get(format!("{}/admin/api/providers", gw.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["tenants"]
        .as_array()
        .expect("the roster is a list")
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn a_service_can_be_declared_and_then_used() {
    let gw = gateway().await;
    let admin = admin(&gw).await;

    assert_eq!(add_tenant(&admin, &gw, "optimizer").await.status(), 200);
    assert!(roster(&admin, &gw).await.contains("optimizer"));

    // Declaring it is what makes it usable everywhere else — the point of
    // the roster is that a name has to exist before it can be worn.
    assert_eq!(
        move_account(&admin, &gw, "seat-2", json!("optimizer"))
            .await
            .status(),
        200,
    );
}

#[tokio::test]
async fn a_service_name_has_to_be_one_worth_reading() {
    let gw = gateway().await;
    let admin = admin(&gw).await;
    for bad in ["", "   ", "has space", "punct!", &"x".repeat(65)] {
        let response = add_tenant(&admin, &gw, bad).await;
        assert_eq!(
            response.status(),
            422,
            "`{bad}` should be refused: it is about to be copied onto accounts and into log lines",
        );
    }
    assert_eq!(
        add_tenant(&admin, &gw, "agi").await.status(),
        409,
        "duplicate"
    );
}

#[tokio::test]
async fn a_service_still_holding_accounts_cannot_be_deleted() {
    let gw = gateway().await;
    let admin = admin(&gw).await;

    // `agi` owns seat-1. Deleting it would leave that account owned by
    // nobody — reporting healthy and in quota while serving no one.
    let response = remove_tenant(&admin, &gw, "agi").await;
    assert_eq!(response.status(), 409);
    let body: Value = response.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("account"),
        "says what is in the way: {message}"
    );
    assert!(message.contains("pool/seat-1"), "names it: {message}");
    assert!(
        roster(&admin, &gw).await.contains("agi"),
        "and did not delete it"
    );
}

#[tokio::test]
async fn a_service_still_named_by_a_key_cannot_be_deleted() {
    let gw = gateway().await;
    let admin = admin(&gw).await;

    // `kris` owns no account, but a virtual key names it. Deleting it would
    // leave that key owning nothing, and every request it makes refused.
    let response = remove_tenant(&admin, &gw, "kris").await;
    assert_eq!(response.status(), 409);
    let message = response.json::<Value>().await.unwrap()["error"]["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(message.contains("kris-key"), "names the key: {message}");
}

#[tokio::test]
async fn a_service_nothing_points_at_can_be_deleted() {
    let gw = gateway().await;
    let admin = admin(&gw).await;

    assert_eq!(add_tenant(&admin, &gw, "temporary").await.status(), 200);
    assert_eq!(remove_tenant(&admin, &gw, "temporary").await.status(), 200);
    assert!(!roster(&admin, &gw).await.contains("temporary"));

    // And once it is gone it cannot be worn, which is the roster doing its
    // job: a typo becomes an error rather than an account owned by nobody.
    assert_eq!(
        move_account(&admin, &gw, "seat-2", json!("temporary"))
            .await
            .status(),
        422,
    );
}

#[tokio::test]
async fn deleting_a_service_that_does_not_exist_says_so() {
    let gw = gateway().await;
    let admin = admin(&gw).await;
    assert_eq!(
        remove_tenant(&admin, &gw, "never-existed").await.status(),
        404
    );
}

#[tokio::test]
async fn a_freed_service_can_be_deleted_after_its_accounts_move_away() {
    let gw = gateway().await;
    let admin = admin(&gw).await;

    assert_eq!(remove_tenant(&admin, &gw, "agi").await.status(), 409);
    // Unassign its only account, and the blocker is gone.
    assert_eq!(
        move_account(&admin, &gw, "seat-1", Value::Null)
            .await
            .status(),
        200,
    );
    assert_eq!(remove_tenant(&admin, &gw, "agi").await.status(), 200);
    assert!(!roster(&admin, &gw).await.contains("agi"));
}
