//! Credential bookkeeping on a provider: the account a seat really signs
//! in as, and removing a batch of seats in one commit.
//!
//! Both exist for the same operational problem. A pool is assembled from
//! credential *files*, and nothing stops two files holding credentials for
//! one upstream account — so a provider can carry fifteen keys wearing
//! fifteen names that are really one account's quota. The names differ,
//! the emails may differ in case or be missing, and the quota windows move
//! together in a way that reads as coincidence. `account_id` is the only
//! field that says so, and removing the duplicates once they are found has
//! to be a single commit rather than fifteen.

use std::sync::Arc;

use base64::Engine;
use router_core::config::{Config, Format};
use router_server::{AppState, build_router};
use router_store::{BackendSpec, Command, Store};
use serde_json::{Value, json};

/// A Codex `auth.json` for `account`, with an `exp` far enough out that
/// nothing tries to renew it mid-test.
///
/// `account_id` is deliberately left out of some of these documents: a
/// credential in the wild may carry it explicitly or only inside the
/// `id_token`, and a duplicate pair that is only visible through the JWT
/// is exactly the case an operator cannot see by reading the config.
fn codex_auth_json(account: &str, email: &str, explicit_account_id: bool) -> String {
    let encode = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
    let jwt = |claims: Value| {
        format!(
            "{}.{}.{}",
            encode(br#"{"alg":"RS256"}"#),
            encode(claims.to_string().as_bytes()),
            encode(b"sig")
        )
    };
    let mut tokens = json!({
        "access_token": jwt(json!({ "exp": 4_000_000_000u64 })),
        "refresh_token": "rt.1.AAAtest",
        "id_token": jwt(json!({
            "email": email,
            "https://api.openai.com/auth": { "chatgpt_account_id": account }
        })),
    });
    if explicit_account_id {
        tokens["account_id"] = json!(account);
    }
    json!({ "auth_mode": "chatgpt", "tokens": tokens }).to_string()
}

struct Harness {
    url: String,
    _store: Arc<Store>,
    _dir: tempfile::TempDir,
}

/// A managed gateway carrying four Codex seats over three accounts:
/// `seat-a` and `seat-dupe` are the same account under two file names,
/// and only the second of them spells `account_id` out.
async fn managed_gateway() -> Harness {
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

    let seats = [
        ("seat-a", "acct-one", "one@rapidclaims.ai", true),
        ("seat-dupe", "acct-one", "one@rapidclaims.ai", false),
        ("seat-b", "acct-two", "two@rapidclaims.ai", true),
        ("seat-c", "acct-three", "three@example.com", true),
    ];
    let mut entries = Vec::new();
    for (name, account, email, explicit) in seats {
        let path = dir.path().join(format!("{name}.json"));
        std::fs::write(&path, codex_auth_json(account, email, explicit)).unwrap();
        entries.push(format!(
            r#"{{ name = "{name}", value = "file:{}" }}"#,
            path.display()
        ));
    }

    let text = format!(
        r#"
[console]
admin_keys = ["admin-test-key"]

[providers.codex]
type = "codex_subscription"
keys = [{}]
"#,
        entries.join(", ")
    );
    store
        .commit(None, Command::PutConfig { text: text.clone() })
        .await
        .expect("commit succeeds");

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
    client
}

/// The seat names on `codex`, in payload order.
async fn seat_names(admin: &reqwest::Client, url: &str) -> Vec<String> {
    let body: Value = admin
        .get(format!("{url}/admin/api/providers"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["data"][0]["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn providers_report_the_account_a_seat_signs_in_as() {
    let gateway = managed_gateway().await;
    let admin = admin_client(&gateway).await;

    let body: Value = admin
        .get(format!("{}/admin/api/providers", gateway.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let by_name: std::collections::BTreeMap<&str, &Value> = body["data"][0]["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| (k["name"].as_str().unwrap(), &k["credential"]))
        .collect();
    assert_eq!(by_name.len(), 4);

    // The pair that matters: two names, one upstream account. The second
    // carries no `account_id` field at all, so this only holds because the
    // id_token's claim is decoded on the way in.
    assert_eq!(by_name["seat-a"]["account_id"], "acct-one");
    assert_eq!(by_name["seat-dupe"]["account_id"], "acct-one");
    assert_eq!(by_name["seat-b"]["account_id"], "acct-two");
    assert_eq!(by_name["seat-c"]["account_id"], "acct-three");

    // The email is what a person reads; it is not what identifies the
    // account, which is the whole reason `account_id` is reported too.
    assert_eq!(by_name["seat-a"]["email"], "one@rapidclaims.ai");
    assert_eq!(by_name["seat-dupe"]["email"], "one@rapidclaims.ai");
}

#[tokio::test]
async fn a_batch_of_seats_is_removed_in_one_commit() {
    let gateway = managed_gateway().await;
    let admin = admin_client(&gateway).await;

    let before: Value = admin
        .get(format!("{}/admin/api/config", gateway.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let version_before = before["version"].as_u64().unwrap();

    let response = admin
        .delete(format!("{}/admin/api/providers/codex/keys/bulk", gateway.url))
        .json(&json!({ "keys": ["seat-dupe", "seat-c"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let result: Value = response.json().await.unwrap();
    let mut removed: Vec<&str> = result["removed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    removed.sort_unstable();
    assert_eq!(removed, ["seat-c", "seat-dupe"]);
    assert!(result["missing"].as_array().unwrap().is_empty());

    assert_eq!(seat_names(&admin, &gateway.url).await, ["seat-a", "seat-b"]);

    // Two seats, one commit. The point of the endpoint: a per-key loop
    // would have moved the version twice and rebuilt the table twice.
    let after: Value = admin
        .get(format!("{}/admin/api/config", gateway.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after["version"].as_u64().unwrap(), version_before + 1);
}

#[tokio::test]
async fn names_that_are_already_gone_are_reported_not_refused() {
    let gateway = managed_gateway().await;
    let admin = admin_client(&gateway).await;

    // One live name, one that was never there: the live one still goes.
    let result: Value = admin
        .delete(format!("{}/admin/api/providers/codex/keys/bulk", gateway.url))
        .json(&json!({ "keys": ["seat-b", "seat-never"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["removed"], json!(["seat-b"]));
    assert_eq!(result["missing"], json!(["seat-never"]));
    assert_eq!(
        seat_names(&admin, &gateway.url).await,
        ["seat-a", "seat-dupe", "seat-c"]
    );

    // A batch matching nothing commits nothing, and says so rather than
    // erroring — the console's selection is a snapshot, and a colleague
    // having already removed those seats is the outcome that was wanted.
    let version: Value = admin
        .get(format!("{}/admin/api/config", gateway.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let result: Value = admin
        .delete(format!("{}/admin/api/providers/codex/keys/bulk", gateway.url))
        .json(&json!({ "keys": ["seat-never", "seat-also-never"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(result["removed"].as_array().unwrap().is_empty());
    assert_eq!(result["missing"], json!(["seat-never", "seat-also-never"]));

    let unchanged: Value = admin
        .get(format!("{}/admin/api/config", gateway.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(unchanged["version"], version["version"]);
    assert_eq!(
        seat_names(&admin, &gateway.url).await,
        ["seat-a", "seat-dupe", "seat-c"]
    );
}
